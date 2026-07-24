//! On-disk session history as rotating text files.
//!
//! Primary-screen PTY output is stripped of CSI/OSC control sequences and
//! appended as UTF-8 lines under `$session/history/NNNN.txt`. Capture pauses
//! while the session is on the alternate screen (full-screen TUIs). Local
//! client raw mode is unrelated — that is only how the attach client drives
//! the TTY.

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

/// Append-only history writer owned by the session daemon.
pub struct HistoryWriter {
    session_name: String,
    history_dir: PathBuf,
    file_index: u32,
    lines_in_file: usize,
    file: Option<File>,
    line_acc: String,
    utf8_pending: Vec<u8>,
    strip: StripState,
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
            line_acc: String::new(),
            utf8_pending: Vec::new(),
            strip: StripState::Ground,
        };
        w.open_next_file(/*first=*/ true)?;
        Ok(w)
    }

    /// Feed raw PTY bytes. When `alt_screen` is set, line capture pauses so a
    /// full-screen app does not pollute the text history.
    pub fn feed(&mut self, data: &[u8], alt_screen: bool) {
        if alt_screen {
            // Drop any half-built primary-screen line if a TUI took over.
            self.line_acc.clear();
            self.utf8_pending.clear();
            self.strip = StripState::Ground;
            return;
        }
        for &b in data {
            self.feed_display_byte(b);
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

    fn feed_display_byte(&mut self, b: u8) {
        match self.strip {
            StripState::Ground => match b {
                ESC => {
                    self.utf8_pending.clear();
                    self.strip = StripState::Esc;
                }
                b'\n' => {
                    self.utf8_pending.clear();
                    self.finish_line();
                }
                b'\r' => self.utf8_pending.clear(),
                b'\t' => {
                    self.utf8_pending.clear();
                    self.line_acc.push('\t');
                }
                c if (0x20..0x7f).contains(&c) => {
                    self.utf8_pending.clear();
                    self.line_acc.push(c as char);
                }
                c if c >= 0x80 => self.push_utf8_byte(c),
                _ => self.utf8_pending.clear(),
            },
            StripState::Esc => match b {
                b'[' => self.strip = StripState::Csi,
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    self.strip = StripState::StringSeq { esc: false };
                }
                _ => self.strip = StripState::Ground,
            },
            StripState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.strip = StripState::Ground;
                }
            }
            StripState::StringSeq { ref mut esc } => match b {
                BEL => self.strip = StripState::Ground,
                ESC => *esc = true,
                b'\\' if *esc => self.strip = StripState::Ground,
                _ => *esc = false,
            },
        }
    }

    fn push_utf8_byte(&mut self, b: u8) {
        self.utf8_pending.push(b);
        match std::str::from_utf8(&self.utf8_pending) {
            Ok(s) => {
                self.line_acc.push_str(s);
                self.utf8_pending.clear();
            }
            Err(e) if e.error_len().is_some() => {
                // Invalid sequence — drop it.
                self.utf8_pending.clear();
            }
            Err(_) => {
                // Incomplete; wait for more bytes.
            }
        }
    }

    fn finish_line(&mut self) {
        let line = std::mem::take(&mut self.line_acc);
        if let Err(e) = self.write_body_line(&line) {
            // Best-effort: history must not kill the daemon.
            let _ = e;
        }
    }

    fn flush_partial_line(&mut self) {
        if !self.line_acc.is_empty() {
            self.finish_line();
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
        // If finish() was not called (panic / early return), still try to seal.
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
    // Keep deps light: unix timestamp is enough for the marker; docs call it
    // time/date. Prefer a readable UTC-ish form via chrono-less formatting.
    // Format as unix seconds with a clear label if we lack a time crate.
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

#[derive(Debug)]
enum StripState {
    Ground,
    Esc,
    Csi,
    StringSeq { esc: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ensure_base_dir, SessionPaths};
    use tempfile::tempdir;

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
        // One more forces rotate.
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
        let dir = tempdir().unwrap();
        ensure_base_dir(dir.path()).unwrap();
        let paths = SessionPaths::for_name(dir.path(), "tui");
        fs::create_dir_all(&paths.dir).unwrap();
        let mut hist = HistoryWriter::open(&paths, "tui").unwrap();
        hist.feed(b"before\n", false);
        hist.feed(b"\x1b[31mred\x1b[0m\n", false);
        hist.feed(b"tui junk\n", true);
        hist.feed(b"after\n", false);
        hist.finish();
        let files = list_history_files(&paths);
        let text = fs::read_to_string(&files[0]).unwrap();
        assert!(text.contains("before"));
        assert!(text.contains("red"));
        assert!(!text.contains("tui junk"), "{text}");
        assert!(text.contains("after"));
        assert!(!text.contains("\x1b"), "{text:?}");
    }
}
