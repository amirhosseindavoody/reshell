//! Reattach must restore DEC modes and drive a two-phase winsize so
//! differential TUIs emit a full cell dump (not a no-op same-size redraw).
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn reattach_restores_mouse_and_alt_screen() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "restore");
    let sock = wait_sock(base, "restore");

    // First attach: enable alt-screen + mouse like a TUI editor would.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        attach_winsize(&mut stream, 24, 80);
        std::thread::sleep(Duration::from_millis(50));
        let enable = "printf '\\033[?1049h\\033[?1000h\\033[?1002h\\033[?1006h'\n";
        write_msg(&mut stream, 1, enable.as_bytes());
        let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));
        write_msg(&mut stream, 3, &[]); // Detach
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
    kill_session(base, "restore");
}

#[test]
fn reattach_two_phase_winsize_for_full_paint() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "winch");
    let sock = wait_sock(base, "winch");

    // Start a SIGWINCH reporter that prints each size it observes.
    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        attach_winsize(&mut stream, 24, 80);
        std::thread::sleep(Duration::from_millis(80));
        let script = concat!(
            "python3 - <<'PY'\n",
            "import os, signal, sys, time\n",
            "def winch(sig, frame):\n",
            "    s = os.get_terminal_size()\n",
            "    sys.stdout.write(f'WINCH {s.lines} {s.columns}\\n')\n",
            "    sys.stdout.flush()\n",
            "signal.signal(signal.SIGWINCH, winch)\n",
            "sys.stdout.write('READY\\n')\n",
            "sys.stdout.flush()\n",
            "t0 = time.time()\n",
            "while time.time() - t0 < 30:\n",
            "    time.sleep(0.05)\n",
            "PY\n",
        );
        write_msg(&mut stream, 1, script.as_bytes());
        let data = collect_data(&mut stream, Instant::now() + Duration::from_secs(3));
        assert!(
            String::from_utf8_lossy(&data).contains("READY"),
            "reporter did not start: {:?}",
            String::from_utf8_lossy(&data)
        );
        write_msg(&mut stream, 3, &[]);
        std::thread::sleep(Duration::from_millis(150));
    }

    // Reattach at the same 24x80 — daemon must briefly use 23x80 then restore
    // 24x80 so differential TUIs invalidate their cell buffer.
    let mut stream = UnixStream::connect(&sock).expect("reconnect");
    attach_winsize(&mut stream, 24, 80);
    let data = collect_data(&mut stream, Instant::now() + Duration::from_secs(4));
    let text = String::from_utf8_lossy(&data);

    assert!(
        text.contains("WINCH 23 80"),
        "expected temporary redraw size 23x80 in {text:?}"
    );
    assert!(
        text.contains("WINCH 24 80"),
        "expected restored size 24x80 in {text:?}"
    );

    // Temporary size must be observed before the final size.
    let temp_pos = text.find("WINCH 23 80").unwrap();
    let final_pos = text.find("WINCH 24 80").unwrap();
    assert!(
        temp_pos < final_pos,
        "temporary size should precede final size in {text:?}"
    );

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "winch");
}
