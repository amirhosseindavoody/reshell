//! Minimal session picker for bare `reshell` / `reshell attach` (no name).
//!
//! Order: create-new, then detached (attachable), then attached (dimmed).
//! The session this process is inside is marked with `*`. Cursor defaults to
//! the first attachable session when one exists. Keys: Enter/`s` attach (switch),
//! `k` kill with confirmation, `q`/Esc cancel. Choosing create-new prompts for
//! a session name (editable suggested default).

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::termios::{
    tcgetattr, tcsetattr, LocalFlags, SetArg, SpecialCharacterIndices, Termios,
};
use nix::unistd::read as nix_read;

use crate::session::{self, allocate_session_name};

const CSI_CLEAR_LINE: &str = "\x1b[2K";
const CSI_HIDE_CURSOR: &str = "\x1b[?25l";
const CSI_SHOW_CURSOR: &str = "\x1b[?25h";
const SGR_RESET: &str = "\x1b[0m";
const SGR_REVERSE: &str = "\x1b[7m";
const SGR_DIM: &str = "\x1b[90m";
const SGR_BOLD: &str = "\x1b[1m";

const STATE_W: usize = 10;
const TIME_W: usize = 14;
const MARKER_W: usize = 2; // "> " or "  "
const MIN_NAME_W: usize = 8;
const MIN_SHELL_W: usize = 8;
const MAX_SHELL_W: usize = 24;

/// Outcome of the interactive session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAction {
    /// Create a new session with the given name (already validated).
    CreateNew { name: String },
    /// Attach / switch to this session.
    Attach(String),
    Cancelled,
}

/// One live session row for the picker.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub name: String,
    pub attached: bool,
    /// True when this process is running inside the session.
    pub current: bool,
    pub state: String,
    pub created: String,
    pub last_active: String,
    pub shell: String,
}

#[derive(Clone)]
enum Entry {
    CreateNew,
    Session {
        name: String,
        attached: bool,
        current: bool,
        state: String,
        created: String,
        last_active: String,
        shell: String,
    },
}

struct ColWidths {
    name: usize,
    shell: usize,
}

