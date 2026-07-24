//! On-disk session history as rotating text files.
//!
//! Primary-screen PTY output is turned into UTF-8 lines under
//! `$session/history/NNNN.txt`. Capture is **line-oriented**, not a VT screen
//! buffer: a current-line cursor model collapses interactive shell redraws
//! (`\r`, backspace, same-line CSI erase/cursor moves) so logs look closer to
//! what appeared on the terminal. Other CSI/OSC is stripped. Capture pauses
//! while the session is on the alternate screen (full-screen TUIs).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::session::SessionPaths;

/// Soft cap on body lines per history file (excluding marker lines).
pub const HISTORY_LINES_PER_FILE: usize = 2000;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const BS: u8 = 0x08;
const TAB: u8 = b'\t';
const TAB_STOP: usize = 8;

/// Append-only history writer owned by the session daemon.
pub struct HistoryWriter {
    session_name: String,
    history_dir: PathBuf,
    file_index: u32,
    lines_in_file: usize,
    file: Option<File>,
    /// Characters of the current (uncommitted) line.
    line: Vec<char>,
    /// 0-based column into `line` (may be past `line.len()` for padding).
    col: usize,
    utf8_pending: Vec<u8>,
    parser: Parser,
}

impl HistoryWriter {
    /// Create `$session/history/` and open `0001.txt` with a begin marker.
    pub fn open(paths: &SessionPaths, session_name: &str) -> Result<Self> {
        let history_dir = paths.history_dir();
        fs::create_dir_all(&history_dir).with_context(|| {
            format!("create history dir {}", history_dir.display())
        })?;
        let mut w = Self {
            session_name: session_name.to_string(),
            history_dir,
            file_index: 0,
            lines_in_file: 0,
            file: None,
            line: Vec::new(),
            col: 0,
            utf8_pending: Vec::new(),
            parser: Parser::Ground,
        };
        w.open_next_file(/*first=*/ true)?;
        Ok(w)
    }

    /// Feed raw PTY bytes. When `alt_screen` is set, line capture pauses so a
    /// full-screen app does not pollute the text history.
    pub fn feed(&mut self, data: &[u8], alt_screen: bool) {
        if alt_screen {
            // Drop any half-built primary-screen line if a TUI took over.
            self.reset_line();
            self.utf8_pending.clear();
            self.parser = Parser::Ground;
            return;
        }
        for &b in data {
            self.feed_byte(b);
        }
    }

    /// Flush a partial line and write a session-closed marker.
    pub fn finish(&mut self) {
        if self.file.is_none() {
            return;
        }
        self.flush_partial_line();
        if let Some(mut f) = self.file.take() {
            let _ = writeln!(
                f,
                "*** reshell history: session={} file={} ended session closed ***",
                self.session_name, self.file_index
            );
            let _ = f.flush();
        }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.parser {
            Parser::Ground => match b {
                ESC => {
                    self.utf8_pending.clear();
                    self.parser = Parser::Esc;
                }
                b'\n' => {
                    self.utf8_pending.clear();
                    self.finish_line();
                }
                b'\r' => {
                    self.utf8_pending.clear();
                    self.col = 0;
                }
                BS => {
                    self.utf8_pending.clear();
                    self.col = self.col.saturating_sub(1);
                }
                // DEL: treat as backspace (common for interactive shells).
                0x7f => {
                    self.utf8_pending.clear();
                    self.col = self.col.saturating_sub(1);
                }
                TAB => {
                    self.utf8_pending.clear();
                    self.advance_tab();
                }
                c if (0x20..0x7f).contains(&c) => {
                    self.utf8_pending.clear();
                    self.put_char(c as char);
                }
                c if c >= 0x80 => self.push_utf8_byte(c),
                _ => self.utf8_pending.clear(),
            },
            Parser::Esc => match b {
                b'[' => {
                    self.parser = Parser::Csi {
                        private: false,
                        params: Params::default(),
                        intermediate: 0,
                    };
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    self.parser = Parser::StringSeq { esc: false };
                }
                _ => self.parser = Parser::Ground,
            },
            Parser::Csi {
                ref mut private,
                ref mut params,
                ref mut intermediate,
            } => match b {
                b'?' if !*private && params.len == 0 && !params.started && *intermediate == 0 => {
                    *private = true;
                }
                b'0'..=b'9' => params.push_digit(b),
                b';' => params.sep(),
                0x20..=0x2f => {
                    *intermediate = b;
                }
                0x40..=0x7e => {
                    let private = *private;
                    let mut params = *params;
                    let intermediate = *intermediate;
                    params.finish();
                    self.parser = Parser::Ground;
                    if !private && intermediate == 0 {
                        self.handle_same_line_csi(&params, b);
                    }
                }
                _ => {
                    self.parser = Parser::Ground;
                }
            },
            Parser::StringSeq { ref mut esc } => match b {
                BEL => self.parser = Parser::Ground,
                ESC => *esc = true,
                b'\\' if *esc => self.parser = Parser::Ground,
                _ => *esc = false,
            },
        }
    }

