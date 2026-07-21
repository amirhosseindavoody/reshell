//! Bounded in-memory ring of PTY bytes captured while detached.
//!
//! Replayed as `Data` on the next attach (after DEC mode restore + clear).
//! This is not a VT screen buffer — TUIs still redraw via the two-phase winsize.

use std::collections::VecDeque;

use anyhow::{bail, Result};

/// Hard cap so a mis-set flag cannot OOM the daemon.
pub const SCROLLBACK_MAX: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct Scrollback {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Scrollback {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    /// Append bytes, dropping oldest when over capacity.
    pub fn push(&mut self, data: &[u8]) {
        if self.cap == 0 || data.is_empty() {
            return;
        }
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf
                .extend(data[data.len() - self.cap..].iter().copied());
            return;
        }
        let need = self.buf.len() + data.len();
        if need > self.cap {
            let drop_n = need - self.cap;
            self.buf.drain(..drop_n);
        }
        self.buf.extend(data.iter().copied());
    }

    /// Drain all buffered bytes for attach replay.
    pub fn take(&mut self) -> Vec<u8> {
        self.buf.drain(..).collect()
    }
}

/// Parse a size string: plain decimal bytes, or with `K`/`M`/`Ki`/`Mi` suffix.
///
/// Examples: `0`, `4096`, `512K`, `1M`, `1Mi`.
pub fn parse_scrollback_size(s: &str) -> Result<usize> {
    let s = s.trim();
    if s.is_empty() {
        bail!("scrollback size must not be empty");
    }
    let lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(rest) = lower.strip_suffix("mi") {
        (rest, 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix("ki") {
        (rest, 1024)
    } else if let Some(rest) = lower.strip_suffix('m') {
        (rest, 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix('k') {
        (rest, 1024)
    } else {
        (lower.as_str(), 1usize)
    };
    let num = num.trim();
    if num.is_empty() {
        bail!("invalid scrollback size '{s}' (examples: 0, 512K, 1M)");
    }
    let n: usize = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid scrollback size '{s}' (examples: 0, 512K, 1M)"))?;
    let bytes = n
        .checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("scrollback size '{s}' overflows"))?;
    if bytes > SCROLLBACK_MAX {
        bail!(
            "scrollback size {bytes} exceeds max {} (16M)",
            SCROLLBACK_MAX
        );
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_respects_cap() {
        let mut sb = Scrollback::new(8);
        sb.push(b"abcdefghij"); // 10 bytes → keep last 8
        assert_eq!(sb.take(), b"cdefghij");
    }

    #[test]
    fn push_drops_oldest() {
        let mut sb = Scrollback::new(4);
        sb.push(b"ab");
        sb.push(b"cd");
        sb.push(b"ef");
        assert_eq!(sb.take(), b"cdef");
    }

    #[test]
    fn disabled_is_noop() {
        let mut sb = Scrollback::new(0);
        sb.push(b"hello");
        assert!(sb.take().is_empty());
    }

    #[test]
    fn parse_suffixes() {
        assert_eq!(parse_scrollback_size("0").unwrap(), 0);
        assert_eq!(parse_scrollback_size("4096").unwrap(), 4096);
        assert_eq!(parse_scrollback_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_scrollback_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_scrollback_size("1Mi").unwrap(), 1024 * 1024);
        assert!(parse_scrollback_size("32M").is_err());
        assert!(parse_scrollback_size("").is_err());
        assert!(parse_scrollback_size("nope").is_err());
    }
}
