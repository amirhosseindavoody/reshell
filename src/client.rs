use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{
    tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios,
};
use nix::unistd::{read as nix_read, write as nix_write};

use crate::context::ContextSnapshot;
use crate::protocol::{self, Message, Winsize};
use crate::session::{self, SessionPaths};
use crate::termstate::TermState;
use crate::vscode_si;

static WINCH_FLAG: AtomicBool = AtomicBool::new(false);
static HUP_FLAG: AtomicBool = AtomicBool::new(false);
static SWITCH_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_winch(_: nix::libc::c_int) {
    WINCH_FLAG.store(true, Ordering::Relaxed);
}

extern "C" fn handle_hup(_: nix::libc::c_int) {
    HUP_FLAG.store(true, Ordering::Relaxed);
}

extern "C" fn handle_usr1(_: nix::libc::c_int) {
    SWITCH_FLAG.store(true, Ordering::Relaxed);
}

enum AttachEnd {
    /// Normal detach / hangup / peer close.
    Done,
    /// Outer client should connect to this session next (TTY stays raw).
    SwitchTo(String),
}

/// Attach to `name` on this process's TTY.
///
/// While attached, a `SIGUSR1` + `switch_to` request (from an in-session
/// `join_session` / `new` / picker) detaches the current session and attaches
/// to the next one in-loop — so leaving a session never nests a second client.
pub fn attach(base: &std::path::Path, name: &str, detach_key: u8) -> Result<()> {
    // Validate target before requiring a TTY so scripts/tests get clear errors.
    preflight_attach(base, name, /*wait_if_attached=*/ false)?;

    let stdin_fd = io::stdin().as_raw_fd();
    if !nix::unistd::isatty(stdin_fd).unwrap_or(false) {
        bail!("stdin is not a tty; attach requires a terminal");
    }

    let orig = tcgetattr(io::stdin().as_fd()).context("tcgetattr")?;
    let mut raw = orig.clone();
    make_raw(&mut raw);
    tcsetattr(io::stdin().as_fd(), SetArg::TCSAFLUSH, &raw).context("tcsetattr raw")?;
    let restore_on_drop = TermiosGuard {
        fd: stdin_fd,
        termios: Rc::new(orig),
    };

    if vscode_si::running_in_vscode_terminal() {
        let _ = write_all_fd(io::stdout().as_raw_fd(), vscode_si::OSC_633_COMMAND_FINISHED);
        let _ = io::stdout().flush();
    }

    unsafe {
        signal::signal(Signal::SIGWINCH, SigHandler::Handler(handle_winch))
            .context("install SIGWINCH handler")?;
        signal::signal(Signal::SIGHUP, SigHandler::Handler(handle_hup))
            .context("install SIGHUP handler")?;
        signal::signal(Signal::SIGUSR1, SigHandler::Handler(handle_usr1))
            .context("install SIGUSR1 handler")?;
        let _ = signal::signal(Signal::SIGINT, SigHandler::SigIgn);
        let _ = signal::signal(Signal::SIGTERM, SigHandler::SigIgn);
    }

    // Clear any stale switch request from a previous run.
    SWITCH_FLAG.store(false, Ordering::Relaxed);
    HUP_FLAG.store(false, Ordering::Relaxed);
    WINCH_FLAG.store(false, Ordering::Relaxed);

    let mut current = name.to_string();
    let mut wait_if_attached = false;
    let result = loop {
        match attach_one(base, &current, stdin_fd, detach_key, wait_if_attached) {
            Ok(AttachEnd::Done) => break Ok(()),
            Ok(AttachEnd::SwitchTo(next)) => {
                eprintln!("switching to {next}");
                let _ = write_all_fd(
                    io::stdout().as_raw_fd(),
                    &TermState::client_cleanup_sequence(),
                );
                let _ = io::stdout().flush();
                current = next;
                // After a switch the previous lock may still be releasing.
                wait_if_attached = true;
            }
            Err(e) => break Err(e),
        }
    };

    let _ = write_all_fd(io::stdout().as_raw_fd(), &TermState::client_cleanup_sequence());
    let _ = io::stdout().flush();
    drop(restore_on_drop);
    unsafe {
        let _ = signal::signal(Signal::SIGWINCH, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGHUP, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGUSR1, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGINT, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGTERM, SigHandler::SigDfl);
    }
    result
}

