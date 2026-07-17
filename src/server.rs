use std::fs;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{fcntl, open, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::pty::{self, OpenptyResult, Winsize as NixWinsize};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, dup2, execvp, fork, setsid, ForkResult, Pid};
use nix::unistd::{read as nix_read, write as nix_write};

use crate::protocol::{self, Message, Winsize};
use crate::session::{
    self, cleanup_session_files, now_unix, set_attached, write_meta, SessionMeta, SessionPaths,
};

pub struct NewSessionOpts {
    pub name: String,
    pub shell: String,
    pub base: std::path::PathBuf,
}

/// Create a new detached session daemon and return once it is listening.
pub fn create_session(opts: NewSessionOpts) -> Result<()> {
    session::validate_session_name(&opts.name)?;
    session::ensure_base_dir(&opts.base)?;

    let paths = SessionPaths::for_name(&opts.base, &opts.name);
    if paths.meta.exists() {
        if let Ok(meta) = session::read_meta(&paths) {
            if session::process_alive(meta.pid) {
                bail!("session '{}' already exists", opts.name);
            }
        }
        cleanup_session_files(&paths)?;
    }

    fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create session dir {}", paths.dir.display()))?;

    // Readiness pipe: child writes one byte when socket is ready.
    let (read_fd, write_fd) = nix::unistd::pipe().context("create readiness pipe")?;

    match unsafe { fork() }.context("fork session daemon")? {
        ForkResult::Parent { child: _ } => {
            // Close write end in parent; keep ownership of read_fd.
            let read_raw = read_fd.as_raw_fd();
            drop(write_fd);
            wait_for_ready(read_raw, &paths)?;
            drop(read_fd);
            Ok(())
        }
        ForkResult::Child => {
            drop(read_fd);
            // Detach from controlling terminal / parent session.
            let _ = setsid();
            // Ignore SIGHUP so SSH disconnect of creator does not kill us.
            unsafe {
                let _ = signal::signal(Signal::SIGHUP, SigHandler::SigIgn);
                let _ = signal::signal(Signal::SIGINT, SigHandler::SigIgn);
                let _ = signal::signal(Signal::SIGPIPE, SigHandler::SigIgn);
            }

            reopen_stdio_null()?;

            let result = run_daemon(opts, paths, write_fd);
            if let Err(e) = &result {
                let _ = fs::write("/tmp/reshell-daemon-error.log", format!("{e:?}\n"));
            }
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
    }
}

fn wait_for_ready(read_fd: RawFd, paths: &SessionPaths) -> Result<()> {
    let mut buf = [0u8; 1];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match nix_read(read_fd, &mut buf) {
            Ok(0) => bail!("session daemon exited before becoming ready"),
            Ok(_) => {
                if !paths.socket.exists() {
                    bail!("session daemon signaled ready but socket is missing");
                }
                return Ok(());
            }
            Err(Errno::EINTR) => {
                if std::time::Instant::now() > deadline {
                    bail!("timed out waiting for session daemon");
                }
            }
            Err(e) => return Err(e).context("read readiness pipe"),
        }
        if std::time::Instant::now() > deadline {
            bail!("timed out waiting for session daemon");
        }
    }
}

fn reopen_stdio_null() -> Result<()> {
    let null_fd = open("/dev/null", OFlag::O_RDWR, Mode::empty()).context("open /dev/null")?;
    dup2(null_fd, 0).context("dup2 stdin")?;
    dup2(null_fd, 1).context("dup2 stdout")?;
    dup2(null_fd, 2).context("dup2 stderr")?;
    if null_fd > 2 {
        let _ = close(null_fd);
    }
    Ok(())
}

fn run_daemon(opts: NewSessionOpts, paths: SessionPaths, ready_fd: OwnedFd) -> Result<()> {
    let OpenptyResult { master, slave } = pty::openpty(None, None).context("openpty")?;

    let shell_path = opts.shell.clone();
    match unsafe { fork() }.context("fork shell")? {
        ForkResult::Parent { child: shell_pid } => {
            drop(slave);
            set_cloexec(master.as_raw_fd())?;
            set_nonblocking(master.as_raw_fd())?;

            let _ = fs::remove_file(&paths.socket);
            let listener = UnixListener::bind(&paths.socket)
                .with_context(|| format!("bind {}", paths.socket.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600));
            }
            set_nonblocking(listener.as_raw_fd())?;

            let meta = SessionMeta {
                name: opts.name.clone(),
                pid: Pid::this().as_raw(),
                shell: shell_path,
                created_unix: now_unix(),
                attached: false,
            };
            write_meta(&paths, &meta)?;

            let _ = nix_write(ready_fd.as_fd(), &[1u8]);
            drop(ready_fd);

            server_loop(master, listener, shell_pid, &paths)?;
            cleanup_session_files(&paths)?;
            Ok(())
        }
        ForkResult::Child => {
            drop(master);
            drop(ready_fd);
            let slave_fd = slave.as_raw_fd();
            let _ = setsid();

            // Make slave the controlling terminal (Linux).
            let ret = unsafe { nix::libc::ioctl(slave_fd, nix::libc::TIOCSCTTY as _, 0) };
            if ret < 0 {
                // Non-fatal on some environments; shell may still work.
            }

            dup2(slave_fd, 0).context("dup2 slave stdin")?;
            dup2(slave_fd, 1).context("dup2 slave stdout")?;
            dup2(slave_fd, 2).context("dup2 slave stderr")?;
            if slave_fd > 2 {
                let _ = close(slave_fd);
            }
            std::mem::forget(slave);

            unsafe {
                let _ = signal::signal(Signal::SIGHUP, SigHandler::SigDfl);
                let _ = signal::signal(Signal::SIGINT, SigHandler::SigDfl);
            }

            if std::env::var_os("TERM").is_none() {
                std::env::set_var("TERM", "xterm-256color");
            }

            let shell =
                std::ffi::CString::new(opts.shell.as_str()).context("shell path contains NUL")?;
            let argv0 = std::ffi::CString::new(
                Path::new(&opts.shell)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("sh"),
            )
            .unwrap();
            execvp(&shell, &[&argv0]).context("exec shell")?;
            unreachable!();
        }
    }
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("F_GETFL")?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).context("F_SETFL O_NONBLOCK")?;
    Ok(())
}

