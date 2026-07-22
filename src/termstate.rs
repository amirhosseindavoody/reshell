//! Lightweight terminal mode tracker for reattach.
//!
//! Parses DEC private mode set/reset (and a few related CSI sequences) from PTY
//! output so that on attach we can re-enable mouse, alt-screen, etc. on the new
//! client TTY. This is not a full VT emulator — screen contents are not stored;
//! the child is expected to redraw after a forced `SIGWINCH`.

use std::collections::BTreeMap;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Modes we always try to leave clean on the local TTY when the client exits.
const CLIENT_CLEANUP_MODES: &[u16] = &[
    1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004, 1047, 1048, 1049,
    47,
];

#[derive(Debug, Clone, Default)]
pub struct TermState {
    /// Explicitly observed DEC private modes (`CSI ? Pn h/l`).
    modes: BTreeMap<u16, bool>,
    parser: Parser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Parser {
    #[default]
    Ground,
    Esc,
    Csi {
        private: bool,
        /// Accumulated numeric parameters (0 = empty/default for that slot).
        params: Params,
        intermediate: u8,
    },
    /// OSC / DCS / PM / APC: skip until BEL or ST (`ESC \`).
    StringSeq { esc: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Params {
    vals: [u16; 8],
    len: u8,
    /// Digits seen for the current parameter (false → still default/empty).
    started: bool,
}

impl Params {
    fn push_digit(&mut self, d: u8) {
        let i = self.len as usize;
        if i >= self.vals.len() {
            return;
        }
        let v = self.vals[i].saturating_mul(10).saturating_add((d - b'0') as u16);
        self.vals[i] = v;
        self.started = true;
    }

    fn sep(&mut self) {
        if self.len as usize + 1 < self.vals.len() {
            if !self.started {
                // empty param → leave 0
            }
            self.len += 1;
            self.started = false;
        }
    }

    fn finish(&mut self) {
        if self.started || self.len == 0 {
            // Include the last (possibly empty→0) parameter.
            if (self.len as usize) < self.vals.len() {
                self.len += 1;
            }
        }
        self.started = false;
    }

    fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.vals[..self.len as usize].iter().copied()
    }
}

impl TermState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed PTY output bytes (whether forwarded to a client or discarded).
    pub fn feed(&mut self, data: &[u8]) {
        for &b in data {
            self.feed_byte(b);
        }
    }

    fn feed_byte(&mut self, b: u8) {
        match self.parser {
            Parser::Ground => {
                if b == ESC {
                    self.parser = Parser::Esc;
                }
            }
            Parser::Esc => match b {
                b'[' => {
                    self.parser = Parser::Csi {
                        private: false,
                        params: Params::default(),
                        intermediate: 0,
                    };
                }
                b']' | b'P' | b'_' | b'^' => {
                    self.parser = Parser::StringSeq { esc: false };
                }
                _ => {
                    // Single-character ESC sequence (or unknown) — ignore.
                    self.parser = Parser::Ground;
                }
            },
            Parser::Csi {
                ref mut private,
                ref mut params,
                ref mut intermediate,
            } => {
                match b {
                    b'?' if !*private && params.len == 0 && !params.started && *intermediate == 0 => {
                        *private = true;
                    }
                    b'0'..=b'9' => params.push_digit(b),
                    b';' => params.sep(),
                    0x20..=0x2f => {
                        // Intermediate bytes (e.g. space before 'q').
                        *intermediate = b;
                    }
                    0x40..=0x7e => {
                        let private = *private;
                        let mut params = *params;
                        let intermediate = *intermediate;
                        params.finish();
                        self.parser = Parser::Ground;
                        self.handle_csi(private, &params, intermediate, b);
                    }
                    _ => {
                        // Cancelled / malformed.
                        self.parser = Parser::Ground;
                    }
                }
            }
            Parser::StringSeq { ref mut esc } => {
                if *esc {
                    // ST is ESC \ ; any other byte after ESC cancels the esc flag
                    // and may start a new ESC sequence.
                    if b == b'\\' {
                        self.parser = Parser::Ground;
                    } else if b == ESC {
                        // stay in string with esc=true? treat as new ESC inside — rare.
                        *esc = true;
                    } else {
                        *esc = false;
                    }
                } else if b == BEL {
                    self.parser = Parser::Ground;
                } else if b == ESC {
                    *esc = true;
                }
            }
        }
    }

    fn handle_csi(&mut self, private: bool, params: &Params, intermediate: u8, final_byte: u8) {
        if !private || intermediate != 0 {
            return;
        }
        match final_byte {
            b'h' => {
                for mode in params.iter() {
                    if mode != 0 {
                        self.modes.insert(mode, true);
                    }
                }
            }
            b'l' => {
                for mode in params.iter() {
                    if mode != 0 {
                        self.modes.insert(mode, false);
                    }
                }
            }
            _ => {}
        }
    }

    /// CSI sequences to replay onto a freshly attached client TTY.
    pub fn restore_sequence(&self) -> Vec<u8> {
        if self.modes.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(64);
        // Prefer a known-good order: alt-screen first, then cursor, mouse, paste/focus, rest.
        const PRIORITY: &[u16] = &[
            1049, 1047, 1048, 47, 25, 1000, 1001, 1002, 1003, 1005, 1006, 1015, 1016, 1004,
            2004,
        ];

        let mut emitted = std::collections::HashSet::new();
        for &mode in PRIORITY {
            if let Some(&on) = self.modes.get(&mode) {
                push_dec_mode(&mut out, mode, on);
                emitted.insert(mode);
            }
        }
        for (&mode, &on) in &self.modes {
            if !emitted.contains(&mode) {
                push_dec_mode(&mut out, mode, on);
            }
        }
        out
    }

    /// Best-effort reset of modes that would leave a local TTY unusable after detach.
    pub fn client_cleanup_sequence() -> Vec<u8> {
        let mut out = Vec::with_capacity(CLIENT_CLEANUP_MODES.len() * 8);
        for &mode in CLIENT_CLEANUP_MODES {
            push_dec_mode(&mut out, mode, false);
        }
        // Show cursor; leave primary screen.
        push_dec_mode(&mut out, 25, true);
        out
    }

    fn mode(&self, m: u16) -> Option<bool> {
        self.modes.get(&m).copied()
    }

    /// True when the child has enabled the alternate screen buffer.
    pub fn alt_screen(&self) -> bool {
        matches!(self.mode(1049), Some(true))
            || matches!(self.mode(1047), Some(true))
            || matches!(self.mode(47), Some(true))
    }
}

fn push_dec_mode(out: &mut Vec<u8>, mode: u16, on: bool) {
    out.extend_from_slice(b"\x1b[?");
    out.extend_from_slice(mode.to_string().as_bytes());
    out.push(if on { b'h' } else { b'l' });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_mouse_and_alt_screen() {
        let mut s = TermState::new();
        s.feed(b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        assert_eq!(s.mode(1049), Some(true));
        assert_eq!(s.mode(1000), Some(true));
        assert_eq!(s.mode(1002), Some(true));
        assert_eq!(s.mode(1006), Some(true));

        let restore = s.restore_sequence();
        assert!(restore.windows(8).any(|w| w == b"\x1b[?1049h"));
        assert!(restore.windows(8).any(|w| w == b"\x1b[?1000h"));
        assert!(restore.windows(8).any(|w| w == b"\x1b[?1006h"));
    }

    #[test]
    fn multi_param_decset() {
        let mut s = TermState::new();
        s.feed(b"\x1b[?1000;1002;1006h");
        assert_eq!(s.mode(1000), Some(true));
        assert_eq!(s.mode(1002), Some(true));
        assert_eq!(s.mode(1006), Some(true));
    }

    #[test]
    fn reset_clears_mode() {
        let mut s = TermState::new();
        s.feed(b"\x1b[?1000h\x1b[?1000l");
        assert_eq!(s.mode(1000), Some(false));
        let restore = s.restore_sequence();
        assert!(restore.windows(8).any(|w| w == b"\x1b[?1000l"));
    }

    #[test]
    fn ignores_osc_and_plain_text() {
        let mut s = TermState::new();
        s.feed(b"hello\x1b]0;title\x07world\x1b[?2004h");
        assert_eq!(s.mode(2004), Some(true));
        assert_eq!(s.mode(1000), None);
    }

    #[test]
    fn split_across_feeds() {
        let mut s = TermState::new();
        s.feed(b"\x1b[?");
        s.feed(b"1000");
        s.feed(b"h");
        assert_eq!(s.mode(1000), Some(true));
    }

    #[test]
    fn alt_screen_before_mouse_in_restore() {
        let mut s = TermState::new();
        s.feed(b"\x1b[?1000h\x1b[?1049h");
        let restore = s.restore_sequence();
        let alt = restore.windows(8).position(|w| w == b"\x1b[?1049h").unwrap();
        let mouse = restore.windows(8).position(|w| w == b"\x1b[?1000h").unwrap();
        assert!(alt < mouse);
    }
}