fn preflight_attach(base: &std::path::Path, name: &str, wait_if_attached: bool) -> Result<()> {
    session::validate_session_name(name)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        if paths.dir.exists() {
            bail!(
                "session '{name}' meta missing under {} (incomplete session dir)",
                paths.dir.display()
            );
        }
        bail!("session '{name}' not found");
    }
    let meta = session::read_meta(&paths).with_context(|| {
        format!(
            "read meta for session '{name}' at {}",
            paths.meta.display()
        )
    })?;
    if !session::process_alive(meta.pid) {
        let _ = session::cleanup_session_files(&paths);
        bail!(
            "session '{name}' is not running (daemon pid {} is dead; cleaned up leftovers)",
            meta.pid
        );
    }
    let mut attempts = 0;
    while session::is_attached(&paths) {
        if !wait_if_attached {
            bail!("session '{name}' is already attached");
        }
        attempts += 1;
        if attempts > 50 {
            bail!("session '{name}' is already attached");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !paths.socket.exists() {
        bail!(
            "session '{name}' socket missing at {} (daemon pid {} alive but not listening?)",
            paths.socket.display(),
            meta.pid
        );
    }
    Ok(())
}

fn attach_one(
    base: &std::path::Path,
    name: &str,
    stdin_fd: i32,
    detach_key: u8,
    wait_if_attached: bool,
) -> Result<AttachEnd> {
    preflight_attach(base, name, wait_if_attached)?;
    let paths = SessionPaths::for_name(base, name);

    let mut stream = UnixStream::connect(&paths.socket).with_context(|| {
        format!(
            "connect to {} (session '{name}')",
            paths.socket.display(),
        )
    })?;
    set_nonblocking(stream.as_raw_fd())?;

    let ws = current_winsize(stdin_fd)?;
    set_blocking(stream.as_raw_fd())?;
    protocol::write_message(&mut stream, &Message::Attach(ws))?;
    stream.flush()?;
    set_nonblocking(stream.as_raw_fd())?;

    client_loop(&mut stream, stdin_fd, detach_key, &paths)
}

/// Fetch a read-only context snapshot without taking the attach lock.
pub fn fetch_context(base: &std::path::Path, name: &str) -> Result<ContextSnapshot> {
    session::validate_session_name(name)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        bail!("session '{name}' not found");
    }
    let meta = session::read_meta(&paths)?;
    if !session::process_alive(meta.pid) {
        let _ = session::cleanup_session_files(&paths);
        bail!("session '{name}' is not running");
    }
    if !paths.socket.exists() {
        bail!("session '{name}' socket missing at {}", paths.socket.display());
    }

    let mut stream = UnixStream::connect(&paths.socket).with_context(|| {
        format!("connect to {} for context", paths.socket.display())
    })?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    protocol::write_message(&mut stream, &Message::ContextReq)?;

    loop {
        match protocol::read_message(&mut stream)? {
            Some(Message::ContextRes(payload)) => {
                let snap: ContextSnapshot = serde_json::from_slice(&payload)
                    .context("decode context snapshot")?;
                return Ok(snap);
            }
            Some(Message::Data(_)) => continue,
            Some(other) => bail!("unexpected message while fetching context: {other:?}"),
            None => bail!("session '{name}' closed the socket before sending context"),
        }
    }
}

struct TermiosGuard {
    fd: i32,
    termios: Rc<Termios>,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(
            unsafe { BorrowedFd::borrow_raw(self.fd) },
            SetArg::TCSAFLUSH,
            &self.termios,
        );
    }
}

fn make_raw(termios: &mut Termios) {
    // cfmakeraw equivalent
    termios.input_flags = nix::sys::termios::InputFlags::empty();
    termios.output_flags &= !(OutputFlags::OPOST);
    termios.local_flags &= !(LocalFlags::ECHO
        | LocalFlags::ECHONL
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN);
    termios.control_flags &= !(nix::sys::termios::ControlFlags::CSIZE
        | nix::sys::termios::ControlFlags::PARENB);
    termios.control_flags |= nix::sys::termios::ControlFlags::CS8;
    termios.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
    termios.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
}

fn current_winsize(fd: i32) -> Result<Winsize> {
    let mut ws = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, &mut ws) };
    if ret != 0 {
        // Keep defaults.
        return Ok(Winsize {
            rows: 24,
            cols: 80,
        });
    }
    Ok(Winsize {
        rows: ws.ws_row,
        cols: ws.ws_col,
    })
}

fn set_nonblocking(fd: i32) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("F_GETFL")?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).context("F_SETFL O_NONBLOCK")?;
    Ok(())
}

fn set_blocking(fd: i32) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).context("F_GETFL")?;
    let flags = OFlag::from_bits_truncate(flags) - OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).context("F_SETFL blocking")?;
    Ok(())
}

