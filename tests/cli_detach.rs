//! `reshell detach` force-detaches another terminal's attach client.
use std::os::fd::{FromRawFd, IntoRawFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::openpty;

mod common;
use common::*;

fn spawn_attach(base: &std::path::Path, name: &str) -> std::process::Child {
    let pty = openpty(None, None).expect("openpty");
    let _master = pty.master;
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

fn wait_detached(base: &std::path::Path, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if list_state(base, name).contains("detached") {
            return;
        }
        thread::sleep(Duration::from_millis(40));
    }
    panic!("session {name} still attached: {:?}", list_state(base, name));
}

#[test]
fn detach_command_frees_attached_session() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "held");

    let mut child = spawn_attach(base, "held");
    wait_client_pid(base, "held", &mut child);
    assert!(list_state(base, "held").contains("attached"));

    // Direct CLI attach must still refuse while held (exclusive).
    let refuse = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach", "held"])
        .output()
        .unwrap();
    assert!(!refuse.status.success());
    assert!(
        String::from_utf8_lossy(&refuse.stderr).contains("already attached"),
        "expected already-attached error: {}",
        String::from_utf8_lossy(&refuse.stderr)
    );

    let detach = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "detach", "held"])
        .output()
        .unwrap();
    assert!(
        detach.status.success(),
        "detach failed: {}",
        String::from_utf8_lossy(&detach.stderr)
    );
    assert!(
        String::from_utf8_lossy(&detach.stdout).contains("detached held"),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&detach.stdout)
    );

    wait_detached(base, "held");
    assert!(
        !base.join("held/client.pid").exists(),
        "client.pid should be cleared after detach"
    );

    // Attach client should exit after SIGHUP.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("attach client did not exit after detach");
        }
        thread::sleep(Duration::from_millis(40));
    }

    // Session still alive and reattachable.
    let sock = wait_sock(base, "held");
    let mut again = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut again, 24, 80);
    write_msg(&mut again, 1, b"echo AFTER_DETACH\n");
    let data = collect_data(&mut again, Instant::now() + Duration::from_secs(2));
    assert!(
        String::from_utf8_lossy(&data).contains("AFTER_DETACH"),
        "reattach after detach failed: {:?}",
        String::from_utf8_lossy(&data)
    );
    write_msg(&mut again, 3, &[]);

    kill_session(base, "held");
}

#[test]
fn detach_already_detached_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "free");

    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "detach", "free"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "detach free failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already detached"),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Short alias `d` works.
    let alias = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "d", "free"])
        .output()
        .unwrap();
    assert!(
        alias.status.success(),
        "d alias failed: {}",
        String::from_utf8_lossy(&alias.stderr)
    );

    kill_session(base, "free");
}

#[test]
fn detach_missing_session_errors() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "detach", "nope"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not found"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
