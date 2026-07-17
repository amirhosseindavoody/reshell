use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::termios::{
    tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios,
};
use nix::unistd::{read as nix_read, write as nix_write};

use crate::protocol::{self, Message, Winsize, DETACH_BYTE};
use crate::session::{self, SessionPaths};

static WINCH_FLAG: AtomicBool = AtomicBool::new(false);
static HUP_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_winch(_: nix::libc::c_int) {
    WINCH_FLAG.store(true, Ordering::Relaxed);
}

extern "C" fn handle_hup(_: nix::libc::c_int) {
    HUP_FLAG.store(true, Ordering::Relaxed);
}

pub fn attach(base: &std::path::Path, name: &str) -> Result<()> {
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
    if session::is_attached(&paths) {
        bail!("session '{name}' is already attached");
    }
    if !paths.socket.exists() {
        bail!("session '{name}' socket missing");
    }

    let stdin_fd = io::stdin().as_raw_fd();
    if !nix::unistd::isatty(stdin_fd).unwrap_or(false) {
        bail!("stdin is not a tty; attach requires a terminal");
    }

    let mut stream = UnixStream::connect(&paths.socket)
        .with_context(|| format!("connect to {}", paths.socket.display()))?;

    let orig = tcgetattr(io::stdin().as_fd()).context("tcgetattr")?;
    let mut raw = orig.clone();
    make_raw(&mut raw);
    tcsetattr(io::stdin().as_fd(), SetArg::TCSAFLUSH, &raw).context("tcsetattr raw")?;

    let restore_on_drop = TermiosGuard {
        fd: stdin_fd,
        termios: Rc::new(orig),
    };

    unsafe {
        signal::signal(Signal::SIGWINCH, SigHandler::Handler(handle_winch))
            .context("install SIGWINCH handler")?;
        signal::signal(Signal::SIGHUP, SigHandler::Handler(handle_hup))
            .context("install SIGHUP handler")?;
        // Ignore SIGINT/SIGTERM in client so they go to the remote shell via the PTY
        // when the user types Ctrl+C (delivered as bytes in raw mode). Hangup still detaches.
        let _ = signal::signal(Signal::SIGINT, SigHandler::SigIgn);
        let _ = signal::signal(Signal::SIGTERM, SigHandler::SigIgn);
    }

    let ws = current_winsize(stdin_fd)?;
    protocol::write_message(&mut stream, &Message::Attach(ws))?;
    stream.flush()?;

    let result = client_loop(&mut stream, stdin_fd);
    drop(restore_on_drop);
    // Restore default handlers.
    unsafe {
        let _ = signal::signal(Signal::SIGWINCH, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGHUP, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGINT, SigHandler::SigDfl);
        let _ = signal::signal(Signal::SIGTERM, SigHandler::SigDfl);
    }
    result
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

fn client_loop(stream: &mut UnixStream, stdin_fd: i32) -> Result<()> {
    let stdout_fd = io::stdout().as_raw_fd();
    let mut stdin_buf = [0u8; 4096];
    let mut pending_stdin: Vec<u8> = Vec::new();

    loop {
        if HUP_FLAG.swap(false, Ordering::Relaxed) {
            let _ = protocol::write_message(&mut *stream, &Message::Detach);
            let _ = stream.flush();
            return Ok(());
        }
        if WINCH_FLAG.swap(false, Ordering::Relaxed) {
            let ws = current_winsize(stdin_fd)?;
            protocol::write_message(&mut *stream, &Message::Resize(ws))?;
            stream.flush()?;
        }

        let mut fds = [
            PollFd::new(unsafe { BorrowedFd::borrow_raw(stdin_fd) }, PollFlags::POLLIN),
            PollFd::new(stream.as_fd(), PollFlags::POLLIN),
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
                    let _ = protocol::write_message(&mut *stream, &Message::Detach);
                    return Ok(());
                }
                Ok(n) => {
                    pending_stdin.extend_from_slice(&stdin_buf[..n]);
                    if drain_stdin_to_session(stream, &mut pending_stdin)? {
                        return Ok(());
                    }
                }
                Err(Errno::EINTR) => {}
                Err(e) => return Err(e).context("read stdin"),
            }
        }

        if sock_ev.contains(PollFlags::POLLIN) {
            match protocol::read_message(&mut *stream) {
                Ok(Some(Message::Data(data))) => {
                    write_all_fd(stdout_fd, &data)?;
                }
                Ok(Some(_)) => {
                    // Ignore unexpected control messages from server.
                }
                Ok(None) => {
                    // Session ended.
                    return Ok(());
                }
                Err(e) => return Err(e).context("read from session"),
            }
        }

        if sock_ev.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Ok(());
        }
        if stdin_ev.intersects(PollFlags::POLLERR | PollFlags::POLLHUP) {
            let _ = protocol::write_message(&mut *stream, &Message::Detach);
            return Ok(());
        }
    }
}

/// Forward stdin bytes; returns true if detach was requested.
fn drain_stdin_to_session(stream: &mut UnixStream, pending: &mut Vec<u8>) -> Result<bool> {
    if pending.is_empty() {
        return Ok(false);
    }
    if let Some(pos) = pending.iter().position(|&b| b == DETACH_BYTE) {
        let before = pending[..pos].to_vec();
        if !before.is_empty() {
            protocol::write_message(&mut *stream, &Message::Data(before))?;
        }
        protocol::write_message(&mut *stream, &Message::Detach)?;
        stream.flush()?;
        pending.clear();
        return Ok(true);
    }
    let data = std::mem::take(pending);
    protocol::write_message(&mut *stream, &Message::Data(data))?;
    stream.flush()?;
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
