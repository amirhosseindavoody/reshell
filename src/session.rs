use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
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
    pub daemon_log: PathBuf,
}

impl SessionPaths {
    pub fn for_name(base: &Path, name: &str) -> Self {
        let dir = base.join(name);
        Self {
            meta: dir.join("meta.json"),
            socket: dir.join("session.sock"),
            attach_lock: dir.join("attached"),
            daemon_log: dir.join("daemon.log"),
            dir,
        }
    }
}

/// Exclusive advisory lock held by the session daemon while a client is attached.
/// The kernel releases the flock if the daemon dies, which lets callers detect
/// stale `attached` files.
pub struct AttachLock {
    _flock: Flock<File>,
    paths: SessionPaths,
}

impl AttachLock {
    /// Create/open the attach lock file and take an exclusive non-blocking flock.
    pub fn try_acquire(paths: &SessionPaths) -> Result<Self> {
        fs::create_dir_all(&paths.dir)
            .with_context(|| format!("create session dir {}", paths.dir.display()))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&paths.attach_lock)
            .with_context(|| format!("open attach lock {}", paths.attach_lock.display()))?;
        let flock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(f) => f,
            Err((_, Errno::EWOULDBLOCK)) => {
                bail!("attach lock is held")
            }
            Err((_, e)) => return Err(e).context("flock attach lock"),
        };
        mark_attached(paths, true)?;
        Ok(Self {
            _flock: flock,
            paths: paths.clone(),
        })
    }
}

impl Drop for AttachLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.attach_lock);
        let _ = mark_attached(&self.paths, false);
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

/// Auto-generated name: `session-{unix_secs}-{4 hex digits}`.
/// The random suffix avoids collisions when two `new` calls share a second.
pub fn generate_session_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let suffix = random_u16(now.subsec_nanos(), std::process::id());
    format!("session-{secs}-{suffix:04x}")
}

/// Pick an auto name that does not already have a session directory.
pub fn allocate_session_name(base: &Path) -> Result<String> {
    for _ in 0..32 {
        let name = generate_session_name();
        let paths = SessionPaths::for_name(base, &name);
        if !paths.dir.exists() {
            return Ok(name);
        }
    }
    bail!("could not allocate a unique session name");
}

fn random_u16(nanos: u32, pid: u32) -> u16 {
    let mut buf = [0u8; 2];
    if fill_random(&mut buf).is_ok() {
        return u16::from_le_bytes(buf);
    }
    ((nanos ^ pid.wrapping_mul(0x9E37)) & 0xffff) as u16
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    let mut f = File::open("/dev/urandom").context("open /dev/urandom")?;
    f.read_exact(buf).context("read /dev/urandom")?;
    Ok(())
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
        Ok(()) => {
            // Zombies still succeed kill(pid, 0); treat them as not running.
            !process_is_zombie(pid)
        }
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true, // EPERM etc. — process exists
    }
}

fn process_is_zombie(pid: i32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(contents) = fs::read_to_string(&path) else {
        return false;
    };
    // /proc/pid/stat: pid (comm) state ...
    let Some(after_comm) = contents.rfind(')') else {
        return false;
    };
    let rest = contents[after_comm + 1..].trim_start();
    rest.starts_with('Z')
}

fn mark_attached(paths: &SessionPaths, attached: bool) -> Result<()> {
    if let Ok(mut meta) = read_meta(paths) {
        meta.attached = attached;
        meta.last_active_unix = now_unix();
        write_meta(paths, &meta)?;
    }
    Ok(())
}

/// True when a live client holds the attach flock.
///
/// If `attached` exists but nobody holds the flock (crashed daemon / leftover),
/// the stale file is removed and this returns false.
pub fn is_attached(paths: &SessionPaths) -> bool {
    recover_stale_attach_lock(paths);
    paths.attach_lock.exists()
}

/// Remove a leftover `attached` file when no process holds its flock.
/// Returns true if a stale lock was cleared.
pub fn recover_stale_attach_lock(paths: &SessionPaths) -> bool {
    if !paths.attach_lock.exists() {
        return false;
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.attach_lock)
    {
        Ok(f) => f,
        Err(_) => return false,
    };
    // Probe with a non-blocking exclusive lock. Success ⇒ no holder ⇒ stale.
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(locked) => {
            drop(locked); // unlocks on drop
            let _ = fs::remove_file(&paths.attach_lock);
            let _ = mark_attached(paths, false);
            true
        }
        Err((_, Errno::EWOULDBLOCK)) => false,
        Err(_) => false,
    }
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
    let _ = fs::remove_file(&paths.daemon_log);
    let _ = fs::remove_dir(&paths.dir);
    Ok(())
}

