//! Attach exclusivity, stale lock recovery, and kill paths.
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn second_attach_is_rejected_while_first_holds() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "exclusive");
    let sock = wait_sock(base, "exclusive");

    let mut first = UnixStream::connect(&sock).expect("first connect");
    attach_winsize(&mut first, 24, 80);
    // Give the daemon time to take the attach flock / mark attached.
    thread::sleep(Duration::from_millis(100));

    let list = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    let list_txt = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_txt.contains("attached"),
        "expected attached state: {list_txt}"
    );

    // Second connection should be accepted then immediately closed.
    let mut second = UnixStream::connect(&sock).expect("second connect");
    second
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut buf = [0u8; 8];
    match second.read(&mut buf) {
        Ok(0) => {} // peer closed — expected
        Ok(n) => panic!("second attach should be closed, got {n} bytes"),
        Err(e) => panic!("unexpected error on second attach: {e}"),
    }

    // CLI attach should also refuse while the lock is held.
    let cli = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach", "exclusive"])
        .output()
        .unwrap();
    assert!(!cli.status.success());
    let err = String::from_utf8_lossy(&cli.stderr);
    assert!(
        err.contains("already attached"),
        "expected already-attached error, got: {err}"
    );

    write_msg(&mut first, 3, &[]);
    drop(first);
    thread::sleep(Duration::from_millis(100));

    // After detach, a new protocol attach should succeed.
    let mut again = UnixStream::connect(&sock).expect("reattach");
    attach_winsize(&mut again, 24, 80);
    write_msg(&mut again, 1, b"echo AFTER_EXCLUSIVE\n");
    let data = collect_data(&mut again, Instant::now() + Duration::from_secs(2));
    assert!(
        String::from_utf8_lossy(&data).contains("AFTER_EXCLUSIVE"),
        "reattach failed: {:?}",
        String::from_utf8_lossy(&data)
    );
    write_msg(&mut again, 3, &[]);
    kill_session(base, "exclusive");
}

#[test]
fn concurrent_connects_only_one_survives() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "race");
    let sock = wait_sock(base, "race");

    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let sock = sock.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let mut stream = UnixStream::connect(&sock).expect("connect");
            attach_winsize(&mut stream, 24, 80);
            stream
                .set_read_timeout(Some(Duration::from_millis(800)))
                .unwrap();
            // Surviving client should receive attach restore Data.
            // Rejected client sees EOF immediately.
            let mut alive = false;
            let deadline = Instant::now() + Duration::from_millis(800);
            while Instant::now() < deadline {
                match read_msg(&mut stream) {
                    Some((1, _)) => {
                        alive = true;
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            if alive {
                write_msg(&mut stream, 3, &[]);
            }
            alive
        }));
    }
    barrier.wait();

    let results: Vec<bool> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();
    let survivors = results.iter().filter(|&&a| a).count();
    assert_eq!(
        survivors, 1,
        "exactly one concurrent attach should survive, got {results:?}"
    );

    kill_session(base, "race");
}

#[test]
fn stale_attached_file_is_cleared_on_list_and_attach() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "stale-lock");

    let lock = base.join("stale-lock/attached");
    File::create(&lock).unwrap();
    // Also force meta.attached = true without a live flock holder.
    let meta_path = base.join("stale-lock/meta.json");
    let meta = fs::read_to_string(&meta_path).unwrap();
    let patched = meta.replace("\"attached\": false", "\"attached\": true");
    fs::write(&meta_path, patched).unwrap();
    assert!(lock.exists());

    let list = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    let list_txt = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_txt.contains("detached"),
        "stale lock should be recovered to detached: {list_txt}"
    );
    assert!(
        !lock.exists(),
        "stale attached file should be removed by list"
    );

    // Recreate a stale lock and confirm protocol attach still works (daemon
    // takes a fresh flock).
    File::create(&lock).unwrap();
    let sock = wait_sock(base, "stale-lock");
    let mut stream = UnixStream::connect(&sock).expect("connect");
    attach_winsize(&mut stream, 24, 80);
    write_msg(&mut stream, 1, b"echo STALE_OK\n");
    let data = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
    assert!(
        String::from_utf8_lossy(&data).contains("STALE_OK"),
        "attach after stale lock failed: {:?}",
        String::from_utf8_lossy(&data)
    );
    write_msg(&mut stream, 3, &[]);
    kill_session(base, "stale-lock");
}

#[test]
fn kill_removes_session_and_reports_missing() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "kill-me");

    let kill = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "kill-me"])
        .output()
        .unwrap();
    assert!(
        kill.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
    assert!(!base.join("kill-me").exists());

    let again = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "kill-me"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    let err = String::from_utf8_lossy(&again.stderr);
    assert!(
        err.contains("not found"),
        "expected not-found error, got: {err}"
    );
}

#[test]
fn daemon_log_written_under_session_dir() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "logged");
    let log_path = base.join("logged/daemon.log");
    for _ in 0..50 {
        if log_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(log_path.exists(), "expected daemon.log under session dir");
    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(
        contents.contains("ready") || contents.contains("starting"),
        "unexpected log contents: {contents:?}"
    );
    kill_session(base, "logged");
}

#[test]
fn auto_generated_names_are_unique() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let mut names = Vec::new();
    for _ in 0..5 {
        let out = Command::new(reshell_bin())
            .args([
                "--dir",
                base.to_str().unwrap(),
                "new",
                "--detach",
                "--shell",
                "/bin/bash",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            name.starts_with("session-"),
            "unexpected auto name: {name}"
        );
        let parts: Vec<_> = name.split('-').collect();
        assert_eq!(parts.len(), 3, "expected session-SECS-SUFFIX: {name}");
        assert_eq!(parts[2].len(), 4);
        names.push(name);
    }
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "auto names collided: {names:?}");
    for name in &names {
        kill_session(base, name);
    }
}
