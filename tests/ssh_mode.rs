//! `reshell ssh` integration: fake ssh binary + remote bootstrap.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

mod common;
use common::*;

fn write_fake_ssh(dir: &std::path::Path, behavior: &str) -> std::path::PathBuf {
    let path = dir.join("fake-ssh");
    // Last argv is the remote command (matches `ssh -t … remote_cmd`).
    let script = format!(
        r#"#!/bin/bash
set -euo pipefail
log="{log}"
printf '%s\n' "$@" > "$log"
remote="${{@: -1}}"
case "{behavior}" in
  run)
    exec bash -c "$remote"
    ;;
  fail)
    echo "fake-ssh: simulated connection failure" >&2
    exit 255
    ;;
  *)
    echo "unknown behavior" >&2
    exit 99
    ;;
esac
"#,
        log = dir.join("ssh-args.txt").display(),
        behavior = behavior,
    );
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

#[test]
fn ssh_help_lists_reconnect_and_install() {
    let out = Command::new(reshell_bin())
        .args(["ssh", "--help"])
        .output()
        .expect("help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("pixi") || text.contains("reconnect") || text.contains("SSH"));
    assert!(text.contains("--no-install"));
    assert!(text.contains("-n"));
}

#[test]
fn ssh_requires_destination() {
    let out = Command::new(reshell_bin())
        .args(["ssh"])
        .output()
        .expect("ssh no dest");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("destination") || err.contains("required"),
        "stderr={err}"
    );
}

#[test]
fn ssh_bootstrap_creates_remote_session_via_fake_ssh() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("sessions");
    fs::create_dir_all(&base).unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // Put real reshell on PATH as `reshell` for the remote script.
    std::os::unix::fs::symlink(reshell_bin(), bin_dir.join("reshell")).unwrap();
    let fake_ssh = write_fake_ssh(tmp.path(), "run");
    std::os::unix::fs::symlink(&fake_ssh, bin_dir.join("ssh")).unwrap();

    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // Attach needs a TTY; without one, `new` still creates the daemon then fails.
    let out = Command::new(reshell_bin())
        .env("PATH", &path_env)
        .env("RESHELL_DIR", &base)
        .args([
            "ssh",
            "-n",
            "ssh-demo",
            "--no-install",
            "--shell",
            "/bin/bash",
            "localhost",
        ])
        .output()
        .expect("run reshell ssh");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        base.join("ssh-demo/session.sock").exists(),
        "expected session daemon; status={:?} out={combined}",
        out.status
    );

    // Fake ssh was invoked with -t and our destination.
    let args = fs::read_to_string(tmp.path().join("ssh-args.txt")).unwrap_or_default();
    assert!(args.contains("-t"), "ssh args missing -t: {args}");
    assert!(args.contains("localhost"), "ssh args missing host: {args}");
    assert!(
        args.contains("base64"),
        "remote command missing bootstrap: {args}"
    );

    kill_session(&base, "ssh-demo");
}

#[test]
fn ssh_non_tty_does_not_reconnect_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_ssh = write_fake_ssh(tmp.path(), "fail");
    std::os::unix::fs::symlink(&fake_ssh, bin_dir.join("ssh")).unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let start = std::time::Instant::now();
    let out = Command::new(reshell_bin())
        .env("PATH", &path_env)
        .args(["ssh", "-n", "x", "--no-install", "nowhere"])
        .output()
        .expect("ssh fail");
    assert!(!out.status.success());
    // Should fail fast (no reconnect wait) because stdin is not a TTY.
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "took too long — reconnect loop? {:?}",
        start.elapsed()
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("reconnect requires a TTY")
            || err.contains("connection ended")
            || err.contains("255"),
        "stderr={err}"
    );
}

#[test]
fn ssh_forwards_extra_args_after_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_ssh = write_fake_ssh(tmp.path(), "fail");
    std::os::unix::fs::symlink(&fake_ssh, bin_dir.join("ssh")).unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let _ = Command::new(reshell_bin())
        .env("PATH", &path_env)
        .args(["ssh", "-n", "y", "--no-install", "myhost", "-p", "2222"])
        .output()
        .expect("ssh");

    let args = fs::read_to_string(tmp.path().join("ssh-args.txt")).unwrap();
    assert!(args.lines().any(|l| l == "-p"), "args={args}");
    assert!(args.lines().any(|l| l == "2222"), "args={args}");
    assert!(args.lines().any(|l| l == "myhost"), "args={args}");
}
