use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::unistd::{getuid, Pid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub name: String,
    pub pid: i32,
    pub shell: String,
    pub created_unix: u64,
    pub attached: bool,
    /// Last attach/detach time; used by `reshell attach` with no name.
    /// Older meta files omit this field (treated as 0 → fall back to created).
    #[serde(default)]
    pub last_active_unix: u64,
}

#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub dir: PathBuf,
    pub meta: PathBuf,
    pub socket: PathBuf,
    pub attach_lock: PathBuf,
}

impl SessionPaths {
    pub fn for_name(base: &Path, name: &str) -> Self {
        let dir = base.join(name);
        Self {
            meta: dir.join("meta.json"),
            socket: dir.join("session.sock"),
            attach_lock: dir.join("attached"),
            dir,
        }
    }
}

pub fn session_base_dir() -> Result<PathBuf> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return Ok(PathBuf::from(runtime).join("reshell"));
        }
    }
    let uid = getuid().as_raw();
    Ok(PathBuf::from(format!("/tmp/reshell-{uid}")))
}

pub fn ensure_base_dir(base: &Path) -> Result<()> {
    fs::create_dir_all(base)
        .with_context(|| format!("create session base dir {}", base.display()))?;
    // Restrict to owner when possible.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(base, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

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

pub fn generate_session_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("session-{secs}")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_meta(paths: &SessionPaths, meta: &SessionMeta) -> Result<()> {
    fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create session dir {}", paths.dir.display()))?;
    let tmp = paths.meta.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(meta).context("serialize session meta")?;
    {
        let mut f = File::create(&tmp).context("create temp meta file")?;
        f.write_all(&json).context("write meta")?;
        f.write_all(b"\n").ok();
    }
    fs::rename(&tmp, &paths.meta).context("rename meta file")?;
    Ok(())
}

pub fn read_meta(paths: &SessionPaths) -> Result<SessionMeta> {
    let mut f = File::open(&paths.meta)
        .with_context(|| format!("open session meta {}", paths.meta.display()))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).context("read session meta")?;
    serde_json::from_str(&buf).context("parse session meta")
}

pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match nix::sys::signal::kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // EPERM etc. — process exists
    }
}

pub fn set_attached(paths: &SessionPaths, attached: bool) -> Result<()> {
    if attached {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&paths.attach_lock)
            .with_context(|| format!("create attach lock {}", paths.attach_lock.display()))?;
    } else {
        let _ = fs::remove_file(&paths.attach_lock);
    }
    if let Ok(mut meta) = read_meta(paths) {
        meta.attached = attached;
        meta.last_active_unix = now_unix();
        let _ = write_meta(paths, &meta);
    }
    Ok(())
}

pub fn is_attached(paths: &SessionPaths) -> bool {
    paths.attach_lock.exists()
}

pub fn list_sessions(base: &Path) -> Result<Vec<(SessionMeta, SessionPaths)>> {
    ensure_base_dir(base)?;
    let mut out = Vec::new();
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).context("read session base dir"),
    };

    for entry in entries {
        let entry = entry.context("read dir entry")?;
        let file_type = entry.file_type().context("dir entry type")?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let paths = SessionPaths::for_name(base, &name);
        if !paths.meta.exists() {
            continue;
        }
        let mut meta = match read_meta(&paths) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !process_alive(meta.pid) {
            // Stale session leftovers.
            let _ = cleanup_session_files(&paths);
            continue;
        }
        meta.attached = is_attached(&paths);
        out.push((meta, paths));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

/// Session activity timestamp: last attach/detach, else creation time.
pub fn session_activity(meta: &SessionMeta) -> u64 {
    if meta.last_active_unix > 0 {
        meta.last_active_unix
    } else {
        meta.created_unix
    }
}

/// Most recently active live session (by `last_active_unix`, then created).
pub fn most_recent_session(base: &Path) -> Result<SessionMeta> {
    let mut sessions = list_sessions(base)?;
    if sessions.is_empty() {
        bail!("no sessions found");
    }
    sessions.sort_by(|a, b| {
        session_activity(&b.0)
            .cmp(&session_activity(&a.0))
            .then_with(|| b.0.created_unix.cmp(&a.0.created_unix))
            .then_with(|| a.0.name.cmp(&b.0.name))
    });
    Ok(sessions.remove(0).0)
}

pub fn cleanup_session_files(paths: &SessionPaths) -> Result<()> {
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.meta);
    let _ = fs::remove_file(&paths.attach_lock);
    let _ = fs::remove_dir(&paths.dir);
    Ok(())
}

pub fn kill_session(base: &Path, name: &str) -> Result<()> {
    validate_session_name(name)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        bail!("session '{name}' not found");
    }
    let meta = read_meta(&paths)?;
    if process_alive(meta.pid) {
        let pid = Pid::from_raw(meta.pid);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM)
            .with_context(|| format!("send SIGTERM to session pid {}", meta.pid))?;
        // Brief wait then escalate.
        std::thread::sleep(std::time::Duration::from_millis(200));
        if process_alive(meta.pid) {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    cleanup_session_files(&paths)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_names() {
        assert!(validate_session_name("demo").is_ok());
        assert!(validate_session_name("my_session-1.0").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name("bad name").is_err());
        assert!(validate_session_name("../x").is_err());
    }

    #[test]
    fn meta_roundtrip() {
        let dir = tempdir().unwrap();
        let paths = SessionPaths::for_name(dir.path(), "demo");
        let meta = SessionMeta {
            name: "demo".into(),
            pid: 1,
            shell: "/bin/bash".into(),
            created_unix: 123,
            attached: false,
            last_active_unix: 0,
        };
        write_meta(&paths, &meta).unwrap();
        let loaded = read_meta(&paths).unwrap();
        assert_eq!(loaded.name, "demo");
        assert_eq!(loaded.pid, 1);
        assert_eq!(loaded.last_active_unix, 0);
    }

    #[test]
    fn most_recent_prefers_last_active() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();

        // Two fake metas with a live-looking pid (our own) so list_sessions
        // keeps them; we won't actually run daemons.
        let self_pid = std::process::id() as i32;
        for (name, created, last) in [
            ("older", 100u64, 100u64),
            ("newer", 200u64, 500u64),
            ("middle", 300u64, 300u64),
        ] {
            let paths = SessionPaths::for_name(base, name);
            write_meta(
                &paths,
                &SessionMeta {
                    name: name.into(),
                    pid: self_pid,
                    shell: "/bin/bash".into(),
                    created_unix: created,
                    attached: false,
                    last_active_unix: last,
                },
            )
            .unwrap();
        }

        let recent = most_recent_session(base).unwrap();
        assert_eq!(recent.name, "newer");
    }
}