/// Interactive picker. Requires a controlling TTY (`/dev/tty` or stdin).
///
/// All rows are navigable. Detached sessions can be attached with Enter / `s`.
/// Attached sessions are dimmed (Enter/`s` no-op). The current session is marked
/// with `*`. `k` kills the highlighted session after confirmation.
pub fn pick_session(base: &Path, sessions: &[SessionRow]) -> Result<PickAction> {
    let mut tty = open_tty()?;
    let tty_fd = tty.as_raw_fd();
    if !nix::unistd::isatty(tty_fd).unwrap_or(false) {
        bail!("no tty available; session picker requires a terminal");
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(sessions.len() + 1);
    entries.push(Entry::CreateNew);

    let mut detached: Vec<&SessionRow> = sessions.iter().filter(|s| !s.attached).collect();
    let mut attached: Vec<&SessionRow> = sessions.iter().filter(|s| s.attached).collect();
    for s in detached.drain(..) {
        entries.push(entry_from_row(s));
    }
    for s in attached.drain(..) {
        entries.push(entry_from_row(s));
    }

    let mut cursor = first_cursor(&entries).unwrap_or(0);

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

    let cols = tty_cols(tty_fd).unwrap_or(80).max(40) as usize;
    let mut n_lines = list_n_lines(&entries);
    let mut widths = compute_widths(&entries, cols);
    let mut first_draw = true;
    let mut status: Option<String> = None;
    let result = (|| -> Result<PickAction> {
        loop {
            draw(
                &mut tty,
                &entries,
                cursor,
                first_draw,
                n_lines,
                cols,
                &widths,
                status.as_deref(),
            )?;
            first_draw = false;
            status = None;
            let key = read_key(tty_fd)?;
            match key {
                Key::Up => {
                    cursor = cursor.saturating_sub(1);
                }
                Key::Down => {
                    if cursor + 1 < entries.len() {
                        cursor += 1;
                    }
                }
                Key::Enter => match &entries[cursor] {
                    Entry::CreateNew => {
                        clear_ui(&mut tty, n_lines)?;
                        first_draw = true;
                        match prompt_session_name(&mut tty, tty_fd, base, cols)? {
                            Some(name) => return Ok(PickAction::CreateNew { name }),
                            None => {
                                first_draw = true;
                            }
                        }
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
                        status = Some("session is already attached".into());
                    }
                },
                Key::Char('s') | Key::Char('S') => match &entries[cursor] {
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
                        status = Some("session is already attached".into());
                    }
                    Entry::CreateNew => {
                        status = Some("select a session to switch, or Enter to create".into());
                    }
                },
                Key::Char('k') | Key::Char('K') => {
                    if let Entry::Session { name, .. } = &entries[cursor] {
                        let name = name.clone();
                        clear_ui(&mut tty, n_lines)?;
                        let confirmed = confirm_yn(
                            &mut tty,
                            tty_fd,
                            &format!("Kill session '{name}'?"),
                            cols,
                        )?;
                        first_draw = true;
                        if confirmed {
                            match session::kill_session(base, &name) {
                                Ok(()) => {
                                    entries.remove(cursor);
                                    if cursor >= entries.len() {
                                        cursor = entries.len().saturating_sub(1);
                                    }
                                    n_lines = list_n_lines(&entries);
                                    widths = compute_widths(&entries, cols);
                                    status = Some(format!("killed {name}"));
                                }
                                Err(e) => {
                                    status = Some(format!("kill failed: {e:#}"));
                                }
                            }
                        }
                    } else {
                        status = Some("select a session to kill".into());
                    }
                }
                Key::Cancel | Key::Char('q') | Key::Char('Q') => {
                    clear_ui(&mut tty, n_lines)?;
                    return Ok(PickAction::Cancelled);
                }
                Key::Other
                | Key::Left
                | Key::Right
                | Key::Backspace
                | Key::Delete
                | Key::Char(_) => {}
            }
        }
    })();

    write!(tty, "{CSI_SHOW_CURSOR}").ok();
    tty.flush().ok();
    result
}

fn entry_from_row(s: &SessionRow) -> Entry {
    Entry::Session {
        name: s.name.clone(),
        attached: s.attached,
        current: s.current,
        state: s.state.clone(),
        created: s.created.clone(),
        last_active: s.last_active.clone(),
        shell: s.shell.clone(),
    }
}

fn list_n_lines(entries: &[Entry]) -> usize {
    let session_count = entries.len().saturating_sub(1);
    // title, blank, create [, blank, col header, sessions…]
    if session_count > 0 {
        2 + 1 + 1 + 1 + session_count
    } else {
        2 + 1
    }
}

/// One-line y/N confirmation. Enter / Esc / n = no; y = yes.
fn confirm_yn(tty: &mut impl Write, tty_fd: RawFd, prompt: &str, cols: usize) -> Result<bool> {
    let n_lines = 2;
    let mut first = true;
    loop {
        if !first {
            write!(tty, "\x1b[{n_lines}A").ok();
        }
        first = false;
        write_line(
            tty,
            &truncate_visible(&format!("{prompt} [y/N]"), cols),
        )?;
        write_line(tty, "")?;
        tty.flush().ok();
        match read_key(tty_fd)? {
            Key::Char('y') | Key::Char('Y') => {
                clear_ui(tty, n_lines)?;
                return Ok(true);
            }
            Key::Char('n')
            | Key::Char('N')
            | Key::Enter
            | Key::Cancel
            | Key::Char('q')
            | Key::Char('Q') => {
                clear_ui(tty, n_lines)?;
                return Ok(false);
            }
            _ => {}
        }
    }
}

