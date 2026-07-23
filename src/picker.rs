//! Minimal session picker for bare `reshell` / `reshell attach` (no name).
//!
//! Order: create-new, then detached (attachable), then attached (gray, skipped).
//! Cursor defaults to the first attachable session when one exists.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::termios::{
    tcgetattr, tcsetattr, LocalFlags, SetArg, SpecialCharacterIndices, Termios,
};
use nix::unistd::read as nix_read;

const CSI_CLEAR_LINE: &str = "\x1b[2K";
const CSI_HIDE_CURSOR: &str = "\x1b[?25l";
const CSI_SHOW_CURSOR: &str = "\x1b[?25h";
const SGR_RESET: &str = "\x1b[0m";
const SGR_REVERSE: &str = "\x1b[7m";
const SGR_DIM: &str = "\x1b[90m";

/// Outcome of the interactive session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAction {
    CreateNew,
    Attach(String),
    Cancelled,
}

/// One live session row for the picker.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub name: String,
    pub attached: bool,
    /// Extra columns shown after the name (state, last-active, shell, …).
    pub detail: String,
}

#[derive(Clone)]
enum Entry {
    CreateNew,
    Session {
        name: String,
        attached: bool,
        detail: String,
    },
}

/// Interactive picker. Requires a controlling TTY (`/dev/tty` or stdin).
///
/// Detached sessions are selectable; attached ones are shown dimmed and skipped
/// by the cursor. Esc / `q` / Ctrl+C cancel.
pub fn pick_session(sessions: &[SessionRow]) -> Result<PickAction> {
    let mut tty = open_tty()?;
    let tty_fd = tty.as_raw_fd();
    if !nix::unistd::isatty(tty_fd).unwrap_or(false) {
        bail!("no tty available; session picker requires a terminal");
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(sessions.len() + 1);
    entries.push(Entry::CreateNew);

    let mut detached: Vec<&SessionRow> = sessions.iter().filter(|s| !s.attached).collect();
    let mut attached: Vec<&SessionRow> = sessions.iter().filter(|s| s.attached).collect();
    // Caller usually sorts by activity already; keep relative order within groups.
    for s in detached.drain(..) {
        entries.push(Entry::Session {
            name: s.name.clone(),
            attached: false,
            detail: s.detail.clone(),
        });
    }
    for s in attached.drain(..) {
        entries.push(Entry::Session {
            name: s.name.clone(),
            attached: true,
            detail: s.detail.clone(),
        });
    }

    let mut cursor = first_selectable(&entries).unwrap_or(0);

    let orig = tcgetattr(tty.as_fd()).context("tcgetattr")?;
    let mut raw = orig.clone();
    make_cbreak(&mut raw);
    tcsetattr(tty.as_fd(), SetArg::TCSAFLUSH, &raw).context("tcsetattr cbreak")?;
    let _guard = TermiosGuard {
        fd: tty_fd,
        termios: Rc::new(orig),
    };

    write!(tty, "{CSI_HIDE_CURSOR}").ok();
    tty.flush().ok();

    let n_lines = entries.len() + 2; // header + blank + rows
    let cols = tty_cols(tty_fd).unwrap_or(80).max(20) as usize;
    let mut first_draw = true;
    let result = (|| -> Result<PickAction> {
        loop {
            draw(&mut tty, &entries, cursor, first_draw, n_lines, cols)?;
            first_draw = false;
            match read_key(tty_fd)? {
                Key::Up => {
                    if let Some(i) = prev_selectable(&entries, cursor) {
                        cursor = i;
                    }
                }
                Key::Down => {
                    if let Some(i) = next_selectable(&entries, cursor) {
                        cursor = i;
                    }
                }
                Key::Enter => match &entries[cursor] {
                    Entry::CreateNew => {
                        clear_ui(&mut tty, n_lines)?;
                        return Ok(PickAction::CreateNew);
                    }
                    Entry::Session {
                        name,
                        attached: false,
                        ..
                    } => {
                        let name = name.clone();
                        clear_ui(&mut tty, n_lines)?;
                        return Ok(PickAction::Attach(name));
                    }
                    Entry::Session {
                        attached: true, ..
                    } => {
                        // Cursor should never land here; ignore Enter.
                    }
                },
                Key::Cancel => {
                    clear_ui(&mut tty, n_lines)?;
                    return Ok(PickAction::Cancelled);
                }
                Key::Other => {}
            }
        }
    })();

    write!(tty, "{CSI_SHOW_CURSOR}").ok();
    tty.flush().ok();
    result
}

/// Open the controlling terminal for interactive UI I/O.
///
/// Prefer `/dev/tty` so the picker still works if stdout/stderr are redirected.
/// Fall back to stdin when `/dev/tty` is unavailable but stdin is a TTY.
fn open_tty() -> Result<std::fs::File> {
    match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(f) => Ok(f),
        Err(_) => {
            let stdin_fd = io::stdin().as_raw_fd();
            if !nix::unistd::isatty(stdin_fd).unwrap_or(false) {
                bail!("stdin is not a tty; session picker requires a terminal");
            }
            // Duplicate stdin so we can write UI escapes even when only stdin is a TTY.
            let dup = nix::unistd::dup(stdin_fd).context("dup stdin for picker")?;
            Ok(unsafe { std::fs::File::from(OwnedFd::from_raw_fd(dup)) })
        }
    }
}

