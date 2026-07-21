//! Shared helpers for integration tests (protocol framing + session setup).
#![allow(dead_code)]
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub fn reshell_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_reshell"))
}

pub fn write_msg(w: &mut impl Write, kind: u8, payload: &[u8]) {
    w.write_all(&[kind]).unwrap();
    w.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    w.write_all(payload).unwrap();
    w.flush().unwrap();
}

pub fn read_msg(r: &mut impl Read) -> Option<(u8, Vec<u8>)> {
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

pub fn attach_winsize(stream: &mut UnixStream, rows: u16, cols: u16) {
    let mut ws = [0u8; 4];
    ws[0..2].copy_from_slice(&rows.to_le_bytes());
    ws[2..4].copy_from_slice(&cols.to_le_bytes());
    write_msg(stream, 4, &ws); // MSG_ATTACH
}

pub fn wait_sock(base: &Path, name: &str) -> PathBuf {
    let sock = base.join(name).join("session.sock");
    for _ in 0..50 {
        if sock.exists() {
            return sock;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket missing for {name}");
}

pub fn new_detached(base: &Path, name: &str) {
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
    wait_sock(base, name);
}

pub fn collect_data(stream: &mut UnixStream, deadline: Instant) -> Vec<u8> {
    let mut out = Vec::new();
    while Instant::now() < deadline {
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        match read_msg(stream) {
            Some((1, data)) => out.extend_from_slice(&data),
            Some(_) => {}
            None => {}
        }
    }
    out
}

pub fn kill_session(base: &Path, name: &str) {
    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", name])
        .output();
}
