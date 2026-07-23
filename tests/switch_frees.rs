//! In-session leave-and-join always frees the original attach lock.
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::openpty;

mod common;
use common::*;

fn spawn_attach(base: &std::path::Path, name: &str) -> std::process::Child {
    let pty = openpty(None, None).expect("openpty");
    let _master = pty.master; // keep PTY alive for the child lifetime via leak
    std::mem::forget(_master);
    let slave_fd = pty.slave.into_raw_fd();
    let slave_in = unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap()) };
    let slave_out = unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap()) };
    let _ = nix::unistd::close(slave_fd);

    let base_str = base.to_str().unwrap().to_string();
    Command::new(reshell_bin())
        .args(["--dir", &base_str, "attach", name])
        .stdin(slave_in)
        .stdout(slave_out)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn attach")
}

fn wait_client_pid(base: &std::path::Path, name: &str, child: &mut std::process::Child) {
    let client_pid_path = base.join(name).join("client.pid");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if client_pid_path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "expected {name}/client.pid after attach; child status {:?}",
        child.try_wait()
    );
}

fn list_state(base: &std::path::Path, name: &str) -> String {
    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.split_whitespace().next() == Some(name))
        .unwrap_or("")
        .to_string()
}

fn wait_states(base: &std::path::Path, from: &str, to: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut from_free = false;
    let mut to_held = false;
    while Instant::now() < deadline {
        from_free = list_state(base, from).contains("detached");
        to_held = list_state(base, to).contains("attached");
        if from_free && to_held {
            return;
        }
        thread::sleep(Duration::from_millis(40));
    }
    panic!(
        "handoff incomplete: {from}={:?} {to}={:?}",
        list_state(base, from),
        list_state(base, to)
    );
}

#[test]
fn attach_from_inside_frees_original_session() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "alpha");
    new_detached(base, "beta");

    let mut child = spawn_attach(base, "alpha");
    wait_client_pid(base, "alpha", &mut child);
    assert!(list_state(base, "alpha").contains("attached"));
    assert!(list_state(base, "beta").contains("detached"));

    let switch = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach", "beta"])
        .env("RESHELL_SESSION", "alpha")
        .output()
        .expect("request switch");
    assert!(
        switch.status.success(),
        "switch request failed: {}",
        String::from_utf8_lossy(&switch.stderr)
    );
    let err = String::from_utf8_lossy(&switch.stderr);
    assert!(
        err.contains("switching from alpha to beta"),
        "unexpected stderr: {err}"
    );

    wait_states(base, "alpha", "beta");
    assert!(
        !base.join("alpha/client.pid").exists(),
        "alpha client.pid should be cleared"
    );
    assert!(
        base.join("beta/client.pid").exists(),
        "beta client.pid should be written"
    );

    let _ = child.kill();
    let _ = child.wait();
    kill_session(base, "alpha");
    kill_session(base, "beta");
}

#[test]
fn new_from_inside_frees_original_session() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "alpha");

    let mut child = spawn_attach(base, "alpha");
    wait_client_pid(base, "alpha", &mut child);
    assert!(list_state(base, "alpha").contains("attached"));

    // `reshell new beta` from inside alpha must leave alpha and join beta —
    // never nest a second attach client on top of alpha.
    let created = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            "beta",
            "--shell",
            "/bin/bash",
        ])
        .env("RESHELL_SESSION", "alpha")
        .output()
        .expect("new from inside");
    assert!(
        created.status.success(),
        "new-from-inside failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let err = String::from_utf8_lossy(&created.stderr);
    assert!(
        err.contains("switching from alpha to beta"),
        "unexpected stderr: {err}"
    );

    wait_states(base, "alpha", "beta");
    assert!(
        !base.join("alpha/client.pid").exists(),
        "alpha client.pid should be cleared after new"
    );
    assert!(
        base.join("beta/client.pid").exists(),
        "beta client.pid should be written after new"
    );

    let _ = child.kill();
    let _ = child.wait();
    kill_session(base, "alpha");
    kill_session(base, "beta");
}

#[test]
fn attach_same_session_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "alpha");

    let mut child = spawn_attach(base, "alpha");
    wait_client_pid(base, "alpha", &mut child);

    let again = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach", "alpha"])
        .env("RESHELL_SESSION", "alpha")
        .output()
        .expect("attach same");
    assert!(
        again.status.success(),
        "same-session attach should succeed: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    let err = String::from_utf8_lossy(&again.stderr);
    assert!(
        err.contains("already in session 'alpha'"),
        "unexpected stderr: {err}"
    );
    // Original attach must still hold alpha.
    assert!(list_state(base, "alpha").contains("attached"));
    assert!(base.join("alpha/client.pid").exists());

    let _ = child.kill();
    let _ = child.wait();
    kill_session(base, "alpha");
}
