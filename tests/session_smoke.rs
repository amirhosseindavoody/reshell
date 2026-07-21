use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn create_detach_reconnect_keeps_shell() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "smoke");
    let sock = wait_sock(base, "smoke");

    // First attach via protocol: run a marker command.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        attach_winsize(&mut stream, 24, 80);
        write_msg(
            &mut stream,
            1,
            b"export PS1=; echo RESHELL_MARKER_42\n",
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut collected = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while Instant::now() < deadline {
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
        attach_winsize(&mut stream, 24, 80);
        write_msg(&mut stream, 1, b"echo RESHELL_STILL_ALIVE\n");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut collected = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        while Instant::now() < deadline {
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
