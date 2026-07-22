use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, Flock, FlockArg, OFlag};
use nix::sys::stat::Mode;
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
        Self::for_dir(base.join(name))
    }

    /// Build paths from an absolute session directory (survives rename when
    /// resolved via `/proc/self/fd/<dirfd>`).
    pub fn for_dir(dir: PathBuf) -> Self {
        Self {
            meta: dir.join("meta.json"),
            socket: dir.join("session.sock"),
            attach_lock: dir.join("attached"),
            daemon_log: dir.join("daemon.log"),
            dir,
        }
    }
}

/// Resolve the current path of an open directory fd (Linux `/proc`).
pub fn paths_from_dir_fd(dir_fd: RawFd) -> Result<SessionPaths> {
    let link = PathBuf::from(format!("/proc/self/fd/{dir_fd}"));
    let dir = fs::read_link(&link)
        .with_context(|| format!("resolve session dir via {}", link.display()))?;
    Ok(SessionPaths::for_dir(dir))
}

/// Exclusive advisory lock held by the session daemon while a client is attached.
/// The kernel releases the flock if the daemon dies, which lets callers detect
/// stale `attached` files.
///
/// Meta/lock updates go through `dir_fd` so a live `reshell rename` (directory
/// move) does not leave the daemon writing to a stale path.
pub struct AttachLock {
    _flock: Flock<File>,
    dir_fd: OwnedFd,
}

impl AttachLock {
    /// Create/open the attach lock file and take an exclusive non-blocking flock.
    pub fn try_acquire(dir_fd: &OwnedFd) -> Result<Self> {
        let paths = paths_from_dir_fd(dir_fd.as_raw_fd())?;
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
        mark_attached(&paths, true)?;
        let dir_fd = dup_fd(dir_fd)?;
        Ok(Self {
            _flock: flock,
            dir_fd,
        })
    }
}

impl Drop for AttachLock {
    fn drop(&mut self) {
        if let Ok(paths) = paths_from_dir_fd(self.dir_fd.as_raw_fd()) {
            let _ = fs::remove_file(&paths.attach_lock);
            let _ = mark_attached(&paths, false);
        }
    }
}

fn dup_fd(fd: &OwnedFd) -> Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let raw = nix::unistd::dup(fd.as_raw_fd()).context("dup session dir fd")?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Open the session directory as a directory fd (rename-safe).
pub fn open_session_dir_fd(paths: &SessionPaths) -> Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let raw = open(
        &paths.dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open session dir {}", paths.dir.display()))?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
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
    let _ = cleanup_stale_sessions(base)?;
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
            continue;
        }
        meta.attached = is_attached(&paths);
        // Keep meta.name aligned with the directory name (after rename).
        if meta.name != name {
            meta.name = name.clone();
            let _ = write_meta(&paths, &meta);
        }
        out.push((meta, paths));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

/// Remove dead-session leftovers, orphan dirs, and stale attach locks.
/// Returns how many session directories were removed.
pub fn cleanup_stale_sessions(base: &Path) -> Result<usize> {
    ensure_base_dir(base)?;
    let mut removed = 0usize;
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).context("read session base dir"),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let paths = SessionPaths::for_name(base, &name);

        if !paths.meta.exists() {
            // Orphan dir (no meta): remove leftover sock/lock/log then the dir.
            let _ = cleanup_session_files(&paths);
            if !paths.dir.exists() {
                removed += 1;
            }
            continue;
        }

        let meta = match read_meta(&paths) {
            Ok(m) => m,
            Err(_) => {
                let _ = cleanup_session_files(&paths);
                removed += 1;
                continue;
            }
        };

        if !process_alive(meta.pid) {
            let _ = cleanup_session_files(&paths);
            removed += 1;
            continue;
        }

        // Live session: still recover a stale attach lock if present.
        let _ = recover_stale_attach_lock(&paths);
    }
    Ok(removed)
}