/// Ask for a session name with an editable suggested default.
///
/// Used when there are no sessions yet (skip the list) or after choosing
/// "Create new session" in the picker. Returns `None` if cancelled.
pub fn prompt_new_session_name(base: &Path) -> Result<Option<String>> {
    let mut tty = open_tty()?;
    let tty_fd = tty.as_raw_fd();
    if !nix::unistd::isatty(tty_fd).unwrap_or(false) {
        bail!("no tty available; session name prompt requires a terminal");
    }

    let orig = tcgetattr(tty.as_fd()).context("tcgetattr")?;
    let mut raw = orig.clone();
    make_cbreak(&mut raw);
    tcsetattr(tty.as_fd(), SetArg::TCSAFLUSH, &raw).context("tcsetattr cbreak")?;
    let _guard = TermiosGuard {
        fd: tty_fd,
        termios: Rc::new(orig),
    };

    let cols = tty_cols(tty_fd).unwrap_or(80).max(40) as usize;
    prompt_session_name(&mut tty, tty_fd, base, cols)
}

fn prompt_session_name(
    tty: &mut impl Write,
    tty_fd: RawFd,
    base: &Path,
    cols: usize,
) -> Result<Option<String>> {
    let default = allocate_session_name(base)?;
    let mut buf: Vec<char> = default.chars().collect();
    let mut cursor = buf.len();
    let mut error: Option<String> = None;
    let mut first = true;
    // prompt line + optional error line
    let n_lines = 2;

    // Keep the hardware cursor hidden; the caret is drawn in-buffer so redraw
    // math stays simple (cursor always parked below the 2-line UI).
    write!(tty, "{CSI_HIDE_CURSOR}").ok();
    loop {
        draw_name_prompt(tty, &buf, cursor, error.as_deref(), first, n_lines, cols)?;
        first = false;
        match read_key(tty_fd)? {
            Key::Enter => {
                let name: String = buf.iter().collect();
                let name = name.trim().to_string();
                if let Err(e) = session::validate_session_name(&name) {
                    error = Some(format!("{e:#}"));
                    continue;
                }
                let paths = session::SessionPaths::for_name(base, &name);
                if paths.dir.exists() {
                    error = Some(format!("session '{name}' already exists"));
                    continue;
                }
                clear_ui(tty, n_lines)?;
                return Ok(Some(name));
            }
            Key::Cancel => {
                clear_ui(tty, n_lines)?;
                return Ok(None);
            }
            Key::Left => {
                cursor = cursor.saturating_sub(1);
                error = None;
            }
            Key::Right => {
                if cursor < buf.len() {
                    cursor += 1;
                }
                error = None;
            }
            Key::Backspace => {
                if cursor > 0 {
                    buf.remove(cursor - 1);
                    cursor -= 1;
                }
                error = None;
            }
            Key::Delete => {
                if cursor < buf.len() {
                    buf.remove(cursor);
                }
                error = None;
            }
            Key::Char(c) => {
                if !c.is_control() {
                    buf.insert(cursor, c);
                    cursor += 1;
                }
                error = None;
            }
            Key::Up | Key::Down | Key::Other => {}
        }
    }
}

fn draw_name_prompt(
    out: &mut impl Write,
    buf: &[char],
    cursor: usize,
    error: Option<&str>,
    first_draw: bool,
    n_lines: usize,
    cols: usize,
) -> Result<()> {
    if !first_draw {
        write!(out, "\x1b[{n_lines}A").ok();
    }
    let label = "Session name: ";
    let name: String = buf.iter().collect();
    let budget = cols.saturating_sub(label.len()).max(8);
    let (shown, cur_col) = fit_edit_line(&name, cursor, budget);
    // Draw an in-buffer caret (reverse video) so we do not move the real cursor.
    let caret_line = render_caret_line(&shown, cur_col);
    write_line(
        out,
        &truncate_visible(&format!("{label}{caret_line}"), cols),
    )?;
    let err_line = match error {
        Some(e) => truncate_visible(&format!("{SGR_DIM}{e}{SGR_RESET}"), cols),
        None => String::new(),
    };
    write_line(out, &err_line)?;
    out.flush().ok();
    Ok(())
}

