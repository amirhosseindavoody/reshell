use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

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
use crate::termstate::TermState;

/// Stop reading the PTY into the client buffer beyond this size so the shell
/// experiences backpressure instead of unbounded memory growth.
const OUTBOUND_HIGH_WATER: usize = 256 * 1024;

/// After attach, keep the temporary (bumped) winsize at least this long so
/// differential TUIs (ratatui/crossterm) can emit a full cell dump at the new
/// geometry before we restore the real size.
const ATTACH_REDRAW_MIN: Duration = Duration::from_millis(50);

/// Always restore the real winsize by this deadline even if no PTY output
/// arrived (plain shells may not redraw on SIGWINCH).
const ATTACH_REDRAW_MAX: Duration = Duration::from_millis(250);

pub struct NewSessionOpts {
    pub name: String,
    pub shell: String,
    pub base: std::path::PathBuf,
}

struct ClientConn {
    stream: UnixStream,
    outbound: Vec<u8>,
    inbound: Vec<u8>,
    /// True after the client has sent `Attach` (modes restored, winsize applied).
    ready: bool,
}

/// Two-phase attach redraw: hold a temporary winsize long enough for the child
/// to full-paint, then restore the client's real size (another full paint).
///
/// Instant bump+restore is coalesced by apps like fresh (ratatui): crossterm
/// only writes cells that differ from its previous buffer, so a same-size
/// redraw after reattach emits almost nothing onto a blank client TTY.
struct PendingAttachRedraw {
    final_ws: Winsize,
    started: Instant,
    saw_output: bool,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Result<Self> {
        set_nonblocking(stream.as_raw_fd())?;
        Ok(Self {
            stream,
            outbound: Vec::new(),
            inbound: Vec::new(),
            ready: false,
        })
    }

    fn enqueue(&mut self, msg: &Message) -> Result<()> {
        let bytes = protocol::encode_message(msg)?;
        self.outbound.extend_from_slice(&bytes);
        Ok(())
    }

