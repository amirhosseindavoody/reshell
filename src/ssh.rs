//! `reshell ssh` — thin SSH wrapper with remote bootstrap and reconnect.
//!
//! Local client (Linux or Windows):
//! 1. Spawns `ssh -t` to the destination.
//! 2. Remote script ensures a compatible `reshell` (install via pixi if needed),
//!    then creates or attaches a named session daemon on Linux.
//! 3. On SSH / connection failure, enters a reconnect wait with exponential
//!    backoff (1s → 60s). Press `R` to retry immediately, `Q` to quit.
//! 4. Clean detach (remote exit 0) exits locally without reconnecting.
//!
//! The local process does not speak the Unix-socket protocol; the remote
//! `reshell attach` / `new` owns the session.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::nameutil;

/// Default git source used when the remote needs `pixi global install`.
pub const DEFAULT_INSTALL_GIT: &str = "https://github.com/amirhosseindavoody/reshell.git";
/// Default git ref (branch/tag) for remote install.
pub const DEFAULT_INSTALL_REF: &str = "main";

/// Initial reconnect delay; doubles each attempt up to [`MAX_RECONNECT_DELAY`].
pub const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Cap for reconnect backoff (one minute).
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

/// Options for `reshell ssh`.
#[derive(Debug, Clone)]
pub struct SshOpts {
    /// Session name on the remote (generated once if omitted; reused on reconnect).
    pub name: Option<String>,
    /// Shell for a newly created remote session.
    pub shell: Option<String>,
    /// Detach key forwarded to the remote client (`--detach-key`).
    pub detach_key: String,
    /// Skip pixi install when remote reshell is missing/incompatible.
    pub no_install: bool,
    /// Git URL for `pixi global install --git`.
    pub install_git: String,
    /// Git branch/tag for install (`--branch`).
    pub install_ref: String,
    /// Destination from ssh config / `user@host` (optional if present in `ssh_args`).
    pub destination: Option<String>,
    /// Extra arguments forwarded to `ssh` (may include the host when `destination` is None).
    pub ssh_args: Vec<String>,
    /// Override `ssh` binary (tests).
    pub ssh_bin: Option<PathBuf>,
    /// Expected remote version (defaults to this binary's version).
    pub expected_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    RetryNow,
    Quit,
    TimedOut,
}

/// Run the SSH relay loop: connect, and on failure reconnect with backoff.
pub fn run(opts: SshOpts) -> Result<()> {
    let version = opts
        .expected_version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let name = match opts.name.clone() {
        Some(n) => {
            nameutil::validate_session_name(&n)?;
            n
        }
        None => nameutil::generate_session_name(),
    };
    let shell = opts.shell.clone().unwrap_or_else(|| "/bin/zsh".into());
    let ssh_bin = opts
        .ssh_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("ssh"));

    if opts.destination.is_none() && opts.ssh_args.is_empty() {
        bail!("ssh destination required (e.g. `reshell ssh myserver` or `reshell ssh -- user@host`)");
    }

    // Validate detach-key syntax early (forwarded as a string to the remote).
    let _ = crate::protocol::parse_detach_key(&opts.detach_key)?;

    let remote_cmd = build_remote_command(RemoteBootstrap {
        version: &version,
        name: &name,
        shell: &shell,
        detach_key: &opts.detach_key,
        no_install: opts.no_install,
        install_git: &opts.install_git,
        install_ref: &opts.install_ref,
    });

    eprintln!(
        "reshell ssh: session '{name}' via ssh (detach key {}); reconnect: R, quit: Q",
        opts.detach_key
    );

    let mut delay = INITIAL_RECONNECT_DELAY;
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        if attempt > 1 {
            eprintln!("reshell ssh: reconnecting to session '{name}' (attempt {attempt})…");
        }

        let status = run_ssh_once(
            &ssh_bin,
            opts.destination.as_deref(),
            &opts.ssh_args,
            &remote_cmd,
        )?;

        if status.success() {
            // Clean detach / remote exit 0 — do not reconnect.
            return Ok(());
        }

        let code = status.code();
        eprintln!(
            "reshell ssh: connection ended ({})",
            exit_status_label(status)
        );

        // Non-interactive stdin: fail after the first drop (scripts/CI).
        if !stdin_is_tty() {
            bail!(
                "ssh exited with {}; reconnect requires a TTY (press R while waiting)",
                code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
            );
        }

        match wait_for_reconnect(delay)? {
            WaitOutcome::Quit => {
                eprintln!("reshell ssh: quit");
                return Ok(());
            }
            WaitOutcome::RetryNow | WaitOutcome::TimedOut => {}
        }

        delay = next_reconnect_delay(delay);
    }
}