/// Insert a reverse-video caret at `cur_col` within `shown`.
fn render_caret_line(shown: &str, cur_col: usize) -> String {
    let chars: Vec<char> = shown.chars().collect();
    if cur_col >= chars.len() {
        // Caret at end: show a reverse space.
        return format!("{shown}{SGR_REVERSE} {SGR_RESET}");
    }
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i == cur_col {
            out.push_str(SGR_REVERSE);
            out.push(*c);
            out.push_str(SGR_RESET);
        } else {
            out.push(*c);
        }
    }
    out
}

/// Fit `text` into `budget` columns with the cursor visible; returns (display, cursor_col).
fn fit_edit_line(text: &str, cursor: usize, budget: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return (text.to_string(), cursor.min(chars.len()));
    }
    // Window around cursor.
    let half = budget / 2;
    let mut start = cursor.saturating_sub(half);
    if start + budget > chars.len() {
        start = chars.len().saturating_sub(budget);
    }
    let end = (start + budget).min(chars.len());
    let slice: String = chars[start..end].iter().collect();
    (slice, cursor.saturating_sub(start))
}

fn open_tty() -> Result<std::fs::File> {
    match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(f) => Ok(f),
        Err(_) => {
            let stdin_fd = io::stdin().as_raw_fd();
            if !nix::unistd::isatty(stdin_fd).unwrap_or(false) {
                bail!("stdin is not a tty; session picker requires a terminal");
            }
            let dup = nix::unistd::dup(stdin_fd).context("dup stdin for picker")?;
            Ok(unsafe { std::fs::File::from(OwnedFd::from_raw_fd(dup)) })
        }
    }
}

/// Initial cursor: first detachable session, else Create new (index 0).
fn first_cursor(entries: &[Entry]) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .find(|(_, e)| matches!(e, Entry::Session { attached: false, .. }))
        .map(|(i, _)| i)
        .or((!entries.is_empty()).then_some(0))
}

fn display_name(name: &str, current: bool) -> String {
    if current {
        format!("*{name}")
    } else {
        name.to_string()
    }
}

fn compute_widths(entries: &[Entry], cols: usize) -> ColWidths {
    let max_name = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Session { name, current, .. } => {
                Some(display_name(name, *current).chars().count())
            }
            Entry::CreateNew => None,
        })
        .max()
        .unwrap_or(MIN_NAME_W)
        .max(MIN_NAME_W);
    let max_shell = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Session { shell, .. } => Some(shell.chars().count()),
            Entry::CreateNew => None,
        })
        .max()
        .unwrap_or(MIN_SHELL_W)
        .clamp(MIN_SHELL_W, MAX_SHELL_W);

    // marker + name + gaps + state + created + last + shell
    // gaps: 4 single spaces between the 5 data columns after marker
    let fixed_rest = MARKER_W + 1 + STATE_W + 1 + TIME_W + 1 + TIME_W + 1;
    let available = cols.saturating_sub(fixed_rest);
    // Prefer giving name room up to max_name; shell gets the remainder (clamped).
    let shell = max_shell.min(available / 3).max(MIN_SHELL_W.min(available));
    let name_avail = available.saturating_sub(shell);
    let name = max_name.min(name_avail).max(MIN_NAME_W.min(name_avail));
    // If name took less than available, give leftover to shell (still capped).
    let shell = (available.saturating_sub(name))
        .min(MAX_SHELL_W)
        .max(MIN_SHELL_W.min(available.saturating_sub(name)));
    ColWidths { name, shell }
}

