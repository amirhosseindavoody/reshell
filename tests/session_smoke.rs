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
fn create_detach_reconnect_keeps_shell() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    let out = Command::new(reshell_bin())
        .args([
            "--dir",
            base.to_str().unwrap(),
            "new",
            "smoke",
            "--detach",
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

    let sock = base.join("smoke/session.sock");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sock.exists(), "socket not created");

    // First attach via protocol: run a marker command.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        let mut ws = [0u8; 4];
        ws[0..2].copy_from_slice(&24u16.to_le_bytes());
        ws[2..4].copy_from_slice(&80u16.to_le_bytes());
        write_msg(&mut stream, 4, &ws); // MSG_ATTACH
        // Disable job control noise; print unique marker.
        write_msg(
            &mut stream,
            1,
            b"export PS1=; echo RESHELL_MARKER_42\n",
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut collected = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while std::time::Instant::now() < deadline {
            match read_msg(&mut stream) {
                Some((1, data)) => {
                    collected.extend_from_slice(&data);
                    if collected.windows(18).any(|w| w == b"RESHELL_MARKER_42") {
                        break;
                    }
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("RESHELL_MARKER_42"),
            "expected marker in output, got: {text:?}"
        );
        write_msg(&mut stream, 3, &[]); // MSG_DETACH
    }

    // Session still listed as detached.
    let list = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "list"])
        .output()
        .unwrap();
    let list_txt = String::from_utf8_lossy(&list.stdout);
    assert!(list_txt.contains("smoke"), "list missing session: {list_txt}");
    assert!(list_txt.contains("detached"), "expected detached: {list_txt}");

    // Reconnect and confirm shell is still alive (echo again).
    {
        let mut stream = UnixStream::connect(&sock).expect("reconnect");
        let mut ws = [0u8; 4];
        ws[0..2].copy_from_slice(&24u16.to_le_bytes());
        ws[2..4].copy_from_slice(&80u16.to_le_bytes());
        write_msg(&mut stream, 4, &ws);
        write_msg(&mut stream, 1, b"echo RESHELL_STILL_ALIVE\n");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut collected = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while std::time::Instant::now() < deadline {
            match read_msg(&mut stream) {
                Some((1, data)) => {
                    collected.extend_from_slice(&data);
                    if collected.windows(20).any(|w| w == b"RESHELL_STILL_ALIVE") {
                        break;
                    }
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("RESHELL_STILL_ALIVE"),
            "shell died after detach; output: {text:?}"
        );
        write_msg(&mut stream, 3, &[]);
    }

    let kill = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "kill", "smoke"])
        .output()
        .unwrap();
    assert!(kill.status.success());
}
