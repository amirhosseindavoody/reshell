//! `reshell new` defaults to attach; `--detach` creates without attaching.
//! `reshell attach` with no name picks the most recently active session
//! (or creates one if none exist). Bare `reshell` aliases `attach`.
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

mod common;
use common::*;

fn protocol_attach_touch(base: &std::path::Path, name: &str) {
    let sock = base.join(name).join("session.sock");
    let mut stream = UnixStream::connect(&sock).expect("connect");
    attach_winsize(&mut stream, 24, 80);
    // Brief I/O so the daemon marks the session attached then we detach.
    write_msg(&mut stream, 1, b"true\n");
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    let _ = read_msg(&mut stream);
    write_msg(&mut stream, 3, &[]);
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn new_without_detach_requires_tty() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let out = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            "needs-tty",
            "--shell",
            "/bin/bash",
        ])
        .output()
        .expect("run reshell new");
    // Session is created, then attach fails because stdin is not a TTY.
    assert!(
        !out.status.success(),
        "expected attach failure without TTY"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not a tty") || err.contains("tty"),
        "unexpected stderr: {err}"
    );
    // Daemon should still be running (created before attach failed).
    assert!(base.join("needs-tty/session.sock").exists());
    kill_session(base, "needs-tty");
}

#[test]
fn attach_without_name_picks_most_recent() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "first");
    std::thread::sleep(Duration::from_millis(50));
    new_detached(base, "second");
    // `last_active_unix` is second-granularity; wait until the clock ticks so
    // touching `first` is strictly newer than `second`'s create/activity time.
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        == before
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    protocol_attach_touch(base, "first");

    // Named attach without TTY fails the same way — proves the session exists.
    let named = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach", "first"])
        .output()
        .unwrap();
    assert!(!named.status.success());
    assert!(
        String::from_utf8_lossy(&named.stderr).contains("tty"),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );

    let unnamed = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach"])
        .output()
        .unwrap();
    assert!(
        !unnamed.status.success(),
        "attach with no name should reach attach and fail on missing TTY"
    );
    let err = String::from_utf8_lossy(&unnamed.stderr);
    assert!(
        err.contains("attaching to first"),
        "expected most-recent resolution to first, got: {err}"
    );
    assert!(
        err.contains("tty"),
        "expected TTY error after resolving most-recent session, got: {err}"
    );

    // With only `second` left after killing `first`, bare attach still works.
    kill_session(base, "first");
    let only_second = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach"])
        .output()
        .unwrap();
    assert!(!only_second.status.success());
    assert!(String::from_utf8_lossy(&only_second.stderr).contains("tty"));

    kill_session(base, "second");

    // No sessions left: bare attach must not error with "no sessions" — it
    // creates a new session (same as `new`). Default shell may be missing in
    // CI, so only assert we left the "no sessions" path.
    let empty = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach"])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    let err = String::from_utf8_lossy(&empty.stderr);
    assert!(
        !err.contains("no sessions"),
        "bare attach with no sessions should create one, got: {err}"
    );

    // Clean up anything the fallback created, then seed a known session.
    let listed = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    for line in String::from_utf8_lossy(&listed.stdout).lines().skip(1) {
        let name = line.split_whitespace().next().unwrap_or("");
        if !name.is_empty() && name != "(no" {
            kill_session(base, name);
        }
    }

    new_detached(base, "alias-target");

    // Bare `reshell` (no subcommand) is an alias for attach.
    let bare = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!bare.status.success());
    let bare_err = String::from_utf8_lossy(&bare.stderr);
    assert!(
        bare_err.contains("attaching to alias-target"),
        "bare reshell should attach to the recent session, got: {bare_err}"
    );

    kill_session(base, "alias-target");
}
