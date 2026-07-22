//! Rolling session context for `reshell context`.
//!
//! Captures a line-oriented history of primary-screen PTY output and the last
//! command line when VS Code/Cursor OSC 633 shell-integration markers are
//! present. This is a read-only history snapshot — it is never replayed onto
//! the live PTY or attach path.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Default number of trailing output lines retained for context queries.
pub const DEFAULT_CONTEXT_LINES: usize = 100;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub name: String,
    pub last_command: Option<String>,
    pub last_exit_code: Option<i32>,
    pub lines: Vec<String>,
    /// True when the session is currently in the alternate screen (TUI).
    pub alt_screen: bool,
}

#[derive(Debug)]
pub struct SessionContext {
    lines: VecDeque<String>,
    max_lines: usize,
    last_command: Option<String>,
    last_exit_code: Option<i32>,
    line_acc: String,
    utf8_pending: Vec<u8>,
    strip: StripState,
    osc: Osc633State,
}

impl SessionContext {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines: max_lines.max(1),
            last_command: None,
            last_exit_code: None,
            line_acc: String::new(),
            utf8_pending: Vec::new(),
            strip: StripState::Ground,
            osc: Osc633State::default(),
        }
    }

    /// Feed raw PTY bytes. When `alt_screen` is set, line capture pauses so a
    /// full-screen app does not wipe shell history; OSC 633 command markers are
    /// still observed if present.
    pub fn feed(&mut self, data: &[u8], alt_screen: bool) {
        for &b in data {
            if let Some(event) = self.osc.feed(b) {
                match event {
                    Osc633Event::Command { cmdline } => {
                        self.last_command = Some(cmdline);
                        self.last_exit_code = None;
                    }
                    Osc633Event::Finished { exit_code } => {
                        self.last_exit_code = exit_code;
                    }
                }
            }
            if !alt_screen {
                self.feed_display_byte(b);
            }
        }
    }

    pub fn snapshot(&self, name: &str, alt_screen: bool) -> ContextSnapshot {
        ContextSnapshot {
            name: name.to_string(),
            last_command: self.last_command.clone(),
            last_exit_code: self.last_exit_code,
            lines: self.lines.iter().cloned().collect(),
            alt_screen,
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
                // CSI ends on a final byte 0x40–0x7e.
                if (0x40..=0x7e).contains(&b) {
                    self.strip = StripState::Ground;
                }
            }
            StripState::StringSeq { ref mut esc } => {
                if *esc {
                    if b == b'\\' || b == ESC {
                        self.strip = StripState::Ground;
                    } else {
                        *esc = false;
                    }
                } else if b == BEL {
                    self.strip = StripState::Ground;
                } else if b == ESC {
                    *esc = true;
                }
            }
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
                // Invalid sequence — drop the pending bytes.
                self.utf8_pending.clear();
            }
            Err(_) => {
                // Incomplete; wait for more bytes (cap runaway).
                if self.utf8_pending.len() >= 4 {
                    self.utf8_pending.clear();
                }
            }
        }
    }

    fn finish_line(&mut self) {
        let line = std::mem::take(&mut self.line_acc);
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripState {
    Ground,
    Esc,
    Csi,
    StringSeq { esc: bool },
}

#[derive(Debug, Default)]
struct Osc633State {
    phase: OscPhase,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OscPhase {
    #[default]
    Ground,
    Esc,
    /// Collecting OSC body after `ESC ]`.
    Body,
    BodyEsc,
}

#[derive(Debug)]
enum Osc633Event {
    Command { cmdline: String },
    Finished { exit_code: Option<i32> },
}

impl Osc633State {
    fn feed(&mut self, b: u8) -> Option<Osc633Event> {
        match self.phase {
            OscPhase::Ground => {
                if b == ESC {
                    self.phase = OscPhase::Esc;
                }
                None
            }
            OscPhase::Esc => {
                if b == b']' {
                    self.phase = OscPhase::Body;
                    self.buf.clear();
                } else {
                    self.phase = OscPhase::Ground;
                }
                None
            }
            OscPhase::Body => {
                if b == BEL {
                    self.phase = OscPhase::Ground;
                    return self.finish_body();
                }
                if b == ESC {
                    self.phase = OscPhase::BodyEsc;
                    return None;
                }
                if self.buf.len() < 8192 {
                    self.buf.push(b);
                }
                None
            }
            OscPhase::BodyEsc => {
                if b == b'\\' {
                    self.phase = OscPhase::Ground;
                    return self.finish_body();
                }
                // Nested ESC — treat as start of a new sequence.
                self.buf.clear();
                if b == b']' {
                    self.phase = OscPhase::Body;
                } else if b == ESC {
                    self.phase = OscPhase::Esc;
                } else {
                    self.phase = OscPhase::Ground;
                }
                None
            }
        }
    }

    fn finish_body(&mut self) -> Option<Osc633Event> {
        let body = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        parse_osc_633_body(&body)
    }
}

fn parse_osc_633_body(body: &str) -> Option<Osc633Event> {
    // Forms: `633;E;<cmdline>`, `633;E;<cmdline>;<nonce>`, `633;D`, `633;D;<code>`
    let mut parts = body.splitn(3, ';');
    let code = parts.next()?;
    if code != "633" {
        return None;
    }
    let kind = parts.next()?;
    match kind {
        "E" => {
            let rest = parts.next().unwrap_or("");
            // Command may contain `;` — only strip a trailing nonce if it looks
            // like the VS Code form (last field is alphanumeric nonce). For
            // simplicity take everything after `633;E;` and drop a trailing
            // `;hexnonce` of length <= 32 with no spaces when multiple fields.
            let cmdline = trim_osc_633_cmdline(rest);
            if cmdline.is_empty() {
                None
            } else {
                Some(Osc633Event::Command {
                    cmdline: cmdline.to_string(),
                })
            }
        }
        "D" => {
            let rest = parts.next().unwrap_or("").trim();
            let exit_code = if rest.is_empty() {
                None
            } else {
                rest.parse::<i32>().ok()
            };
            Some(Osc633Event::Finished { exit_code })
        }
        _ => None,
    }
}

fn trim_osc_633_cmdline(rest: &str) -> &str {
    if rest.is_empty() {
        return rest;
    }
    // `cmdline;nonce` — nonce is typically hex without spaces.
    if let Some((cmd, nonce)) = rest.rsplit_once(';') {
        if !nonce.is_empty()
            && nonce.len() <= 32
            && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && !cmd.is_empty()
        {
            return cmd;
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_lines_and_drops_csi() {
        let mut ctx = SessionContext::new(3);
        ctx.feed(b"hello\x1b[31mred\x1b[0m\nworld\n", false);
        ctx.feed(b"third\nfourth\n", false);
        let snap = ctx.snapshot("demo", false);
        assert_eq!(snap.lines, vec!["world", "third", "fourth"]);
    }

    #[test]
    fn pauses_lines_on_alt_screen() {
        let mut ctx = SessionContext::new(10);
        ctx.feed(b"keep\n", false);
        ctx.feed(b"tui-junk\n", true);
        let snap = ctx.snapshot("demo", true);
        assert_eq!(snap.lines, vec!["keep"]);
        assert!(snap.alt_screen);
    }

    #[test]
    fn parses_osc_633_command_and_exit() {
        let mut ctx = SessionContext::new(10);
        ctx.feed(b"\x1b]633;E;cargo test;abcd\x07", false);
        ctx.feed(b"ok\n", false);
        ctx.feed(b"\x1b]633;D;0\x07", false);
        let snap = ctx.snapshot("demo", false);
        assert_eq!(snap.last_command.as_deref(), Some("cargo test"));
        assert_eq!(snap.last_exit_code, Some(0));
        assert_eq!(snap.lines, vec!["ok"]);
    }
}