/// Inputs for the remote bootstrap + attach script.
#[derive(Debug, Clone, Copy)]
pub struct RemoteBootstrap<'a> {
    pub version: &'a str,
    pub name: &'a str,
    pub shell: &'a str,
    pub detach_key: &'a str,
    pub no_install: bool,
    pub install_git: &'a str,
    pub install_ref: &'a str,
}

/// Build the remote command that ensures reshell and attaches.
pub fn build_remote_command(b: RemoteBootstrap<'_>) -> String {
    // Script is transported as base64 so SSH quoting stays simple and Windows
    // OpenSSH clients do not mangle nested quotes.
    let script = remote_bootstrap_script(b);
    let b64 = base64_encode(script.as_bytes());
    // Do not prepend ~/.pixi/bin here — preserve the caller's PATH (and tests).
    // The script appends pixi bin as a fallback and prepends after install.
    format!("echo {b64} | base64 -d | bash")
}

/// Shell script run on the Linux server after SSH login.
pub fn remote_bootstrap_script(b: RemoteBootstrap<'_>) -> String {
    let no_install = if b.no_install { "1" } else { "0" };
    format!(
        r#"set -euo pipefail
EXPECTED={expected}
NAME={name}
SHELL_PATH={shell}
DETACH_KEY={detach}
NO_INSTALL={no_install}
INSTALL_GIT={git}
INSTALL_REF={git_ref}

export PATH="$PATH:$HOME/.pixi/bin"

version_of() {{
  # clap: "reshell 2026.7.17+0"
  "$1" --version 2>/dev/null | awk '{{print $NF; exit}}'
}}

find_reshell() {{
  if command -v reshell >/dev/null 2>&1; then
    command -v reshell
    return 0
  fi
  if [ -x "$HOME/.pixi/bin/reshell" ]; then
    echo "$HOME/.pixi/bin/reshell"
    return 0
  fi
  return 1
}}

install_reshell() {{
  if [ "$NO_INSTALL" = "1" ]; then
    echo "reshell ssh: remote reshell missing or incompatible (expected $EXPECTED); pass without --no-install to auto-install" >&2
    exit 1
  fi
  if ! command -v pixi >/dev/null 2>&1; then
    echo "reshell ssh: pixi not found on remote; install pixi (https://pixi.sh) or install reshell manually" >&2
    exit 1
  fi
  echo "reshell ssh: installing reshell $EXPECTED via pixi global install…" >&2
  pixi global install --git "$INSTALL_GIT" --branch "$INSTALL_REF" reshell
  export PATH="$HOME/.pixi/bin:$PATH"
}}

BIN=""
if BIN=$(find_reshell); then
  HAVE_VER=$(version_of "$BIN" || true)
  if [ "$HAVE_VER" != "$EXPECTED" ]; then
    echo "reshell ssh: remote reshell version '$HAVE_VER' != expected '$EXPECTED'" >&2
    install_reshell
    BIN=$(find_reshell) || {{ echo "reshell ssh: reshell not found after install" >&2; exit 1; }}
    HAVE_VER=$(version_of "$BIN" || true)
    if [ "$HAVE_VER" != "$EXPECTED" ]; then
      echo "reshell ssh: warning: remote version is '$HAVE_VER' (wanted '$EXPECTED'); continuing" >&2
    fi
  fi
else
  echo "reshell ssh: reshell not found on remote" >&2
  install_reshell
  BIN=$(find_reshell) || {{ echo "reshell ssh: reshell not found after install" >&2; exit 1; }}
fi

# Create the session daemon if needed, then attach (TTY is this SSH session).
if "$BIN" info "$NAME" >/dev/null 2>&1; then
  echo "reshell ssh: attaching to remote session '$NAME'" >&2
  exec "$BIN" --detach-key "$DETACH_KEY" attach "$NAME"
else
  echo "reshell ssh: creating remote session '$NAME'" >&2
  exec "$BIN" --detach-key "$DETACH_KEY" new "$NAME" --shell "$SHELL_PATH"
fi
"#,
        expected = sh_quote(b.version),
        name = sh_quote(b.name),
        shell = sh_quote(b.shell),
        detach = sh_quote(b.detach_key),
        no_install = no_install,
        git = sh_quote(b.install_git),
        git_ref = sh_quote(b.install_ref),
    )
}

/// Next backoff delay (double, capped at one minute).
pub fn next_reconnect_delay(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > MAX_RECONNECT_DELAY {
        MAX_RECONNECT_DELAY
    } else {
        doubled
    }
}