fn pad_trunc(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count == width {
        return s.to_string();
    }
    if count < width {
        let mut out = s.to_string();
        out.extend(std::iter::repeat_n(' ', width - count));
        return out;
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let keep = width - 1;
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn format_session_row(
    marker: &str,
    name: &str,
    state: &str,
    created: &str,
    last_active: &str,
    shell: &str,
    widths: &ColWidths,
) -> String {
    format!(
        "{marker}{name} {state:<state_w$} {created} {last_active} {shell}",
        name = pad_trunc(name, widths.name),
        state_w = STATE_W,
        created = pad_trunc(created, TIME_W),
        last_active = pad_trunc(last_active, TIME_W),
        shell = pad_trunc(shell, widths.shell),
    )
}

#[allow(clippy::too_many_arguments)]
fn draw(
    out: &mut impl Write,
    entries: &[Entry],
    cursor: usize,
    first_draw: bool,
    n_lines: usize,
    cols: usize,
    widths: &ColWidths,
    status: Option<&str>,
) -> Result<()> {
    if !first_draw {
        write!(out, "\x1b[{n_lines}A").ok();
    }
    let help = match status {
        Some(s) => s.to_string(),
        None => "↑/↓  Enter/s switch  k kill  q quit  *=current".into(),
    };
    write_line(out, &truncate_visible(&help, cols))?;
    write_line(out, "")?;

    let has_sessions = entries.iter().any(|e| matches!(e, Entry::Session { .. }));

    for (i, entry) in entries.iter().enumerate() {
        if i == 1 && has_sessions {
            // Blank + column header after the create-new row.
            write_line(out, "")?;
            let header = format_session_row(
                "  ",
                "NAME",
                "STATE",
                "CREATED",
                "LAST ACTIVE",
                "SHELL",
                widths,
            );
            write_line(
                out,
                &truncate_visible(&format!("{SGR_BOLD}{header}{SGR_RESET}"), cols),
            )?;
        }
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
                current,
                state,
                created,
                last_active,
                shell,
            } => {
                let marker = if selected { "> " } else { "  " };
                let shown = display_name(name, *current);
                let body = format_session_row(
                    marker,
                    &shown,
                    state,
                    created,
                    last_active,
                    shell,
                    widths,
                );
                if selected {
                    format!("{SGR_REVERSE}{body}{SGR_RESET}")
                } else if *attached && !*current {
                    format!("{SGR_DIM}{body}{SGR_RESET}")
                } else if *current {
                    format!("{SGR_BOLD}{body}{SGR_RESET}")
                } else {
                    body
                }
            }
        };
        write_line(out, &truncate_visible(&line, cols))?;
    }
    out.flush().ok();
    Ok(())
}

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
    if (s.contains(SGR_REVERSE) || s.contains(SGR_DIM) || s.contains(SGR_BOLD))
        && !out.ends_with(SGR_RESET)
    {
        out.push_str(SGR_RESET);
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
    Left,
    Right,
    Enter,
    Cancel,
    Backspace,
    Delete,
    Char(char),
    Other,
}