fn first_selectable(entries: &[Entry]) -> Option<usize> {
    // Prefer first attachable session so "Create new" starts unselected when
    // there is something to reconnect to.
    entries
        .iter()
        .enumerate()
        .find(|(_, e)| matches!(e, Entry::Session { attached: false, .. }))
        .map(|(i, _)| i)
        .or_else(|| {
            entries
                .iter()
                .enumerate()
                .find(|(_, e)| is_selectable(e))
                .map(|(i, _)| i)
        })
}

fn is_selectable(e: &Entry) -> bool {
    match e {
        Entry::CreateNew => true,
        Entry::Session { attached, .. } => !attached,
    }
}

fn prev_selectable(entries: &[Entry], from: usize) -> Option<usize> {
    (0..from).rev().find(|&i| is_selectable(&entries[i]))
}

fn next_selectable(entries: &[Entry], from: usize) -> Option<usize> {
    ((from + 1)..entries.len()).find(|&i| is_selectable(&entries[i]))
}

fn draw(
    out: &mut impl Write,
    entries: &[Entry],
    cursor: usize,
    first_draw: bool,
    n_lines: usize,
    cols: usize,
) -> Result<()> {
    if !first_draw {
        // Cursor sits on the line *after* the last row; move to the header.
        write!(out, "\x1b[{n_lines}A").ok();
    }
    // Always start at column 0 before clearing — required if OPOST was off and
    // also keeps redraw correct after soft-wrap edge cases.
    write_line(out, &truncate_visible("Select a session (↑/↓ Enter, q Esc)", cols))?;
    write_line(out, "")?;
    for (i, entry) in entries.iter().enumerate() {
        let selected = i == cursor;
        let line = match entry {
            Entry::CreateNew => {
                if selected {
                    format!("{SGR_REVERSE}> Create new session{SGR_RESET}")
                } else {
                    "  Create new session".to_string()
                }
            }
            Entry::Session {
                name,
                attached,
                detail,
            } => {
                if *attached {
                    format!("{SGR_DIM}  {name:<20} {detail}{SGR_RESET}")
                } else if selected {
                    format!("{SGR_REVERSE}> {name:<20} {detail}{SGR_RESET}")
                } else {
                    format!("  {name:<20} {detail}")
                }
            }
        };
        write_line(out, &truncate_visible(&line, cols))?;
    }
    out.flush().ok();
    Ok(())
}

/// Visible-width truncate that leaves CSI/SGR sequences intact enough for SGR_RESET.
fn truncate_visible(s: &str, cols: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut visible = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if visible >= cols {
            break;
        }
        out.push(c);
        visible += 1;
    }
    // Ensure styles don't leak if we cut mid-line.
    if s.contains(SGR_REVERSE) || s.contains(SGR_DIM) {
        if !out.ends_with(SGR_RESET) {
            out.push_str(SGR_RESET);
        }
    }
    out
}

fn tty_cols(fd: RawFd) -> Option<u16> {
    let mut ws = nix::pty::Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col)
    } else {
        None
    }
}

/// Write one UI row: CR + clear line + text + LF.
///
/// Leading `\r` keeps the row left-aligned. We leave OPOST on so `\n` still
/// advances the line correctly on the TTY (do not emit `\r\n` here or OPOST
/// turns it into `\r\r\n`).
fn write_line(out: &mut impl Write, text: &str) -> Result<()> {
    write!(out, "\r{CSI_CLEAR_LINE}{text}\n").ok();
    Ok(())
}

fn clear_ui(out: &mut impl Write, n_lines: usize) -> Result<()> {
    write!(out, "\x1b[{n_lines}A").ok();
    for _ in 0..n_lines {
        write_line(out, "")?;
    }
    write!(out, "\x1b[{n_lines}A").ok();
    out.flush().ok();
    Ok(())
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Other,
}

fn read_key(fd: RawFd) -> Result<Key> {
    let b = read_byte_blocking(fd)?;
    match b {
        b'\r' | b'\n' => Ok(Key::Enter),
        b'q' | b'Q' | 0x03 => Ok(Key::Cancel),
        0x1b => read_escape_sequence(fd),
        b'k' | b'K' => Ok(Key::Up),
        b'j' | b'J' => Ok(Key::Down),
        _ => Ok(Key::Other),
    }
}

