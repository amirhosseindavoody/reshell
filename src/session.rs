use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, Flock, FlockArg, OFlag};
use nix::sys::signal::{self, Signal};
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
    /// Last attach/detach time; used by non-TTY bare attach and the picker sort.
    /// Older meta files omit this field (treated as 0 → fall back to created).
    #[serde(default)]
    pub last_active_unix: u64,
    /// Set when the session has ended and been moved to the archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_unix: Option<u64>,
    /// Why the session ended: `shell_exit`, `killed`, `stale`, or `replaced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
}

/// Why a session directory was torn down (recorded in archived `meta.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Shell exited; daemon finished normally.
    ShellExit,
    /// `reshell kill` (or equivalent) terminated the daemon.
    Killed,
    /// Dead pid / leftover discovered by `list` / `clean` / attach preflight.
    Stale,
    /// A new session reused the name and replaced dead leftovers.
    Replaced,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShellExit => "shell_exit",
            Self::Killed => "killed",
            Self::Stale => "stale",
            Self::Replaced => "replaced",
        }
    }
}

/// Colocated archive directory name under a custom `--dir` base (skipped by list).
pub const ARCHIVE_DIR_NAME: &str = "archive";

/// File in a live session dir that records where to archive on exit.
const ARCHIVE_ROOT_FILE: &str = "archive_root";

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

    pub fn client_pid_file(&self) -> PathBuf {
        self.dir.join("client.pid")
    }

    pub fn switch_to_file(&self) -> PathBuf {
        self.dir.join("switch_to")
    }

    /// Directory of rotating text history files (`0001.txt`, …).
    pub fn history_dir(&self) -> PathBuf {
        self.dir.join("history")
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
            let _ = clear_client_pid(&paths);
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

/// Durable archive root for ended sessions (history + daemon.log + meta).
///
/// Resolution order:
/// 1. `explicit` (`--archive-dir`)
/// 2. `RESHELL_ARCHIVE_DIR`
/// 3. If `custom_base` is true (CLI `--dir` was set): `$base/archive`
/// 4. `$XDG_STATE_HOME/reshell/archive` or `$HOME/.local/state/reshell/archive`
/// 5. `$base/archive` (last resort)
pub fn resolve_archive_dir(
    explicit: Option<&Path>,
    base: &Path,
    custom_base: bool,
) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(d) = std::env::var("RESHELL_ARCHIVE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if custom_base {
        return base.join(ARCHIVE_DIR_NAME);
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        if !state.is_empty() {
            return PathBuf::from(state).join("reshell").join(ARCHIVE_DIR_NAME);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("reshell")
                .join(ARCHIVE_DIR_NAME);
        }
    }
    base.join(ARCHIVE_DIR_NAME)
}