/// Parse `reshell --version` stdout into the version token.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_reshell_version_output(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .last()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn run_ssh_once(
    ssh_bin: &PathBuf,
    destination: Option<&str>,
    ssh_args: &[String],
    remote_cmd: &str,
) -> Result<ExitStatus> {
    let mut cmd = Command::new(ssh_bin);
    cmd.arg("-t");
    for a in ssh_args {
        cmd.arg(a);
    }
    if let Some(dest) = destination {
        cmd.arg(dest);
    }
    cmd.arg(remote_cmd);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", ssh_bin.display()))?;
    let status = child.wait().context("wait for ssh")?;
    Ok(status)
}

fn exit_status_label(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit {code}")
    } else {
        "terminated by signal".into()
    }
}

fn classify_key(b: u8) -> Option<WaitOutcome> {
    match b {
        b'r' | b'R' => Some(WaitOutcome::RetryNow),
        b'q' | b'Q' | 0x03 => Some(WaitOutcome::Quit),
        _ => None,
    }
}

fn print_reconnect_status(left: Duration) {
    let secs = left.as_secs().max(1);
    eprint!("\r\x1b[Kreshell ssh: reconnecting in {secs}s…  (R retry now, Q quit)");
    let _ = io::stderr().flush();
}

fn clear_status_line() {
    eprint!("\r\x1b[K");
    let _ = io::stderr().flush();
}

/// Single-quote a string for safe embedding in a POSIX shell script.
pub fn sh_quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Minimal base64 encoder (no extra dependency).
pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(unix)]
mod tty {
    use super::*;
    use std::io::ErrorKind;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
    use std::sync::atomic::{AtomicBool, Ordering};

    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags};
    use nix::sys::termios::{
        tcgetattr, tcsetattr, LocalFlags, SetArg, SpecialCharacterIndices, Termios,
    };
    use nix::unistd::{isatty, read as nix_read};

    static INTERRUPT_FLAG: AtomicBool = AtomicBool::new(false);

    extern "C" fn handle_interrupt(_: nix::libc::c_int) {
        INTERRUPT_FLAG.store(true, Ordering::Relaxed);
    }

    pub fn stdin_is_tty() -> bool {
        isatty(io::stdin().as_raw_fd()).unwrap_or(false)
    }

    pub fn wait_for_reconnect(delay: Duration) -> Result<WaitOutcome> {
        let stdin_fd = io::stdin().as_raw_fd();
        let orig = tcgetattr(io::stdin().as_fd()).context("tcgetattr")?;
        let mut raw = orig.clone();
        raw.local_flags.remove(LocalFlags::ICANON | LocalFlags::ECHO);
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        tcsetattr(io::stdin().as_fd(), SetArg::TCSANOW, &raw).context("tcsetattr reconnect")?;
        let _guard = TermiosGuard {
            fd: stdin_fd,
            termios: orig,
        };

        INTERRUPT_FLAG.store(false, Ordering::Relaxed);
        unsafe {
            let _ = nix::sys::signal::signal(
                nix::sys::signal::Signal::SIGINT,
                nix::sys::signal::SigHandler::Handler(handle_interrupt),
            );
        }

        let deadline = Instant::now() + delay;
        let mut last_print = Instant::now() - Duration::from_secs(2);

        loop {
            if INTERRUPT_FLAG.load(Ordering::Relaxed) {
                eprintln!();
                return Ok(WaitOutcome::Quit);
            }

            let now = Instant::now();
            if now >= deadline {
                clear_status_line();
                return Ok(WaitOutcome::TimedOut);
            }

            if now.duration_since(last_print) >= Duration::from_millis(200) {
                print_reconnect_status(deadline.saturating_duration_since(now));
                last_print = now;
            }

            let mut fds = [PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) },
                PollFlags::POLLIN,
            )];
            let wait = Duration::from_millis(100)
                .min(deadline.saturating_duration_since(Instant::now()));
            let wait_ms = wait.as_millis().min(i32::MAX as u128) as i32;
            match poll(&mut fds, wait_ms as u16) {
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e).context("poll stdin"),
            }

            if fds[0]
                .revents()
                .map(|r| r.contains(PollFlags::POLLIN))
                .unwrap_or(false)
            {
                let mut buf = [0u8; 32];
                match nix_read(stdin_fd, &mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if let Some(out) = classify_key(b) {
                                clear_status_line();
                                return Ok(out);
                            }
                        }
                    }
                    Err(Errno::EAGAIN) | Err(Errno::EINTR) => {}
                    Err(e) => {
                        let err = io::Error::from(e);
                        if err.kind() != ErrorKind::WouldBlock {
                            return Err(err).context("read stdin");
                        }
                    }
                }
            }
        }
    }

    struct TermiosGuard {
        fd: i32,
        termios: Termios,
    }

    impl Drop for TermiosGuard {
        fn drop(&mut self) {
            let fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
            let _ = tcsetattr(fd, SetArg::TCSAFLUSH, &self.termios);
            unsafe {
                let _ = nix::sys::signal::signal(
                    nix::sys::signal::Signal::SIGINT,
                    nix::sys::signal::SigHandler::SigDfl,
                );
            }
        }
    }
}

