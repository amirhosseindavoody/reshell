//! In-session switch frees the original attach lock.
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::openpty;

mod common;
use common::*;

#[test]
fn switch_from_inside_frees_original_session() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "alpha");
    new_detached(base, "beta");
    let _ = wait_sock(base, "alpha");
    let _ = wait_sock(base, "beta");

    let pty = openpty(None, None).expect("openpty");
    let _master = pty.master; // keep PTY alive
    let slave_fd = pty.slave.into_raw_fd();
    let slave_in = unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap()) };
    let slave_out = unsafe { Stdio::from_raw_fd(nix::unistd::dup(slave_fd).unwrap()) };
    let _ = nix::unistd::close(slave_fd);

    let base_str = base.to_str().unwrap().to_string();
    let mut child = Command::new(reshell_bin())
        .args(["--dir", &base_str, "attach", "alpha"])
        .stdin(slave_in)
        .stdout(slave_out)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn attach alpha");

    // Wait until the daemon recorded the attach client's pid.
    let client_pid_path = base.join("alpha/client.pid");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if client_pid_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        client_pid_path.exists(),
        "expected alpha/client.pid after attach; child status {:?}",
        child.try_wait()
    );
    assert!(
        list_state(base, "alpha").contains("attached"),
        "alpha should be attached"
    );
    assert!(
        list_state(base, "beta").contains("detached"),
        "beta should start detached"
    );

    // Simulate the in-session picker asking the outer client to switch.
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

    // Outer client should detach alpha and attach beta.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut alpha_free = false;
    let mut beta_held = false;
    while Instant::now() < deadline {
        let alpha = list_state(base, "alpha");
        let beta = list_state(base, "beta");
        alpha_free = alpha.contains("detached");
        beta_held = beta.contains("attached");
        if alpha_free && beta_held {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    assert!(alpha_free, "alpha should be freed after switch");
    assert!(beta_held, "beta should be attached after switch");
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
