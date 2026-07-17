//! Regression: large PTY output (TUI redraws) must not freeze or drop the attach client.
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

#[test]
fn large_pty_output_reaches_client() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    let out = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            "flood",
            "--shell",
            "/bin/bash",
        ])
        .output()
        .expect("run reshell new");
    assert!(
        out.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let sock = base.join("flood/session.sock");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock.exists());

    let mut stream = UnixStream::connect(&sock).expect("connect");
    let mut ws = [0u8; 4];
    ws[0..2].copy_from_slice(&24u16.to_le_bytes());
    ws[2..4].copy_from_slice(&80u16.to_le_bytes());
    write_msg(&mut stream, 4, &ws);

    // ~200KB of output — enough to fill small socket buffers if writes are naive.
    let cmd = "python3 -c 'import sys; sys.stdout.write((\"X\"*100+\"\\n\")*2000); sys.stdout.flush()'\n";
    write_msg(&mut stream, 1, cmd.as_bytes());

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut total = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline && total < 180_000 {
        match read_msg(&mut stream) {
            Some((1, data)) => total += data.len(),
            Some(_) => {}
            None => break,
        }
    }

    assert!(
        total >= 180_000,
        "expected ~200KB of PTY output, got {total} bytes (TUI redraws would break)"
    );

    write_msg(&mut stream, 3, &[]);
    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "flood"])
        .output();
}