fn set_blocking(fd: RawFd) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("F_GETFL")?;
    let flags = OFlag::from_bits_truncate(flags) - OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).context("F_SETFL blocking")?;
    Ok(())
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFD).context("F_GETFD")?;
    fcntl(
        fd,
        FcntlArg::F_SETFD(
            nix::fcntl::FdFlag::from_bits_truncate(flags) | nix::fcntl::FdFlag::FD_CLOEXEC,
        ),
    )
    .context("F_SETFD CLOEXEC")?;
    Ok(())
}

fn apply_winsize(master: RawFd, ws: Winsize) -> Result<()> {
    let nix_ws = NixWinsize {
        ws_row: ws.rows,
        ws_col: ws.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { nix::libc::ioctl(master, nix::libc::TIOCSWINSZ as _, &nix_ws) };
    Errno::result(ret).context("TIOCSWINSZ")?;
    Ok(())
}

fn server_loop(
    master: OwnedFd,
    listener: UnixListener,
    shell_pid: Pid,
    paths: &SessionPaths,
) -> Result<()> {
    let master_fd = master.as_raw_fd();
    let mut client: Option<UnixStream> = None;
    let mut buf = [0u8; 8192];

    loop {
        match waitpid(shell_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                if client.take().is_some() {
                    let _ = set_attached(paths, false);
                }
                break;
            }
            Ok(_) => {}
            Err(Errno::ECHILD) => break,
            Err(e) => return Err(e).context("waitpid"),
        }

        let mut fds = Vec::with_capacity(3);
        fds.push(PollFd::new(master.as_fd(), PollFlags::POLLIN));
        fds.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
        if let Some(ref c) = client {
            fds.push(PollFd::new(c.as_fd(), PollFlags::POLLIN));
        }

        match poll(&mut fds, 500u16) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll"),
        }

        let master_revents = fds[0].revents().unwrap_or(PollFlags::empty());
        let listen_revents = fds[1].revents().unwrap_or(PollFlags::empty());
        let client_revents = if client.is_some() {
            fds[2].revents().unwrap_or(PollFlags::empty())
        } else {
            PollFlags::empty()
        };

        if listen_revents.contains(PollFlags::POLLIN) {
            match listener.accept() {
                Ok((stream, _)) => {
                    set_nonblocking(stream.as_raw_fd())?;
                    if client.is_some() {
                        drop(stream);
                    } else {
                        client = Some(stream);
                        let _ = set_attached(paths, true);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("accept"),
            }
        }

        if master_revents.contains(PollFlags::POLLIN) {
            match nix_read(master_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(ref mut c) = client {
                        if protocol::write_message(&mut *c, &Message::Data(buf[..n].to_vec()))
                            .is_err()
                            || c.flush().is_err()
                        {
                            drop_client(&mut client, paths);
                        }
                    }
                }
                Err(Errno::EAGAIN) => {}
                Err(Errno::EIO) => break,
                Err(e) => return Err(e).context("read pty master"),
            }
        }

        if master_revents.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
        {
            break;
        }

        if client.is_some()
            && client_revents.intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP)
        {
            if client_revents.contains(PollFlags::POLLIN) {
                let disconnect = match handle_client_readable(client.as_mut().unwrap(), master_fd) {
                    Ok(false) => false,
                    Ok(true) => true,
                    Err(_) => true,
                };
                if disconnect {
                    drop_client(&mut client, paths);
                }
            } else {
                drop_client(&mut client, paths);
            }
        }
    }

    let _ = signal::kill(shell_pid, Signal::SIGHUP);
    let _ = waitpid(shell_pid, None);
    Ok(())
}

fn drop_client(client: &mut Option<UnixStream>, paths: &SessionPaths) {
    *client = None;
    let _ = set_attached(paths, false);
}

/// Returns Ok(true) if client should be disconnected (detach / EOF / error).
fn handle_client_readable(client: &mut UnixStream, master_fd: RawFd) -> Result<bool> {
    set_blocking(client.as_raw_fd())?;
    let msg = match protocol::read_message(&mut *client) {
        Ok(Some(m)) => m,
        Ok(None) => {
            let _ = set_nonblocking(client.as_raw_fd());
            return Ok(true);
        }
        Err(_) => {
            let _ = set_nonblocking(client.as_raw_fd());
            return Ok(true);
        }
    };
    set_nonblocking(client.as_raw_fd())?;

    match msg {
        Message::Attach(ws) | Message::Resize(ws) => {
            apply_winsize(master_fd, ws)?;
            Ok(false)
        }
        Message::Data(data) => {
            write_all_fd(master_fd, &data)?;
            Ok(false)
        }
        Message::Detach => Ok(true),
    }
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        match nix_write(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, data) {
            Ok(0) => bail!("short write to pty"),
            Ok(n) => data = &data[n..],
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e).context("write pty master"),
        }
    }
    Ok(())
}