/// After ESC: arrow keys are `ESC [ A/B`. Bare Esc (no follow-up within ~50ms)
/// cancels. Other CSI sequences are ignored.
fn read_escape_sequence(fd: RawFd) -> Result<Key> {
    let Some(b1) = read_byte_timeout(fd, 50)? else {
        return Ok(Key::Cancel);
    };
    if b1 != b'[' {
        return Ok(Key::Cancel);
    }
    let Some(b2) = read_byte_timeout(fd, 50)? else {
        return Ok(Key::Cancel);
    };
    Ok(match b2 {
        b'A' => Key::Up,
        b'B' => Key::Down,
        _ => Key::Other,
    })
}

fn read_byte_blocking(fd: RawFd) -> Result<u8> {
    let mut buf = [0u8; 1];
    loop {
        match nix_read(fd, &mut buf) {
            Ok(0) => bail!("tty closed"),
            Ok(_) => return Ok(buf[0]),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("read key"),
        }
    }
}

fn read_byte_timeout(fd: RawFd, timeout_ms: u16) -> Result<Option<u8>> {
    let mut pfd = [PollFd::new(
        unsafe { BorrowedFd::borrow_raw(fd) },
        PollFlags::POLLIN,
    )];
    loop {
        match poll(&mut pfd, timeout_ms) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll tty"),
        }
    }
    let mut buf = [0u8; 1];
    match nix_read(fd, &mut buf) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(buf[0])),
        Err(Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e).context("read escape byte"),
    }
}

struct TermiosGuard {
    fd: RawFd,
    termios: Rc<Termios>,
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        let _ = tcsetattr(
            unsafe { BorrowedFd::borrow_raw(self.fd) },
            SetArg::TCSAFLUSH,
            &self.termios,
        );
        // Best-effort cursor restore if the caller did not already show it.
        if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
            let _ = write!(tty, "{CSI_SHOW_CURSOR}");
            let _ = tty.flush();
        }
    }
}

/// Input raw enough for single-key reads, but **keep OPOST** so the terminal
/// still maps `\n` → `\r\n`. Full `cfmakeraw` (which clears OPOST) made the
/// picker staircase because `writeln!` only emits `\n`.
fn make_cbreak(termios: &mut Termios) {
    termios.local_flags &= !(LocalFlags::ECHO
        | LocalFlags::ECHONL
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN);
    termios.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;
    termios.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_prefers_first_detached() {
        let entries = vec![
            Entry::CreateNew,
            Entry::Session {
                name: "a".into(),
                attached: false,
                detail: String::new(),
            },
            Entry::Session {
                name: "b".into(),
                attached: true,
                detail: String::new(),
            },
        ];
        assert_eq!(first_selectable(&entries), Some(1));
    }

    #[test]
    fn cursor_falls_back_to_create_when_all_attached() {
        let entries = vec![
            Entry::CreateNew,
            Entry::Session {
                name: "busy".into(),
                attached: true,
                detail: String::new(),
            },
        ];
        assert_eq!(first_selectable(&entries), Some(0));
    }

    #[test]
    fn navigation_skips_attached() {
        let entries = vec![
            Entry::CreateNew,
            Entry::Session {
                name: "free".into(),
                attached: false,
                detail: String::new(),
            },
            Entry::Session {
                name: "busy".into(),
                attached: true,
                detail: String::new(),
            },
        ];
        assert_eq!(next_selectable(&entries, 0), Some(1));
        assert_eq!(next_selectable(&entries, 1), None);
        assert_eq!(prev_selectable(&entries, 1), Some(0));
        assert_eq!(next_selectable(&entries, 0), Some(1));
    }

    #[test]
    fn write_line_uses_cr_clear_and_lf() {
        let mut buf = Vec::new();
        write_line(&mut buf, "hello").unwrap();
        assert_eq!(buf, b"\r\x1b[2Khello\n");
    }

    #[test]
    fn draw_keeps_rows_left_aligned() {
        let entries = vec![
            Entry::CreateNew,
            Entry::Session {
                name: "demo".into(),
                attached: false,
                detail: "detached".into(),
            },
        ];
        let mut buf = Vec::new();
        draw(&mut buf, &entries, 1, true, 4, 80).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("\r\x1b[2K  Create new session\n"));
        assert!(text.contains("demo"));
        // Redraw moves up by the fixed row count.
        let mut buf2 = Vec::new();
        draw(&mut buf2, &entries, 0, false, 4, 80).unwrap();
        assert!(
            buf2.starts_with(b"\x1b[4A"),
            "{:?}",
            &buf2[..8.min(buf2.len())]
        );
    }
}