#[cfg(windows)]
mod tty {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    pub fn stdin_is_tty() -> bool {
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() || handle == (-1isize as _) {
                return false;
            }
            let mut mode = 0u32;
            GetConsoleMode(handle, &mut mode) != 0
        }
    }

    pub fn wait_for_reconnect(delay: Duration) -> Result<WaitOutcome> {
        let handle = io::stdin().as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut orig_mode = 0u32;
        let has_console = unsafe { GetConsoleMode(handle, &mut orig_mode) != 0 };
        if has_console {
            let raw = orig_mode
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            unsafe {
                SetConsoleMode(handle, raw);
            }
        }
        let _guard = ConsoleModeGuard {
            handle,
            mode: orig_mode,
            restore: has_console,
        };

        let deadline = Instant::now() + delay;
        let mut last_print = Instant::now() - Duration::from_secs(2);

        loop {
            let now = Instant::now();
            if now >= deadline {
                clear_status_line();
                return Ok(WaitOutcome::TimedOut);
            }

            if now.duration_since(last_print) >= Duration::from_millis(200) {
                print_reconnect_status(deadline.saturating_duration_since(now));
                last_print = now;
            }

            let wait = Duration::from_millis(100)
                .min(deadline.saturating_duration_since(Instant::now()));
            let wait_ms = wait.as_millis().min(u32::MAX as u128) as u32;
            let wr = unsafe { WaitForSingleObject(handle, wait_ms) };
            if wr != WAIT_OBJECT_0 {
                continue;
            }

            let mut buf = [0u8; 32];
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                continue;
            }
            for &b in &buf[..read as usize] {
                if let Some(out) = classify_key(b) {
                    clear_status_line();
                    return Ok(out);
                }
            }
        }
    }

    struct ConsoleModeGuard {
        handle: windows_sys::Win32::Foundation::HANDLE,
        mode: u32,
        restore: bool,
    }

    impl Drop for ConsoleModeGuard {
        fn drop(&mut self) {
            if self.restore {
                unsafe {
                    SetConsoleMode(self.handle, self.mode);
                }
            }
            // Do not CloseHandle stdin.
        }
    }
}

use tty::{stdin_is_tty, wait_for_reconnect};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(2)),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(32)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn parse_version() {
        assert_eq!(
            parse_reshell_version_output("reshell 2026.7.17+0\n").as_deref(),
            Some("2026.7.17+0")
        );
        assert_eq!(parse_reshell_version_output("").as_deref(), None);
    }

    #[test]
    fn sh_quote_escapes_quotes() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn base64_roundtrip_known() {
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn remote_script_mentions_version_and_pixi() {
        let script = remote_bootstrap_script(RemoteBootstrap {
            version: "2026.7.17+0",
            name: "demo",
            shell: "/bin/bash",
            detach_key: "^\\",
            no_install: false,
            install_git: DEFAULT_INSTALL_GIT,
            install_ref: DEFAULT_INSTALL_REF,
        });
        assert!(script.contains("2026.7.17+0"));
        assert!(script.contains("pixi global install"));
        assert!(script.contains("demo"));
        assert!(script.contains("/bin/bash"));
        let cmd = build_remote_command(RemoteBootstrap {
            version: "2026.7.17+0",
            name: "demo",
            shell: "/bin/bash",
            detach_key: "^\\",
            no_install: false,
            install_git: DEFAULT_INSTALL_GIT,
            install_ref: DEFAULT_INSTALL_REF,
        });
        assert!(cmd.contains("base64 -d"));
    }

    #[test]
    fn remote_script_no_install_flag() {
        let script = remote_bootstrap_script(RemoteBootstrap {
            version: "1.0.0",
            name: "s",
            shell: "/bin/zsh",
            detach_key: "^a",
            no_install: true,
            install_git: DEFAULT_INSTALL_GIT,
            install_ref: "main",
        });
        assert!(script.contains("NO_INSTALL=1"));
    }
}
