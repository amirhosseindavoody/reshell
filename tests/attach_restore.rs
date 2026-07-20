//! Reattach must restore DEC modes (mouse / alt-screen) observed from the child.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

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

fn attach_winsize(stream: &mut UnixStream, rows: u16, cols: u16) {
    let mut ws = [0u8; 4];
    ws[0..2].copy_from_slice(&rows.to_le_bytes());
    ws[2..4].copy_from_slice(&cols.to_le_bytes());
    write_msg(stream, 4, &ws);
}

fn collect_data(stream: &mut UnixStream, deadline: Instant) -> Vec<u8> {
    let mut out = Vec::new();
    while Instant::now() < deadline {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        match read_msg(stream) {
            Some((1, data)) => out.extend_from_slice(&data),
            Some(_) => {}
            None => {
                // timeout or EOF — keep waiting until deadline for more
                if out.windows(8).any(|w| w == b"\x1b[?1049h")
                    && out.windows(8).any(|w| w == b"\x1b[?1000h")
                {
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn reattach_restores_mouse_and_alt_screen() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    let out = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            "restore",
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

    let sock = base.join("restore/session.sock");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock.exists());

    // First attach: enable alt-screen + mouse like a TUI editor would.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        attach_winsize(&mut stream, 24, 80);
        // Give Attach a moment to complete before writing to the PTY.
        std::thread::sleep(Duration::from_millis(50));
        let enable = "printf '\\033[?1049h\\033[?1000h\\033[?1002h\\033[?1006h'\n";
        write_msg(&mut stream, 1, enable.as_bytes());
        // Drain some output so the daemon sees and parses the CSI sequences.
        let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
        write_msg(&mut stream, 3, &[]); // Detach
        // Allow daemon to drop the client.
        std::thread::sleep(Duration::from_millis(100));
    }

    // Reattach: expect restored DEC modes as the first Data from the daemon.
    let mut stream = UnixStream::connect(&sock).expect("reconnect");
    attach_winsize(&mut stream, 24, 80);
    let data = collect_data(&mut stream, Instant::now() + Duration::from_secs(3));

    assert!(
        data.windows(8).any(|w| w == b"\x1b[?1049h"),
        "expected alt-screen restore in {:?}",
        String::from_utf8_lossy(&data)
    );
    assert!(
        data.windows(8).any(|w| w == b"\x1b[?1000h"),
        "expected mouse 1000 restore in {:?}",
        String::from_utf8_lossy(&data)
    );
    assert!(
        data.windows(8).any(|w| w == b"\x1b[?1006h"),
        "expected SGR mouse restore in {:?}",
        String::from_utf8_lossy(&data)
    );

    write_msg(&mut stream, 3, &[]);
    let _ = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "restore"])
        .output();
}