pub fn ensure_archive_dir(archive_root: &Path) -> Result<()> {
    fs::create_dir_all(archive_root)
        .with_context(|| format!("create archive dir {}", archive_root.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(archive_root, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Persist the archive root inside a live session so the daemon can find it on exit.
pub fn write_archive_root(paths: &SessionPaths, archive_root: &Path) -> Result<()> {
    let path = paths.dir.join(ARCHIVE_ROOT_FILE);
    fs::write(&path, format!("{}\n", archive_root.display()))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Read archive root recorded at session create; fall back to `fallback`.
pub fn read_archive_root(paths: &SessionPaths, fallback: &Path) -> PathBuf {
    let path = paths.dir.join(ARCHIVE_ROOT_FILE);
    if let Ok(raw) = fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    fallback.to_path_buf()
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

pub use crate::nameutil::{generate_session_name, validate_session_name};

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

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_meta(paths: &SessionPaths, meta: &SessionMeta) -> Result<()> {
    fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create session dir {}", paths.dir.display()))?;
    // Unique temp name so concurrent writers (e.g. detach updating `attached`
    // while `rename` rewrites `name`) cannot race on a shared `meta.json.tmp`.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = paths.dir.join(format!(
        ".meta.json.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let json = serde_json::to_vec_pretty(meta).context("serialize session meta")?;
    {
        let mut f = File::create(&tmp)
            .with_context(|| format!("create temp meta file {}", tmp.display()))?;
        f.write_all(&json).context("write meta")?;
        f.write_all(b"\n").ok();
    }
    if let Err(e) = fs::rename(&tmp, &paths.meta) {
        let _ = fs::remove_file(&tmp);
        return Err(e).context("rename meta file");
    }
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

pub fn list_sessions(base: &Path, archive_root: &Path) -> Result<Vec<(SessionMeta, SessionPaths)>> {
    ensure_base_dir(base)?;
    let _ = cleanup_stale_sessions(base, archive_root)?;
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
        if name == ARCHIVE_DIR_NAME {
            continue;
        }
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
/// Ended sessions with history/logs are moved under `archive_root` first.
/// Returns how many live session directories were removed.
pub fn cleanup_stale_sessions(base: &Path, archive_root: &Path) -> Result<usize> {
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
        if name == ARCHIVE_DIR_NAME {
            continue;
        }
        let paths = SessionPaths::for_name(base, &name);

        if !paths.meta.exists() {
            // Orphan dir (no meta): remove leftover sock/lock/log then the dir.
            let _ = cleanup_session_files(&paths, archive_root, EndReason::Stale);
            if !paths.dir.exists() {
                removed += 1;
            }
            continue;
        }

        let meta = match read_meta(&paths) {
            Ok(m) => m,
            Err(_) => {
                let _ = cleanup_session_files(&paths, archive_root, EndReason::Stale);
                removed += 1;
                continue;
            }
        };

        if !process_alive(meta.pid) {
            let _ = cleanup_session_files(&paths, archive_root, EndReason::Stale);
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
        let archive = read_archive_root(&old_paths, &base.join(ARCHIVE_DIR_NAME));
        let _ = cleanup_session_files(&old_paths, &archive, EndReason::Stale);
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
pub fn session_info(
    base: &Path,
    name: &str,
    archive_root: &Path,
) -> Result<(SessionMeta, SessionPaths)> {
    validate_session_name(name)?;
    let _ = cleanup_stale_sessions(base, archive_root)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        bail!("session '{name}' not found");
    }
    let mut meta = read_meta(&paths)?;
    if !process_alive(meta.pid) {
        let archive = read_archive_root(&paths, archive_root);
        let _ = cleanup_session_files(&paths, &archive, EndReason::Stale);
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
pub fn most_recent_session(base: &Path, archive_root: &Path) -> Result<SessionMeta> {
    let mut sessions = list_sessions(base, archive_root)?;
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
pub fn current_session(base: &Path, archive_root: &Path) -> Result<Option<SessionMeta>> {
    let _ = cleanup_stale_sessions(base, archive_root)?;
    let sessions = list_sessions(base, archive_root)?;
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

/// Archive durable artifacts (meta, daemon.log, history/) then remove the live dir.
///
/// Returns the archive directory path when something useful was preserved.
pub fn cleanup_session_files(
    paths: &SessionPaths,
    archive_root: &Path,
    reason: EndReason,
) -> Result<Option<PathBuf>> {
    let archive = read_archive_root(paths, archive_root);
    let archived = archive_session(paths, &archive, reason)?;

    // Remove ephemeral leftovers (and anything archive did not move).
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.attach_lock);
    let _ = fs::remove_file(paths.client_pid_file());
    let _ = fs::remove_file(paths.switch_to_file());
    let _ = fs::remove_file(paths.dir.join(ARCHIVE_ROOT_FILE));
    let _ = fs::remove_file(&paths.meta);
    let _ = fs::remove_file(&paths.daemon_log);
    let _ = fs::remove_dir_all(paths.history_dir());
    // vscode-si helper dir, etc.
    if paths.dir.exists() {
        let _ = fs::remove_dir_all(&paths.dir);
    }
    Ok(archived)
}

/// Move meta / daemon.log / history into `$archive_root/<name>@<ended_unix>/`.
fn archive_session(
    paths: &SessionPaths,
    archive_root: &Path,
    reason: EndReason,
) -> Result<Option<PathBuf>> {
    let has_meta = paths.meta.exists();
    let has_log = paths.daemon_log.exists();
    let hist = paths.history_dir();
    let has_history = hist.is_dir()
        && fs::read_dir(&hist)
            .map(|rd| rd.filter_map(|e| e.ok()).next().is_some())
            .unwrap_or(false);

    if !has_meta && !has_log && !has_history {
        return Ok(None);
    }

    ensure_archive_dir(archive_root)?;
    let ended = now_unix();
    let name = read_meta(paths)
        .map(|m| m.name)
        .unwrap_or_else(|_| {
            paths
                .dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into())
        });
    let archive_id = format!("{name}@{ended}");
    let dest = archive_root.join(&archive_id);
    // Collision in the same second: append a counter.
    let dest = if dest.exists() {
        let mut n = 1u32;
        loop {
            let cand = archive_root.join(format!("{archive_id}-{n}"));
            if !cand.exists() {
                break cand;
            }
            n += 1;
            if n > 1000 {
                bail!("could not allocate archive dir under {}", archive_root.display());
            }
        }
    } else {
        dest
    };

    fs::create_dir_all(&dest)
        .with_context(|| format!("create archive session dir {}", dest.display()))?;

    let dest_paths = SessionPaths::for_dir(dest.clone());

    if has_meta {
        let mut meta = read_meta(paths)?;
        meta.attached = false;
        meta.ended_unix = Some(ended);
        meta.end_reason = Some(reason.as_str().to_string());
        write_meta(&dest_paths, &meta)?;
    } else {
        // Minimal meta so `list --all` / `info` still work.
        let meta = SessionMeta {
            name: name.clone(),
            pid: 0,
            shell: String::new(),
            created_unix: 0,
            attached: false,
            last_active_unix: 0,
            ended_unix: Some(ended),
            end_reason: Some(reason.as_str().to_string()),
        };
        write_meta(&dest_paths, &meta)?;
    }

    if has_log {
        let _ = fs::rename(&paths.daemon_log, &dest_paths.daemon_log)
            .or_else(|_| fs::copy(&paths.daemon_log, &dest_paths.daemon_log).map(|_| ()));
    }
    if has_history {
        let dest_hist = dest_paths.history_dir();
        if dest_hist.exists() {
            let _ = fs::remove_dir_all(&dest_hist);
        }
        if fs::rename(&hist, &dest_hist).is_err() {
            // Cross-device: copy tree then leave original for cleanup.
            copy_dir_recursive(&hist, &dest_hist)?;
        }
    }

    Ok(Some(dest))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)
                .with_context(|| format!("copy {} → {}", entry.path().display(), to.display()))?;
        }
    }
    Ok(())
}

/// An ended session preserved under the archive root.
#[derive(Debug, Clone)]
pub struct EndedSession {
    pub meta: SessionMeta,
    pub paths: SessionPaths,
    /// Directory name under the archive root (`name@unix`).
    pub archive_id: String,
}

/// List archived (ended) sessions, newest first.
pub fn list_ended_sessions(archive_root: &Path) -> Result<Vec<EndedSession>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(archive_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).context("read archive dir"),
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
        let archive_id = entry.file_name().to_string_lossy().into_owned();
        let paths = SessionPaths::for_dir(entry.path());
        if !paths.meta.exists() {
            continue;
        }
        let meta = match read_meta(&paths) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(EndedSession {
            meta,
            paths,
            archive_id,
        });
    }

    out.sort_by(|a, b| {
        b.meta
            .ended_unix
            .unwrap_or(0)
            .cmp(&a.meta.ended_unix.unwrap_or(0))
            .then_with(|| a.meta.name.cmp(&b.meta.name))
            .then_with(|| a.archive_id.cmp(&b.archive_id))
    });
    Ok(out)
}

/// Find the newest archived session with this live name (or exact archive id).
pub fn find_ended_session(archive_root: &Path, name_or_id: &str) -> Result<EndedSession> {
    let ended = list_ended_sessions(archive_root)?;
    if let Some(e) = ended.iter().find(|e| e.archive_id == name_or_id) {
        return Ok(e.clone());
    }
    if let Some(e) = ended.into_iter().find(|e| e.meta.name == name_or_id) {
        return Ok(e);
    }
    bail!("ended session '{name_or_id}' not found under {}", archive_root.display());
}

/// Delete archived sessions. Returns how many archive dirs were removed.
pub fn purge_ended_sessions(archive_root: &Path) -> Result<usize> {
    let ended = list_ended_sessions(archive_root)?;
    let mut removed = 0usize;
    for e in ended {
        if fs::remove_dir_all(&e.paths.dir).is_ok() {
            removed += 1;
        }
    }
    // Remove empty archive root (best effort).
    let _ = fs::remove_dir(archive_root);
    Ok(removed)
}

pub fn kill_session(base: &Path, name: &str, archive_root: &Path) -> Result<()> {
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
    let archive = read_archive_root(&paths, archive_root);
    cleanup_session_files(&paths, &archive, EndReason::Killed)?;
    Ok(())
}

/// Terminate every live session under `base`. Returns killed session names
/// (sorted, same order as [`list_sessions`]).
pub fn kill_all_sessions(base: &Path, archive_root: &Path) -> Result<Vec<String>> {
    let sessions = list_sessions(base, archive_root)?;
    let mut killed = Vec::with_capacity(sessions.len());
    for (meta, _) in sessions {
        kill_session(base, &meta.name, archive_root)?;
        killed.push(meta.name);
    }
    Ok(killed)
}

/// Record the interactive attach client's pid (from `SO_PEERCRED`).
pub fn write_client_pid(paths: &SessionPaths, pid: i32) -> Result<()> {
    let path = paths.client_pid_file();
    let tmp = paths.dir.join(format!(".client.pid.{}.tmp", std::process::id()));
    fs::write(&tmp, format!("{pid}\n")).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

pub fn clear_client_pid(paths: &SessionPaths) -> Result<()> {
    let _ = fs::remove_file(paths.client_pid_file());
    Ok(())
}

pub fn read_client_pid(paths: &SessionPaths) -> Option<i32> {
    let raw = fs::read_to_string(paths.client_pid_file()).ok()?;
    raw.trim().parse().ok()
}

/// Ask the attach client for this session to switch to `target` (SIGUSR1 + file).
pub fn write_switch_to(paths: &SessionPaths, target: &str) -> Result<()> {
    validate_session_name(target)?;
    let path = paths.switch_to_file();
    let tmp = paths.dir.join(format!(".switch_to.{}.tmp", std::process::id()));
    fs::write(&tmp, format!("{target}\n")).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Read and remove a pending switch target, if any.
pub fn take_switch_to(paths: &SessionPaths) -> Option<String> {
    let path = paths.switch_to_file();
    let raw = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let name = raw.trim().to_string();
    if name.is_empty() || validate_session_name(&name).is_err() {
        return None;
    }
    Some(name)
}

/// Ask the attach client for `name` to detach (shell keeps running).
///
/// Signals `SIGHUP` to the recorded outer client (`client.pid`), which sends
/// `Detach` and exits — the same path as an SSH hangup. Waits until the attach
/// lock is released. Idempotent when the session is already detached.
pub fn request_detach(base: &Path, name: &str) -> Result<()> {
    validate_session_name(name)?;
    let paths = SessionPaths::for_name(base, name);
    if !paths.meta.exists() {
        bail!("session '{name}' not found");
    }
    if !is_attached(&paths) {
        return Ok(());
    }
    let Some(pid) = read_client_pid(&paths) else {
        bail!(
            "cannot detach session '{name}': no attach client pid \
             (outer attach is too old or not recorded)"
        );
    };
    if !process_alive(pid) {
        let _ = clear_client_pid(&paths);
        if !is_attached(&paths) {
            return Ok(());
        }
        bail!("cannot detach session '{name}': attach client pid {pid} is dead");
    }
    signal::kill(Pid::from_raw(pid), Signal::SIGHUP)
        .with_context(|| format!("signal attach client pid {pid} (SIGHUP)"))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !is_attached(&paths) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("timed out waiting for session '{name}' to detach");
}

/// Ask the outer attach client for `from` to detach that session and attach to `to`.
///
/// By construction this never nests a second attach client: the recorded outer
/// client (`client.pid`) receives `SIGUSR1`, detaches `from` (freeing its attach
/// lock), then attaches to `to` on the same TTY. This function waits until `from`
/// is detached and `to` is attached (or times out).
pub fn request_attach_switch(base: &Path, from: &str, to: &str) -> Result<()> {
    validate_session_name(from)?;
    validate_session_name(to)?;
    if from == to {
        bail!("already in session '{from}'");
    }
    let from_paths = SessionPaths::for_name(base, from);
    let to_paths = SessionPaths::for_name(base, to);
    if !to_paths.meta.exists() {
        bail!("session '{to}' not found");
    }
    if is_attached(&to_paths) {
        bail!("session '{to}' is already attached");
    }
    let Some(pid) = read_client_pid(&from_paths) else {
        bail!(
            "cannot leave session '{from}': no attach client pid \
             (outer attach is too old or not recorded)"
        );
    };
    if !process_alive(pid) {
        let _ = clear_client_pid(&from_paths);
        bail!("cannot leave session '{from}': attach client pid {pid} is dead");
    }
    write_switch_to(&from_paths, to)?;
    if let Err(e) = signal::kill(Pid::from_raw(pid), Signal::SIGUSR1) {
        let _ = fs::remove_file(from_paths.switch_to_file());
        return Err(e).context(format!("signal attach client pid {pid}"));
    }

    // Wait for the handoff: current session must detach before the target attaches.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut from_free = false;
    while Instant::now() < deadline {
        if !is_attached(&from_paths) {
            from_free = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if !from_free {
        let _ = fs::remove_file(from_paths.switch_to_file());
        bail!("timed out waiting for session '{from}' to detach");
    }

    while Instant::now() < deadline {
        if is_attached(&to_paths) && read_client_pid(&to_paths).is_some() {
            return Ok(());
        }
        if !process_alive(pid) {
            bail!(
                "attach client exited while switching from '{from}' to '{to}' \
                 (target may not be attached)"
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("timed out waiting for session '{to}' to attach after leaving '{from}'");
}

/// Append a line to the session daemon log (best effort).
pub fn append_daemon_log(paths: &SessionPaths, message: &str) {
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.daemon_log)
        .and_then(|mut f| writeln!(f, "{}", message));
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
            ended_unix: None,
            end_reason: None,
        };
        write_meta(&paths, &meta).unwrap();
        let loaded = read_meta(&paths).unwrap();
        assert_eq!(loaded.name, "demo");
        assert_eq!(loaded.pid, 1);
        assert_eq!(loaded.last_active_unix, 0);
    }

    #[test]
    fn concurrent_write_meta_does_not_enoent() {
        let dir = tempdir().unwrap();
        let paths = SessionPaths::for_name(dir.path(), "race");
        let base = SessionMeta {
            name: "race".into(),
            pid: 1,
            shell: "/bin/bash".into(),
            created_unix: 1,
            attached: false,
            last_active_unix: 0,
            ended_unix: None,
            end_reason: None,
        };
        write_meta(&paths, &base).unwrap();

        let mut handles = Vec::new();
        for i in 0..8 {
            let paths = paths.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..40 {
                    let meta = SessionMeta {
                        name: "race".into(),
                        pid: 1,
                        shell: "/bin/bash".into(),
                        created_unix: 1,
                        attached: i % 2 == 0,
                        last_active_unix: (i * 100 + j) as u64,
                        ended_unix: None,
                        end_reason: None,
                    };
                    write_meta(&paths, &meta).expect("concurrent write_meta");
                }
            }));
        }
        for h in handles {
            h.join().expect("writer thread");
        }
        let loaded = read_meta(&paths).unwrap();
        assert_eq!(loaded.name, "race");
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
                ended_unix: None,
                end_reason: None,
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
                ended_unix: None,
                end_reason: None,
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
        let cur = current_session(base, &base.join(ARCHIVE_DIR_NAME))
            .unwrap()
            .expect("current");
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
                    ended_unix: None,
                    end_reason: None,
                },
            )
            .unwrap();
        }
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        // SAFETY: held behind ENV_TEST_LOCK; restored before unlock.
        unsafe {
            std::env::set_var(RESHELL_SESSION_ENV, "mine");
        }
        let cur = current_session(base, &base.join(ARCHIVE_DIR_NAME))
            .unwrap()
            .expect("current");
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
                    ended_unix: None,
                    end_reason: None,
                },
            )
            .unwrap();
        }

        let recent = most_recent_session(base, &base.join(ARCHIVE_DIR_NAME)).unwrap();
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
                ended_unix: None,
                end_reason: None,
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
                ended_unix: None,
                end_reason: None,
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
                ended_unix: None,
                end_reason: None,
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
                ended_unix: None,
                end_reason: None,
            },
        )
        .unwrap();

        let n = cleanup_stale_sessions(base, &base.join(ARCHIVE_DIR_NAME)).unwrap();
        assert!(n >= 2, "expected orphan+dead removed, got {n}");
        assert!(!orphan.exists());
        assert!(!dead.dir.exists());
        // Dead session with meta should be archived.
        let ended = list_ended_sessions(&base.join(ARCHIVE_DIR_NAME)).unwrap();
        assert!(
            ended.iter().any(|e| e.meta.name == "dead"),
            "expected dead session archived: {ended:?}"
        );
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
                        ended_unix: None,
                        end_reason: None,
                    },
                )
                .unwrap();
                kill_session(base, name, &base.join(ARCHIVE_DIR_NAME)).expect("kill_session");
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

    #[test]
    fn kill_archives_history_and_daemon_log() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        ensure_base_dir(base).unwrap();
        let archive = base.join(ARCHIVE_DIR_NAME);
        let name = "keep-me";
        let paths = SessionPaths::for_name(base, name);
        write_meta(
            &paths,
            &SessionMeta {
                name: name.into(),
                pid: i32::MAX - 2,
                shell: "/bin/bash".into(),
                created_unix: 10,
                attached: false,
                last_active_unix: 10,
                ended_unix: None,
                end_reason: None,
            },
        )
        .unwrap();
        write_archive_root(&paths, &archive).unwrap();
        fs::write(&paths.daemon_log, "daemon started\n").unwrap();
        fs::create_dir_all(paths.history_dir()).unwrap();
        fs::write(paths.history_dir().join("0001.txt"), "hello from shell\n").unwrap();

        kill_session(base, name, &archive).unwrap();
        assert!(!paths.dir.exists());
        let ended = list_ended_sessions(&archive).unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].meta.name, name);
        assert_eq!(ended[0].meta.end_reason.as_deref(), Some("killed"));
        assert!(ended[0].paths.daemon_log.exists());
        assert!(ended[0].paths.history_dir().join("0001.txt").exists());
        let found = find_ended_session(&archive, name).unwrap();
        assert_eq!(found.archive_id, ended[0].archive_id);
    }
}