/// Append a line to the session daemon log (best effort).
pub fn append_daemon_log(paths: &SessionPaths, message: &str) {
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.daemon_log)
        .and_then(|mut f| writeln!(f, "{}", message));
}

pub fn kill_session(base: &Path, name: &str) -> Result<()> {
    validate_session_name(name)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        if paths.dir.exists() {
            bail!(
                "session '{name}' meta missing under {} (incomplete session dir)",
                paths.dir.display()
            );
        }
        bail!("session '{name}' not found");
    }
    let meta = read_meta(&paths).with_context(|| {
        format!(
            "read meta for session '{name}' at {}",
            paths.meta.display()
        )
    })?;
    if process_alive(meta.pid) {
        let pid = Pid::from_raw(meta.pid);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).with_context(|| {
            format!(
                "send SIGTERM to session '{name}' pid {} (permission denied or invalid pid?)",
                meta.pid
            )
        })?;
        // Brief wait then escalate.
        std::thread::sleep(std::time::Duration::from_millis(200));
        if process_alive(meta.pid) {
            nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).with_context(|| {
                format!("send SIGKILL to session '{name}' pid {}", meta.pid)
            })?;
            std::thread::sleep(std::time::Duration::from_millis(50));
            if process_alive(meta.pid) {
                bail!(
                    "session '{name}' pid {} still alive after SIGTERM and SIGKILL",
                    meta.pid
                );
            }
        }
    }
    cleanup_session_files(&paths)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::{signal, SigHandler, Signal};
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult};
    use std::time::Duration;
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

    #[test]
    fn auto_names_include_random_suffix() {
        let a = generate_session_name();
        assert!(a.starts_with("session-"), "{a}");
        let parts_a: Vec<_> = a.split('-').collect();
        assert_eq!(parts_a.len(), 3, "expected session-SECS-SUFFIX, got {a}");
        assert_eq!(parts_a[2].len(), 4, "suffix should be 4 hex digits");
        assert!(
            u16::from_str_radix(parts_a[2], 16).is_ok(),
            "suffix should be hex: {}",
            parts_a[2]
        );
    }

    #[test]
    fn stale_attach_lock_is_recovered() {
        let dir = tempdir().unwrap();
        let paths = SessionPaths::for_name(dir.path(), "stale");
        write_meta(
            &paths,
            &SessionMeta {
                name: "stale".into(),
                pid: std::process::id() as i32,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: true,
                last_active_unix: 1,
            },
        )
        .unwrap();
        File::create(&paths.attach_lock).unwrap();
        assert!(paths.attach_lock.exists());
        assert!(recover_stale_attach_lock(&paths));
        assert!(!paths.attach_lock.exists());
        assert!(!is_attached(&paths));
        let meta = read_meta(&paths).unwrap();
        assert!(!meta.attached);
    }

    #[test]
    fn attach_lock_exclusive() {
        let dir = tempdir().unwrap();
        let paths = SessionPaths::for_name(dir.path(), "lock");
        write_meta(
            &paths,
            &SessionMeta {
                name: "lock".into(),
                pid: 1,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: false,
                last_active_unix: 0,
            },
        )
        .unwrap();
        let held = AttachLock::try_acquire(&paths).unwrap();
        assert!(is_attached(&paths));
        assert!(AttachLock::try_acquire(&paths).is_err());
        drop(held);
        assert!(!is_attached(&paths));
        let again = AttachLock::try_acquire(&paths).unwrap();
        drop(again);
    }

    #[test]
    fn kill_escalates_sigterm_to_sigkill() {
        match unsafe { fork() }.expect("fork") {
            ForkResult::Child => {
                unsafe {
                    let _ = signal(Signal::SIGTERM, SigHandler::SigIgn);
                }
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
    ForkResult::Parent { child } => {
                let dir = tempdir().unwrap();
                let base = dir.path();
                let name = "sticky";
                let paths = SessionPaths::for_name(base, name);
                write_meta(
                    &paths,
                    &SessionMeta {
                        name: name.into(),
                        pid: child.as_raw(),
                        shell: "/bin/bash".into(),
                        created_unix: 1,
                        attached: false,
                        last_active_unix: 0,
                    },
                )
                .unwrap();
                kill_session(base, name).expect("kill_session");
                // Reap so the test process does not leave a zombie around.
                let _ = waitpid(child, None);
                assert!(
                    !process_alive(child.as_raw()),
                    "expected SIGKILL to reap SIGTERM-ignoring pid"
                );
                assert!(!paths.meta.exists());
            }
        }
    }
}
