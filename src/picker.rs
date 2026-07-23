//! Minimal session picker for bare `reshell` / `reshell attach` (no name).
//!
//! Order: create-new, then detached (attachable), then attached (gray, skipped).
//! Cursor defaults to the first attachable session when one exists.

use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::termios::{
    tcgetattr, tcsetattr, LocalFlags, OutputFlags, SetArg, SpecialCharacterIndices, Termios,
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
    Session { name: String, attached: bool, detail: String },
}

/// Interactive picker. Requires a TTY on stdin.
///
/// Detached sessions are selectable; attached ones are shown dimmed and skipped
/// by the cursor. Esc / `q` / Ctrl+C cancel.
pub fn pick_session(sessions: &[SessionRow]) -> Result<PickAction> {
    let stdin_fd = io::stdin().as_raw_fd();
    if !nix::unistd::isatty(stdin_fd).unwrap_or(false) {
        bail!("stdin is not a tty; session picker requires a terminal");
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

    let orig = tcgetattr(io::stdin().as_fd()).context("tcgetattr")?;
    let mut raw = orig.clone();
    make_raw(&mut raw);
    tcsetattr(io::stdin().as_fd(), SetArg::TCSAFLUSH, &raw).context("tcsetattr raw")?;
    let _guard = TermiosGuard {
        fd: stdin_fd,
        termios: Rc::new(orig),
    };

    // Draw on stderr so stdout stays clean for scripts that still have a TTY.
    let mut out = io::stderr();
    write!(out, "{CSI_HIDE_CURSOR}").ok();
    out.flush().ok();

    let n_lines = entries.len() + 2; // header + blank + rows
    let mut first_draw = true;
    let result = (|| -> Result<PickAction> {
        loop {
            draw(&mut out, &entries, cursor, first_draw, n_lines)?;
            first_draw = false;
            match read_key()? {
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
                        clear_ui(&mut out, n_lines)?;
                        return Ok(PickAction::CreateNew);
                    }
                    Entry::Session {
                        name,
                        attached: false,
                        ..
                    } => {
                        let name = name.clone();
                        clear_ui(&mut out, n_lines)?;
                        return Ok(PickAction::Attach(name));
                    }
                    Entry::Session { attached: true, .. } => {
                        // Cursor should never land here; ignore Enter.
                    }
                },
                Key::Cancel => {
                    clear_ui(&mut out, n_lines)?;
                    return Ok(PickAction::Cancelled);
                }
                Key::Other => {}
            }
        }
    })();

    write!(out, "{CSI_SHOW_CURSOR}").ok();
    out.flush().ok();
    result
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
) -> Result<()> {
    if !first_draw {
        // Move to the top of the previous draw.
        write!(out, "\x1b[{n_lines}A").ok();
    }
    writeln!(out, "{CSI_CLEAR_LINE}Select a session (↑/↓ Enter, q Esc)").ok();
    writeln!(out, "{CSI_CLEAR_LINE}").ok();
    for (i, entry) in entries.iter().enumerate() {
        write!(out, "{CSI_CLEAR_LINE}").ok();
        let selected = i == cursor;
        match entry {
            Entry::CreateNew => {
                if selected {
                    write!(out, "{SGR_REVERSE}> Create new session{SGR_RESET}").ok();
                } else {
                    write!(out, "  Create new session").ok();
                }
            }
            Entry::Session {
                name,
                attached,
                detail,
            } => {
                if *attached {
                    write!(
                        out,
                        "{SGR_DIM}  {name:<20} {detail}{SGR_RESET}"
                    )
                    .ok();
                } else if selected {
                    write!(
                        out,
                        "{SGR_REVERSE}> {name:<20} {detail}{SGR_RESET}"
                    )
                    .ok();
                } else {
                    write!(out, "  {name:<20} {detail}").ok();
                }
            }
        }
        writeln!(out).ok();
    }
    out.flush().ok();
    Ok(())
}

fn clear_ui(out: &mut impl Write, n_lines: usize) -> Result<()> {
    write!(out, "\x1b[{n_lines}A").ok();
    for _ in 0..n_lines {
        writeln!(out, "{CSI_CLEAR_LINE}").ok();
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

fn read_key() -> Result<Key> {
    let fd = io::stdin().as_raw_fd();
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
fn read_escape_sequence(fd: i32) -> Result<Key> {
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

fn read_byte_blocking(fd: i32) -> Result<u8> {
    let mut buf = [0u8; 1];
    loop {
        match nix_read(fd, &mut buf) {
            Ok(0) => bail!("stdin closed"),
            Ok(_) => return Ok(buf[0]),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("read key"),
        }
    }
}

fn read_byte_timeout(fd: i32, timeout_ms: u16) -> Result<Option<u8>> {
    let mut pfd = [PollFd::new(
        unsafe { BorrowedFd::borrow_raw(fd) },
        PollFlags::POLLIN,
    )];
    loop {
        match poll(&mut pfd, timeout_ms) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll stdin"),
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
        let _ = write!(io::stderr(), "{CSI_SHOW_CURSOR}");
        let _ = io::stderr().flush();
    }
}

fn make_raw(termios: &mut Termios) {
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
        // From create-new, cannot land on attached via next.
        assert_eq!(next_selectable(&entries, 0), Some(1));
    }
}