fn client_loop(
    stream: &mut UnixStream,
    stdin_fd: i32,
    detach_key: u8,
    paths: &SessionPaths,
) -> Result<AttachEnd> {
    let stdout_fd = io::stdout().as_raw_fd();
    let mut stdin_buf = [0u8; 4096];
    let mut sock_buf = [0u8; 8192];
    let mut pending_stdin: Vec<u8> = Vec::new();
    let mut inbound: Vec<u8> = Vec::new();
    let mut outbound: Vec<u8> = Vec::new();

    loop {
        if SWITCH_FLAG.swap(false, Ordering::Relaxed) {
            if let Some(next) = session::take_switch_to(paths) {
                outbound.extend(protocol::encode_message(&Message::Detach)?);
                let _ = flush_outbound(stream, &mut outbound);
                return Ok(AttachEnd::SwitchTo(next));
            }
        }
        if HUP_FLAG.swap(false, Ordering::Relaxed) {
            outbound.extend(protocol::encode_message(&Message::Detach)?);
            let _ = flush_outbound(stream, &mut outbound);
            return Ok(AttachEnd::Done);
        }
        if WINCH_FLAG.swap(false, Ordering::Relaxed) {
            let ws = current_winsize(stdin_fd)?;
            outbound.extend(protocol::encode_message(&Message::Resize(ws))?);
        }

        let mut sock_interest = PollFlags::POLLIN;
        if !outbound.is_empty() {
            sock_interest |= PollFlags::POLLOUT;
        }
        let mut fds = [
            PollFd::new(unsafe { BorrowedFd::borrow_raw(stdin_fd) }, PollFlags::POLLIN),
            PollFd::new(stream.as_fd(), sock_interest),
        ];
        match poll(&mut fds, 200u16) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll"),
        }

        let stdin_ev = fds[0].revents().unwrap_or(PollFlags::empty());
        let sock_ev = fds[1].revents().unwrap_or(PollFlags::empty());

        if stdin_ev.contains(PollFlags::POLLIN) {
            match nix_read(stdin_fd, &mut stdin_buf) {
                Ok(0) => {
                    outbound.extend(protocol::encode_message(&Message::Detach)?);
                    let _ = flush_outbound(stream, &mut outbound);
                    return Ok(AttachEnd::Done);
                }
                Ok(n) => {
                    pending_stdin.extend_from_slice(&stdin_buf[..n]);
                    if enqueue_stdin(&mut outbound, &mut pending_stdin, detach_key)? {
                        let _ = flush_outbound(stream, &mut outbound);
                        return Ok(AttachEnd::Done);
                    }
                }
                Err(Errno::EINTR) | Err(Errno::EAGAIN) => {}
                Err(e) => return Err(e).context("read stdin"),
            }
        }

        if sock_ev.contains(PollFlags::POLLOUT) || !outbound.is_empty() {
            if flush_outbound(stream, &mut outbound)? {
                return Ok(AttachEnd::Done);
            }
        }

        if sock_ev.contains(PollFlags::POLLIN) {
            loop {
                match stream.read(&mut sock_buf) {
                    Ok(0) => return Ok(AttachEnd::Done),
                    Ok(n) => inbound.extend_from_slice(&sock_buf[..n]),
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e).context("read from session"),
                }
            }
            let messages = protocol::drain_messages(&mut inbound)?;
            for msg in messages {
                if let Message::Data(data) = msg {
                    write_all_fd(stdout_fd, &data)?;
                }
            }
        }

        if sock_ev.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Ok(AttachEnd::Done);
        }
        if stdin_ev.intersects(PollFlags::POLLERR | PollFlags::POLLHUP) {
            outbound.extend(protocol::encode_message(&Message::Detach)?);
            let _ = flush_outbound(stream, &mut outbound);
            return Ok(AttachEnd::Done);
        }
    }
}

/// Queue stdin bytes; returns true if detach was requested.
fn enqueue_stdin(outbound: &mut Vec<u8>, pending: &mut Vec<u8>, detach_key: u8) -> Result<bool> {
    if pending.is_empty() {
        return Ok(false);
    }
    if let Some(pos) = pending.iter().position(|&b| b == detach_key) {
        let before = pending[..pos].to_vec();
        if !before.is_empty() {
            outbound.extend(protocol::encode_message(&Message::Data(before))?);
        }
        outbound.extend(protocol::encode_message(&Message::Detach)?);
        pending.clear();
        return Ok(true);
    }
    let data = std::mem::take(pending);
    outbound.extend(protocol::encode_message(&Message::Data(data))?);
    Ok(false)
}

/// Returns true if the peer closed the connection.
fn flush_outbound(stream: &mut UnixStream, outbound: &mut Vec<u8>) -> Result<bool> {
    while !outbound.is_empty() {
        match stream.write(outbound) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                outbound.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e).context("write to session"),
        }
    }
    Ok(false)
}

fn write_all_fd(fd: i32, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        match nix_write(unsafe { BorrowedFd::borrow_raw(fd) }, data) {
            Ok(0) => bail!("short write to stdout"),
            Ok(n) => data = &data[n..],
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("write stdout"),
        }
    }
    Ok(())
}