/// Rename a live (or detached) session directory and update `meta.name`.
///
/// The daemon keeps a directory fd open so attach-lock / meta / log writes keep
/// working after the directory moves.
pub fn rename_session(base: &Path, old_name: &str, new_name: &str) -> Result<()> {
    validate_session_name(old_name)?;
    validate_session_name(new_name)?;
    if old_name == new_name {
        return Ok(());
    }

    let old_paths = SessionPaths::for_name(base, old_name);
    let new_paths = SessionPaths::for_name(base, new_name);

    if !old_paths.meta.exists() {
        bail!("session '{old_name}' not found");
    }
    if new_paths.dir.exists() {
        bail!("session '{new_name}' already exists");
    }

    let meta = read_meta(&old_paths)?;
    if !process_alive(meta.pid) {
        let _ = cleanup_session_files(&old_paths);
        bail!("session '{old_name}' is not running (cleaned up stale files)");
    }

    fs::rename(&old_paths.dir, &new_paths.dir).with_context(|| {
        format!(
            "rename {} → {}",
            old_paths.dir.display(),
            new_paths.dir.display()
        )
    })?;

    let mut meta = read_meta(&new_paths).with_context(|| {
        format!(
            "read meta after rename at {}",
            new_paths.meta.display()
        )
    })?;
    meta.name = new_name.to_string();
    write_meta(&new_paths, &meta)?;
    Ok(())
}

/// Load a live session for `info` (refuses dead/missing).
pub fn session_info(base: &Path, name: &str) -> Result<(SessionMeta, SessionPaths)> {
    validate_session_name(name)?;
    let _ = cleanup_stale_sessions(base)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        bail!("session '{name}' not found");
    }
    let mut meta = read_meta(&paths)?;
    if !process_alive(meta.pid) {
        let _ = cleanup_session_files(&paths);
        bail!("session '{name}' is not running (cleaned up leftovers)");
    }
    meta.attached = is_attached(&paths);
    if meta.name != name {
        meta.name = name.to_string();
        let _ = write_meta(&paths, &meta);
    }
    Ok((meta, paths))
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

/// Environment variable set in the session shell so nested tools can detect it.
pub const RESHELL_SESSION_ENV: &str = "RESHELL_SESSION";

/// Live session this process is running inside, if any.
///
/// Prefers a daemon pid found among process ancestors (survives `rename`, which
/// leaves a stale `RESHELL_SESSION` value). Falls back to `$RESHELL_SESSION`
/// when that names a live session.
pub fn current_session(base: &Path) -> Result<Option<SessionMeta>> {
    let _ = cleanup_stale_sessions(base)?;
    let sessions = list_sessions(base)?;
    if sessions.is_empty() {
        return Ok(None);
    }

    let ancestors = process_ancestor_pids();
    if !ancestors.is_empty() {
        for (meta, paths) in &sessions {
            if ancestors.contains(&meta.pid) {
                let mut meta = meta.clone();
                meta.attached = is_attached(paths);
                return Ok(Some(meta));
            }
        }
    }

    if let Ok(name) = std::env::var(RESHELL_SESSION_ENV) {
        if validate_session_name(&name).is_ok() {
            if let Some((meta, paths)) = sessions.into_iter().find(|(m, _)| m.name == name) {
                let mut meta = meta;
                meta.attached = is_attached(&paths);
                return Ok(Some(meta));
            }
        }
    }

    Ok(None)
}

/// Parent pid chain for this process (`/proc/.../stat`), excluding pid 0/1.
fn process_ancestor_pids() -> Vec<i32> {
    let mut out = Vec::new();
    let mut pid = std::process::id() as i32;
    for _ in 0..64 {
        let Some(ppid) = read_ppid(pid) else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        if out.contains(&ppid) {
            break;
        }
        out.push(ppid);
        pid = ppid;
    }
    out
}

