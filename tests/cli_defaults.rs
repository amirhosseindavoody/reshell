//! `reshell new` defaults to attach; `--detach` creates without attaching.
//! `reshell attach` with no name picks the most recently active session.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn reshell_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_reshell"))
}

fn write_msg(w: &mut impl Write, kind: u8, payload: &[u8]) {
    w.write_all(&[kind]).unwrap();
    w.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    w.write_all(payload).unwrap();
    w.flush().unwrap();
}

fn read_msg(r: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut kind = [0u8; 1];
    if r.read_exact(&mut kind).is_err() {
        return None;
    }
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).ok()?;
    }
    Some((kind[0], payload))
}

fn new_detached(base: &std::path::Path, name: &str) {
    let out = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            name,
            "--detach",
            "--shell",
            "/bin/bash",
        ])
        .output()
        .expect("run reshell new --detach");
    assert!(
        out.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sock = base.join(name).join("session.sock");
    for _ in 0..50 {
        if sock.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket missing for {name}");
}

fn protocol_attach_touch(base: &std::path::Path, name: &str) {
    let sock = base.join(name).join("session.sock");
    let mut stream = UnixStream::connect(&sock).expect("connect");
    let mut ws = [0u8; 4];
    ws[0..2].copy_from_slice(&24u16.to_le_bytes());
    ws[2..4].copy_from_slice(&80u16.to_le_bytes());
    write_msg(&mut stream, 4, &ws);
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
    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "needs-tty"])
        .output();
}

#[test]
fn attach_without_name_picks_most_recent() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "first");
    std::thread::sleep(Duration::from_millis(50));
    new_detached(base, "second");
    // Touch `first` so it becomes the most recently active.
    std::thread::sleep(Duration::from_millis(50));
    protocol_attach_touch(base, "first");

    // `reshell attach` with no name should resolve to `first` and then fail
    // without a TTY — but the error should name that session via the attach
    // path. We check resolution by inspecting which session gets the attach
    // lock attempt: connect ourselves after a failed CLI attach is messy, so
    // instead call the library helper through a tiny CLI probe: list meta.
    // Use the binary: attach with no name fails on tty, but stderr includes
    // nothing about the name. Verify via session meta last_active ordering
    // and that `most_recent` logic is covered by unit tests; here confirm the
    // CLI exit path for "no sessions" vs success path with a named attach.

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
    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "first"])
        .output();
    let only_second = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach"])
        .output()
        .unwrap();
    assert!(!only_second.status.success());
    assert!(String::from_utf8_lossy(&only_second.stderr).contains("tty"));

    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "second"])
        .output();

    let empty = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "attach"])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stderr).contains("no sessions"),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
}