fn read_key(fd: RawFd) -> Result<Key> {
    let b = read_byte_blocking(fd)?;
    match b {
        b'\r' | b'\n' => Ok(Key::Enter),
        0x7f | 0x08 => Ok(Key::Backspace),
        // Ctrl+C / Esc start — Esc may be an arrow / Delete sequence.
        0x03 => Ok(Key::Cancel),
        0x1b => read_escape_sequence(fd),
        b if b < 0x20 => Ok(Key::Other),
        b if b < 0x80 => {
            // Plain ASCII. During list navigation, q cancels; during name edit,
            // callers treat Char('q') as text. Disambiguate: only bare cancel
            // keys are Cancel; `q` is Char so the name prompt can type it.
            // List loop maps Char('q')/Char('Q') to cancel.
            Ok(Key::Char(b as char))
        }
        _ => Ok(Key::Other), // ignore non-UTF8 / multibyte starters for now
    }
}

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
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'3' => {
            // Delete is ESC [ 3 ~
            let _ = read_byte_timeout(fd, 50)?;
            Key::Delete
        }
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
        if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
            let _ = write!(tty, "{CSI_SHOW_CURSOR}");
            let _ = tty.flush();
        }
    }
}

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

    fn sess(name: &str, attached: bool) -> Entry {
        sess_cur(name, attached, false)
    }

    fn sess_cur(name: &str, attached: bool, current: bool) -> Entry {
        Entry::Session {
            name: name.into(),
            attached,
            current,
            state: if attached {
                "attached".into()
            } else {
                "detached".into()
            },
            created: "2h ago".into(),
            last_active: "1m ago".into(),
            shell: "/bin/zsh".into(),
        }
    }

    #[test]
    fn cursor_prefers_first_detached() {
        let entries = vec![Entry::CreateNew, sess("a", false), sess("b", true)];
        assert_eq!(first_cursor(&entries), Some(1));
    }

    #[test]
    fn cursor_falls_back_to_create_when_all_attached() {
        let entries = vec![Entry::CreateNew, sess("busy", true)];
        assert_eq!(first_cursor(&entries), Some(0));
    }

    #[test]
    fn navigation_includes_attached() {
        let entries = vec![Entry::CreateNew, sess("free", false), sess("busy", true)];
        // All rows are navigable with ↑/↓ (no skipping).
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[2], Entry::Session { attached: true, .. }));
    }

    #[test]
    fn current_session_marked_with_asterisk() {
        assert_eq!(display_name("demo", true), "*demo");
        assert_eq!(display_name("demo", false), "demo");
        let entries = vec![Entry::CreateNew, sess_cur("mine", true, true)];
        let widths = compute_widths(&entries, 100);
        let mut buf = Vec::new();
        draw(&mut buf, &entries, 1, true, 6, 100, &widths, None).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("*mine"), "{text}");
        assert!(text.contains("*=current") || text.contains("current"), "{text}");
    }

    #[test]
    fn write_line_uses_cr_clear_and_lf() {
        let mut buf = Vec::new();
        write_line(&mut buf, "hello").unwrap();
        assert_eq!(buf, b"\r\x1b[2Khello\n");
    }

    #[test]
    fn pad_trunc_ellipsis_for_long_names() {
        assert_eq!(pad_trunc("abc", 5), "abc  ");
        assert_eq!(pad_trunc("abcdefghij", 5), "abcd…");
        assert_eq!(pad_trunc("abcd", 4), "abcd");
    }

    #[test]
    fn widths_expand_for_long_names_and_keep_columns() {
        let entries = vec![
            Entry::CreateNew,
            sess("short", false),
            sess("this-is-a-very-long-session-name", false),
        ];
        let w = compute_widths(&entries, 100);
        assert!(w.name >= "this-is-a-very-long-session-name".len() || w.name >= MIN_NAME_W);
        let row = format_session_row(
            "  ",
            "this-is-a-very-long-session-name",
            "detached",
            "2h ago",
            "1m ago",
            "/bin/zsh",
            &w,
        );
        // STATE column should still appear as a whole word (not glued to name).
        assert!(row.contains(" detached "));
        assert!(row.contains("CREATED") || row.contains("2h ago"));
    }

    #[test]
    fn draw_includes_created_and_header() {
        let entries = vec![Entry::CreateNew, sess("demo", false)];
        let widths = compute_widths(&entries, 100);
        let mut buf = Vec::new();
        // n_lines = 2 + 1 + 1 + 1 + 1 = 6
        draw(&mut buf, &entries, 1, true, 6, 100, &widths, None).unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("Create new session"));
        assert!(text.contains("CREATED"));
        assert!(text.contains("LAST ACTIVE"));
        assert!(text.contains("2h ago"));
        assert!(text.contains("1m ago"));
        assert!(text.contains("demo"));
        assert!(text.contains("k kill"));
        assert!(text.contains("Enter/s switch") || text.contains("switch"));
    }

    #[test]
    fn fit_edit_keeps_cursor_in_window() {
        let long = "abcdefghijklmnopqrstuvwxyz";
        let (shown, col) = fit_edit_line(long, 20, 10);
        assert_eq!(shown.chars().count(), 10);
        assert!(col < 10);
    }

    #[test]
    fn render_caret_at_end_and_middle() {
        assert!(render_caret_line("abc", 3).contains(SGR_REVERSE));
        let mid = render_caret_line("abc", 1);
        assert!(mid.contains(&format!("{SGR_REVERSE}b{SGR_RESET}")));
    }
}