    fn flush_outbound(&mut self) -> Result<bool> {
        while !self.outbound.is_empty() {
            match self.stream.write(&self.outbound) {
                Ok(0) => return Ok(true), // disconnect
                Ok(n) => {
                    self.outbound.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return Ok(true),
            }
        }
        Ok(false)
    }

    fn read_inbound(&mut self) -> Result<bool> {
        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(true),
                Ok(n) => self.inbound.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return Ok(true),
            }
        }
        Ok(false)
    }
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

    let (read_fd, write_fd) = nix::unistd::pipe().context("create readiness pipe")?;

    match unsafe { fork() }.context("fork session daemon")? {
        ForkResult::Parent { child: _ } => {
            let read_raw = read_fd.as_raw_fd();
            drop(write_fd);
            wait_for_ready(read_raw, &paths)?;
            drop(read_fd);
            Ok(())
        }
        ForkResult::Child => {
            drop(read_fd);
            let _ = setsid();
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

            let created = now_unix();
            let meta = SessionMeta {
                name: opts.name.clone(),
                pid: Pid::this().as_raw(),
                shell: shell_path,
                created_unix: created,
                attached: false,
                last_active_unix: created,
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

            let _ = unsafe { nix::libc::ioctl(slave_fd, nix::libc::TIOCSCTTY as _, 0) };

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

/// Winsize that differs from `ws` so `TIOCSWINSZ` delivers `SIGWINCH` and
/// ratatui-style apps invalidate their previous cell buffer (full paint).
fn temporary_redraw_winsize(ws: Winsize) -> Winsize {
    Winsize {
        rows: if ws.rows > 1 {
            ws.rows - 1
        } else {
            ws.rows.saturating_add(1)
        },
        cols: ws.cols,
    }
}

fn apply_winsize_and_signal(master: RawFd, ws: Winsize) -> Result<()> {
    apply_winsize(master, ws)?;
    signal_foreground_winch(master);
    Ok(())
}

fn signal_foreground_winch(master: RawFd) {
    let mut pgrp: nix::libc::pid_t = 0;
    let ret = unsafe { nix::libc::ioctl(master, nix::libc::TIOCGPGRP as _, &mut pgrp) };
    if ret == 0 && pgrp > 0 {
        let _ = signal::kill(Pid::from_raw(-pgrp), Signal::SIGWINCH);
    }
}

fn pending_redraw_ready(pending: &PendingAttachRedraw) -> bool {
    let elapsed = pending.started.elapsed();
    if elapsed >= ATTACH_REDRAW_MAX {
        return true;
    }
    pending.saw_output && elapsed >= ATTACH_REDRAW_MIN
}

fn server_loop(
    master: OwnedFd,
    listener: UnixListener,
    shell_pid: Pid,
    paths: &SessionPaths,
) -> Result<()> {
    let master_fd = master.as_raw_fd();
    let mut client: Option<ClientConn> = None;
    let mut term = TermState::new();
    let mut pending_redraw: Option<PendingAttachRedraw> = None;
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

        if let Some(ref pending) = pending_redraw {
            if pending_redraw_ready(pending) {
                let final_ws = pending.final_ws;
                pending_redraw = None;
                let _ = apply_winsize_and_signal(master_fd, final_ws);
            }
        }

        let want_pty_read = match &client {
            Some(c) => c.outbound.len() < OUTBOUND_HIGH_WATER,
            None => true, // drain + discard while detached
        };

        let mut fds = Vec::with_capacity(3);
        let mut pty_flags = PollFlags::empty();
        if want_pty_read {
            pty_flags |= PollFlags::POLLIN;
        }
        fds.push(PollFd::new(master.as_fd(), pty_flags));
        fds.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
        if let Some(ref c) = client {
            let mut interest = PollFlags::POLLIN;
            if !c.outbound.is_empty() {
                interest |= PollFlags::POLLOUT;
            }
            fds.push(PollFd::new(c.stream.as_fd(), interest));
        }

        // Wake sooner while waiting to restore the real winsize after attach.
        let timeout_ms: u16 = if pending_redraw.is_some() { 20 } else { 500 };

        match poll(&mut fds, timeout_ms) {
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
                    if client.is_some() {
                        drop(stream);
                    } else {
                        match ClientConn::new(stream) {
                            Ok(c) => {
                                client = Some(c);
                                let _ = set_attached(paths, true);
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("accept"),
            }
        }

        if master_revents.contains(PollFlags::POLLIN) {
            match nix_read(master_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Always parse modes — including while detached — so reattach
                    // can restore mouse / alt-screen on the new client TTY.
                    term.feed(&buf[..n]);
                    if let Some(ref mut c) = client {
                        if c.ready {
                            if let Some(ref mut pending) = pending_redraw {
                                pending.saw_output = true;
                            }
                            if c.enqueue(&Message::Data(buf[..n].to_vec())).is_err() {
                                pending_redraw = None;
                                drop_client(&mut client, paths);
                            }
                        }
                        // else: wait for Attach (mode restore) before forwarding
                    }
                    // else discarded (still drained; modes above still updated)
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

        if let Some(ref mut c) = client {
            if client_revents.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
                pending_redraw = None;
                drop_client(&mut client, paths);
            } else {
                let mut dead = false;
                if client_revents.contains(PollFlags::POLLOUT) || !c.outbound.is_empty() {
                    dead = c.flush_outbound()?;
                }
                if !dead
                    && client_revents
                        .intersects(PollFlags::POLLIN | PollFlags::POLLHUP)
                {
                    dead = c.read_inbound()?;
                    if !dead {
                        match handle_client_messages(c, master_fd, &term, &mut pending_redraw) {
                            Ok(true) => dead = true,
                            Ok(false) => {}
                            Err(_) => dead = true,
                        }
                    }
                }
                if dead {
                    pending_redraw = None;
                    drop_client(&mut client, paths);
                } else if !c.outbound.is_empty() {
                    // Opportunistic flush after enqueue from PTY.
                    if c.flush_outbound()? {
                        pending_redraw = None;
                        drop_client(&mut client, paths);
                    }
                }
            }
        }
    }

    let _ = signal::kill(shell_pid, Signal::SIGHUP);
    let _ = waitpid(shell_pid, None);
    Ok(())
}

fn drop_client(client: &mut Option<ClientConn>, paths: &SessionPaths) {
    *client = None;
    let _ = set_attached(paths, false);
}

/// Returns Ok(true) if client should disconnect (Detach).
fn handle_client_messages(
    client: &mut ClientConn,
    master_fd: RawFd,
    term: &TermState,
    pending_redraw: &mut Option<PendingAttachRedraw>,
) -> Result<bool> {
    let messages = protocol::drain_messages(&mut client.inbound)?;
    for msg in messages {
        match msg {
            Message::Attach(ws) => {
                // Re-enable DEC modes (mouse, alt-screen, …) on the new TTY
                // before asking the child to redraw.
                let mut restore = term.restore_sequence();
                // Clear whatever is currently visible so a differential TUI
                // paint isn't merged onto stale local cells.
                restore.extend_from_slice(b"\x1b[H\x1b[2J");
                client.enqueue(&Message::Data(restore))?;

                // Phase 1: temporary geometry so ratatui invalidates its
                // previous buffer and emits a full cell dump to this client.
                let temp = temporary_redraw_winsize(ws);
                apply_winsize_and_signal(master_fd, temp)?;
                *pending_redraw = Some(PendingAttachRedraw {
                    final_ws: ws,
                    started: Instant::now(),
                    saw_output: false,
                });
                client.ready = true;
            }
            Message::Resize(ws) => {
                if let Some(ref mut pending) = pending_redraw {
                    // User resized during the attach redraw dance — land on
                    // their latest size when we finish.
                    pending.final_ws = ws;
                } else {
                    apply_winsize(master_fd, ws)?;
                }
            }
            Message::Data(data) => {
                write_all_fd(master_fd, &data)?;
            }
            Message::Detach => return Ok(true),
        }
    }
    Ok(false)
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
