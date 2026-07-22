//! `reshell context` — last command + trailing output without attaching.
use std::process::Command;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn context_shows_recent_output_and_last_command() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "ctx");

    let sock = wait_sock(base, "ctx");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut stream, 24, 80);

    // Emit OSC 633 command markers (as VS Code shell integration would) plus output.
    write_msg(
        &mut stream,
        1,
        b"printf '\\033]633;E;echo hi from ctx\\007'; echo hi from ctx; printf '\\033]633;D;0\\007'\n",
    );
    let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
    // Keep the attach client connected so context must work without the lock.
    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "context", "ctx"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "context failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(
        txt.contains("last_command: echo hi from ctx"),
        "expected last_command, got: {txt}"
    );
    assert!(
        txt.contains("hi from ctx"),
        "expected output line, got: {txt}"
    );
    assert!(
        txt.contains("session: ctx"),
        "expected session name, got: {txt}"
    );

    let json = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "context", "ctx", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let jtxt = String::from_utf8_lossy(&json.stdout);
    assert!(jtxt.contains("\"last_command\": \"echo hi from ctx\""), "{jtxt}");
    assert!(jtxt.contains("\"last_exit_code\": 0"), "{jtxt}");

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "ctx");
}

#[test]
fn context_defaults_to_current_session_env() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "other");
    new_detached(base, "mine");

    let sock = wait_sock(base, "mine");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut stream, 24, 80);
    write_msg(&mut stream, 1, b"echo ONLY_MINE\n");
    let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
    write_msg(&mut stream, 3, &[]);
    drop(stream);

    let out = Command::new(reshell_bin())
        .env("RESHELL_SESSION", "mine")
        .args(["--dir", base.to_str().unwrap(), "context"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(txt.contains("session: mine"), "{txt}");
    assert!(txt.contains("ONLY_MINE"), "{txt}");

    kill_session(base, "other");
    kill_session(base, "mine");
}