/// Parse `ppid` from Linux `/proc/<pid>/stat`.
fn read_ppid(pid: i32) -> Option<i32> {
    let data = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: `pid (comm) state ppid ...` — `comm` may contain spaces/parens.
    let rparen = data.rfind(')')?;
    let rest = data.get(rparen + 2..)?;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
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

/// Terminate every live session under `base`. Returns killed session names
/// (sorted, same order as [`list_sessions`]).
pub fn kill_all_sessions(base: &Path) -> Result<Vec<String>> {
    let sessions = list_sessions(base)?;
    let mut killed = Vec::with_capacity(sessions.len());
    for (meta, _) in sessions {
        kill_session(base, &meta.name)?;
        killed.push(meta.name);
    }
    Ok(killed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::{signal, SigHandler, Signal};
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult};
    use std::sync::Mutex;
    use std::time::Duration;
    use tempfile::tempdir;

    /// Serialize tests that mutate `RESHELL_SESSION` (process-global env).
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    fn current_session_from_ancestor_pid() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();
        let self_pid = std::process::id() as i32;
        let parent = read_ppid(self_pid).expect("ppid");
        assert!(parent > 1);
        write_meta(
            &SessionPaths::for_name(base, "nested"),
            &SessionMeta {
                name: "nested".into(),
                pid: parent,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: false,
                last_active_unix: 1,
            },
        )
        .unwrap();
        // Also write a decoy that `$RESHELL_SESSION` might point at after rename.
        write_meta(
            &SessionPaths::for_name(base, "stale-name"),
            &SessionMeta {
                name: "stale-name".into(),
                pid: self_pid,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: false,
                last_active_unix: 99,
            },
        )
        .unwrap();
        // Ancestor match must win over a live-but-unrelated env name (rename case).
        // Serialize env mutation: other tests must not race on RESHELL_SESSION.
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        // SAFETY: held behind ENV_TEST_LOCK; restored before unlock.
        unsafe {
            std::env::set_var(RESHELL_SESSION_ENV, "stale-name");
        }
        let cur = current_session(base).unwrap().expect("current");
        unsafe {
            std::env::remove_var(RESHELL_SESSION_ENV);
        }
        assert_eq!(cur.name, "nested");
    }

    #[test]
    fn current_session_from_env_when_not_nested() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();
        // Use an alive pid that is not an ancestor (our own pid).
        let self_pid = std::process::id() as i32;
        for (name, last) in [("other", 200u64), ("mine", 100u64)] {
            write_meta(
                &SessionPaths::for_name(base, name),
                &SessionMeta {
                    name: name.into(),
                    pid: self_pid,
                    shell: "/bin/bash".into(),
                    created_unix: 1,
                    attached: false,
                    last_active_unix: last,
                },
            )
            .unwrap();
        }
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        // SAFETY: held behind ENV_TEST_LOCK; restored before unlock.
        unsafe {
            std::env::set_var(RESHELL_SESSION_ENV, "mine");
        }
        let cur = current_session(base).unwrap().expect("current");
        unsafe {
            std::env::remove_var(RESHELL_SESSION_ENV);
        }
        assert_eq!(cur.name, "mine");
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
        let dir_fd = open_session_dir_fd(&paths).unwrap();
        let held = AttachLock::try_acquire(&dir_fd).unwrap();
        assert!(is_attached(&paths));
        assert!(AttachLock::try_acquire(&dir_fd).is_err());
        drop(held);
        assert!(!is_attached(&paths));
        let again = AttachLock::try_acquire(&dir_fd).unwrap();
        drop(again);
    }

    #[test]
    fn rename_updates_meta_and_directory() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();
        let self_pid = std::process::id() as i32;
        let old = SessionPaths::for_name(base, "old-name");
        write_meta(
            &old,
            &SessionMeta {
                name: "old-name".into(),
                pid: self_pid,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: false,
                last_active_unix: 1,
            },
        )
        .unwrap();
        rename_session(base, "old-name", "new-name").unwrap();
        assert!(!old.dir.exists());
        let new = SessionPaths::for_name(base, "new-name");
        assert!(new.meta.exists());
        let meta = read_meta(&new).unwrap();
        assert_eq!(meta.name, "new-name");
    }

    #[test]
    fn cleanup_stale_removes_dead_and_orphan() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();

        // Orphan directory with no meta.
        let orphan = base.join("orphan");
        fs::create_dir_all(&orphan).unwrap();
        File::create(orphan.join("session.sock")).unwrap();

        // Definitely-dead pid session.
        let dead = SessionPaths::for_name(base, "dead");
        write_meta(
            &dead,
            &SessionMeta {
                name: "dead".into(),
                pid: i32::MAX - 1,
                shell: "/bin/bash".into(),
                created_unix: 1,
                attached: false,
                last_active_unix: 0,
            },
        )
        .unwrap();

        let n = cleanup_stale_sessions(base).unwrap();
        assert!(n >= 2, "expected orphan+dead removed, got {n}");
        assert!(!orphan.exists());
        assert!(!dead.dir.exists());
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