    /// Apply CSI that only affects the current line cursor / erase.
    /// Vertical motion and other sequences are ignored (already stripped).
    fn handle_same_line_csi(&mut self, params: &Params, final_byte: u8) {
        match final_byte {
            // Cursor Horizontal Absolute — 1-based column.
            b'G' => {
                let n = params.first_or(1).max(1) as usize;
                self.col = n - 1;
            }
            // Cursor Forward.
            b'C' => {
                let n = params.first_or(1) as usize;
                self.col = self.col.saturating_add(n);
            }
            // Cursor Back.
            b'D' => {
                let n = params.first_or(1) as usize;
                self.col = self.col.saturating_sub(n);
            }
            // Erase in Line.
            b'K' => match params.first_or(0) {
                0 => {
                    // Cursor to end of line.
                    if self.col < self.line.len() {
                        self.line.truncate(self.col);
                    }
                }
                1 => {
                    // Start of line through cursor.
                    let end = self.col.min(self.line.len());
                    for ch in &mut self.line[..end] {
                        *ch = ' ';
                    }
                }
                2 => {
                    // Entire line; cursor position unchanged.
                    self.line.clear();
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn put_char(&mut self, ch: char) {
        while self.line.len() < self.col {
            self.line.push(' ');
        }
        if self.col < self.line.len() {
            self.line[self.col] = ch;
        } else {
            self.line.push(ch);
        }
        self.col += 1;
    }

    fn advance_tab(&mut self) {
        let next = (self.col / TAB_STOP + 1) * TAB_STOP;
        while self.col < next {
            self.put_char(' ');
        }
    }

    fn reset_line(&mut self) {
        self.line.clear();
        self.col = 0;
    }

    fn push_utf8_byte(&mut self, b: u8) {
        self.utf8_pending.push(b);
        match std::str::from_utf8(&self.utf8_pending) {
            Ok(_) => {
                let s = String::from_utf8(std::mem::take(&mut self.utf8_pending)).unwrap();
                for ch in s.chars() {
                    self.put_char(ch);
                }
            }
            Err(e) if e.error_len().is_some() => {
                self.utf8_pending.clear();
            }
            Err(_) => {
                // Incomplete; wait for more bytes.
            }
        }
    }

    fn finish_line(&mut self) {
        let line: String = self.line.iter().collect();
        self.reset_line();
        if let Err(e) = self.write_body_line(&line) {
            let _ = e;
        }
    }

    fn flush_partial_line(&mut self) {
        if !self.line.is_empty() {
            self.finish_line();
        } else {
            self.reset_line();
        }
    }

    fn write_body_line(&mut self, line: &str) -> Result<()> {
        if self.lines_in_file >= HISTORY_LINES_PER_FILE {
            self.rotate()?;
        }
        if let Some(ref mut f) = self.file {
            writeln!(f, "{line}").context("write history line")?;
            self.lines_in_file += 1;
            let _ = f.flush();
        }
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        self.flush_partial_line();
        let next = self.file_index + 1;
        if let Some(ref mut f) = self.file {
            writeln!(
                f,
                "*** reshell history: session={} file={} ended continuing in file={} ***",
                self.session_name, self.file_index, next
            )
            .context("write history continue marker")?;
            let _ = f.flush();
        }
        self.file = None;
        self.open_next_file(/*first=*/ false)
    }

    fn open_next_file(&mut self, first: bool) -> Result<()> {
        self.file_index += 1;
        self.lines_in_file = 0;
        let path = history_file_path(&self.history_dir, self.file_index);
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("create history file {}", path.display()))?;
        let started = format_rfc3339_now();
        if first {
            writeln!(
                f,
                "*** reshell history: session={} file={} started={} beginning of session history ***",
                self.session_name, self.file_index, started
            )?;
        } else {
            let prev = self.file_index - 1;
            writeln!(
                f,
                "*** reshell history: session={} file={} started={} previous_file={} ***",
                self.session_name, self.file_index, started, prev
            )?;
        }
        let _ = f.flush();
        self.file = Some(f);
        Ok(())
    }
}

impl Drop for HistoryWriter {
    fn drop(&mut self) {
        if self.file.is_some() {
            self.finish();
        }
    }
}

fn history_file_path(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index:04}.txt"))
}

fn format_rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// List history files in order (`0001.txt`, `0002.txt`, …).
pub fn list_history_files(paths: &SessionPaths) -> Vec<PathBuf> {
    let dir = paths.history_dir();
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("txt")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    files.sort();
    files
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Parser {
    #[default]
    Ground,
    Esc,
    Csi {
        private: bool,
        params: Params,
        intermediate: u8,
    },
    StringSeq {
        esc: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Params {
    vals: [u16; 4],
    len: u8,
    started: bool,
}

impl Params {
    fn push_digit(&mut self, d: u8) {
        let i = self.len as usize;
        if i >= self.vals.len() {
            return;
        }
        self.vals[i] = self.vals[i]
            .saturating_mul(10)
            .saturating_add((d - b'0') as u16);
        self.started = true;
    }

    fn sep(&mut self) {
        if (self.len as usize) + 1 < self.vals.len() {
            self.len += 1;
            self.started = false;
        }
    }

    fn finish(&mut self) {
        if self.started || self.len == 0 {
            if (self.len as usize) < self.vals.len() {
                self.len += 1;
            }
        }
        self.started = false;
    }

    fn first_or(&self, default: u16) -> u16 {
        if self.len == 0 {
            return default;
        }
        let v = self.vals[0];
        if v == 0 {
            default
        } else {
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ensure_base_dir, SessionPaths};
    use tempfile::tempdir;

    fn open_hist(name: &str) -> (tempfile::TempDir, HistoryWriter, SessionPaths) {
        let dir = tempdir().unwrap();
        ensure_base_dir(dir.path()).unwrap();
        let paths = SessionPaths::for_name(dir.path(), name);
        fs::create_dir_all(&paths.dir).unwrap();
        let hist = HistoryWriter::open(&paths, name).unwrap();
        (dir, hist, paths)
    }

    fn body_lines(paths: &SessionPaths) -> String {
        let files = list_history_files(paths);
        fs::read_to_string(&files[0]).unwrap()
    }

    #[test]
    fn writes_lines_and_rotates() {
        let dir = tempdir().unwrap();
        ensure_base_dir(dir.path()).unwrap();
        let paths = SessionPaths::for_name(dir.path(), "demo");
        fs::create_dir_all(&paths.dir).unwrap();

        let mut hist = HistoryWriter::open(&paths, "demo").unwrap();
        for i in 0..HISTORY_LINES_PER_FILE {
            hist.feed(format!("line-{i}\n").as_bytes(), false);
        }
        hist.feed(b"overflow\n", false);
        hist.finish();

        let files = list_history_files(&paths);
        assert_eq!(files.len(), 2, "{files:?}");
        let first = fs::read_to_string(&files[0]).unwrap();
        assert!(first.contains("beginning of session history"), "{first}");
        assert!(first.contains("line-0"), "{first}");
        assert!(first.contains("continuing in file=2"), "{first}");
        let second = fs::read_to_string(&files[1]).unwrap();
        assert!(second.contains("previous_file=1"), "{second}");
        assert!(second.contains("overflow"), "{second}");
        assert!(second.contains("session closed"), "{second}");
    }

    #[test]
    fn skips_alt_screen_and_strips_csi() {
        let (_dir, mut hist, paths) = open_hist("tui");
        hist.feed(b"before\n", false);
        hist.feed(b"\x1b[31mred\x1b[0m\n", false);
        hist.feed(b"tui junk\n", true);
        hist.feed(b"after\n", false);
        hist.finish();
        let text = body_lines(&paths);
        assert!(text.contains("before"));
        assert!(text.contains("red"));
        assert!(!text.contains("tui junk"), "{text}");
        assert!(text.contains("after"));
        assert!(!text.contains("\x1b"), "{text:?}");
    }

    #[test]
    fn carriage_return_overwrites_line() {
        let (_dir, mut hist, paths) = open_hist("cr");
        // Interactive redraw: print "hello", return, overwrite with "hi".
        hist.feed(b"hello\rhi\n", false);
        hist.finish();
        let text = body_lines(&paths);
        assert!(
            text.lines().any(|l| l == "hillo"),
            "expected CR overwrite to yield 'hillo', got: {text}"
        );
        assert!(!text.contains("hello\n"), "{text}");
    }

    #[test]
    fn backspace_moves_cursor_for_overwrite() {
        let (_dir, mut hist, paths) = open_hist("bs");
        hist.feed(b"ab\x08x\n", false);
        hist.finish();
        let text = body_lines(&paths);
        assert!(
            text.lines().any(|l| l == "ax"),
            "expected backspace overwrite 'ax', got: {text}"
        );
    }

    #[test]
    fn erase_in_line_and_horizontal_cursor() {
        let (_dir, mut hist, paths) = open_hist("el");
        // Type "hello", CR, erase to EOL, write "world".
        hist.feed(b"hello\r\x1b[Kworld\n", false);
        // CHA: write "abc", move to column 1, write "Z" → "Zbc".
        hist.feed(b"abc\x1b[1GZ\n", false);
        // Cursor back then overwrite.
        hist.feed(b"xy\x1b[1D!\n", false);
        hist.finish();
        let text = body_lines(&paths);
        assert!(text.lines().any(|l| l == "world"), "{text}");
        assert!(text.lines().any(|l| l == "Zbc"), "{text}");
        assert!(text.lines().any(|l| l == "x!"), "{text}");
    }

    #[test]
    fn shell_style_prompt_redraw_collapses() {
        let (_dir, mut hist, paths) = open_hist("redraw");
        // Zsh-style: each keystroke redraws the whole prompt+command via CR+EL.
        let steps = ["w", "wh", "whi", "whic", "which"];
        for cmd in steps {
            hist.feed(b"\r\x1b[K% > ~ ", false);
            hist.feed(cmd.as_bytes(), false);
        }
        hist.feed(b"\n", false);
        hist.finish();
        let text = body_lines(&paths);
        assert!(
            text.lines().any(|l| l == "% > ~ which"),
            "expected collapsed prompt+command, got: {text}"
        );
        assert!(!text.contains("ww"), "{text}");
        assert!(!text.contains("whiwhic"), "{text}");
    }
}
