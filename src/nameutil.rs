//! Portable session-name helpers (no Unix deps).
//!
//! Used by the Linux session daemon and by `reshell ssh` on every platform
//! (including the Windows client binary).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.len() > 64 {
        bail!("session name too long (max 64)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("session name may only contain [A-Za-z0-9._-]");
    }
    Ok(())
}

/// Auto-generated name: `session-{unix_secs}-{4 hex digits}`.
pub fn generate_session_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let suffix = random_u16();
    format!("session-{secs}-{suffix:04x}")
}

fn random_u16() -> u16 {
    let mut buf = [0u8; 2];
    if getrandom::getrandom(&mut buf).is_ok() {
        return u16::from_le_bytes(buf);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    ((now.subsec_nanos() ^ std::process::id().wrapping_mul(0x9E37)) & 0xffff) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ok_and_bad() {
        assert!(validate_session_name("demo").is_ok());
        assert!(validate_session_name("my_session-1.0").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("bad name").is_err());
        assert!(validate_session_name("../x").is_err());
    }

    #[test]
    fn generate_looks_like_session() {
        let n = generate_session_name();
        assert!(n.starts_with("session-"));
        assert!(validate_session_name(&n).is_ok());
    }
}
