//! CLI polish: list --json, info, rename, clean, detach-key.
use std::process::Command;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn list_json_and_human_times() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "listed");

    let human = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    assert!(human.status.success());
    let txt = String::from_utf8_lossy(&human.stdout);
    assert!(txt.contains("listed"));
    assert!(
        txt.contains("ago") || txt.contains("s ago") || txt.contains("m ago"),
        "expected relative time in list: {txt}"
    );

    let json = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let txt = String::from_utf8_lossy(&json.stdout);
    assert!(txt.contains("\"name\": \"listed\""), "{txt}");
    assert!(txt.contains("\"attached\": false"), "{txt}");
    assert!(txt.contains("\"pid\":"), "{txt}");

    kill_session(base, "listed");
}

#[test]
fn info_shows_paths() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "info-me");

    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "info", "info-me"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(txt.contains("name:        info-me"));
    assert!(txt.contains("socket:"));
    assert!(txt.contains("daemon_log:"));
    assert!(txt.contains("state:       detached"));

    let json = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "info", "info-me", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    let txt = String::from_utf8_lossy(&json.stdout);
    assert!(txt.contains("\"name\": \"info-me\""), "{txt}");

    kill_session(base, "info-me");
}

#[test]
fn rename_live_session_keeps_shell() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "before");

    let renamed = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "rename",
            "before",
            "after",
        ])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    assert!(!base.join("before").exists());
    assert!(base.join("after/session.sock").exists());

    let sock = wait_sock(base, "after");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut stream, 24, 80);
    write_msg(&mut stream, 1, b"echo RENAMED_OK\n");
    let data = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
    assert!(
        String::from_utf8_lossy(&data).contains("RENAMED_OK"),
        "shell broken after rename: {:?}",
        String::from_utf8_lossy(&data)
    );
    write_msg(&mut stream, 3, &[]);

    let info = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "info", "after"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&info.stdout).contains("name:        after"));

    kill_session(base, "after");
}

#[test]
fn clean_removes_orphan_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    std::fs::create_dir_all(base.join("orphan")).unwrap();
    std::fs::write(base.join("orphan/session.sock"), b"").unwrap();

    let out = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "clean"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(txt.contains("removed"), "{txt}");
    assert!(!base.join("orphan").exists());
}

#[test]
fn completion_prints_shell_script() {
    for shell in ["bash", "zsh", "fish"] {
        let out = Command::new(reshell_bin())
            .args(["completion", shell])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "completion {shell}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let txt = String::from_utf8_lossy(&out.stdout);
        assert!(
            txt.contains("reshell"),
            "completion {shell} missing binary name: {txt}"
        );
    }
}

#[test]
fn detach_key_flag_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    // Invalid key should fail before creating a session.
    let bad = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "--detach-key",
            "not-a-key",
            "list",
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("detach key"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );

    let ok = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "--detach-key",
            "^a",
            "list",
        ])
        .output()
        .unwrap();
    assert!(ok.status.success());
}
