//! On-disk session history files capture primary-screen output.
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn history_captures_shell_output_and_info_lists_files() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "hist");

    let sock = wait_sock(base, "hist");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut stream, 24, 80);
    write_msg(&mut stream, 1, b"printf 'HISTORY_LINE_OK\\n'\n");
    let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(2));

    // Give the daemon a moment to flush the history file.
    let hist = base.join("hist/history/0001.txt");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while Instant::now() < deadline {
        if hist.exists() {
            text = fs::read_to_string(&hist).unwrap_or_default();
            if text.contains("HISTORY_LINE_OK") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(
        text.contains("beginning of session history"),
        "missing begin marker: {text}"
    );
    assert!(
        text.contains("HISTORY_LINE_OK"),
        "expected captured line: {text}"
    );

    let info = Command::new(reshell_bin())
        .args(["--dir", base.to_str().unwrap(), "info", "hist"])
        .output()
        .unwrap();
    assert!(info.status.success(), "{}", String::from_utf8_lossy(&info.stderr));
    let out = String::from_utf8_lossy(&info.stdout);
    assert!(out.contains("history_dir:"), "{out}");
    assert!(out.contains("history/0001.txt"), "{out}");
    assert!(out.contains("(current)"), "{out}");

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "hist");
}

#[test]
fn history_skips_alt_screen_output() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_detached(base, "tuihist");

    let sock = wait_sock(base, "tuihist");
    let mut stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    attach_winsize(&mut stream, 24, 80);
    // Enter/leave alt-screen and the TUI junk must land in separate PTY reads
    // so the daemon's alt-screen state is observed between them.
    write_msg(
        &mut stream,
        1,
        b"printf '\\033[?1049h'; sleep 0.2; printf 'TUI_JUNK\\n'; sleep 0.2; printf '\\033[?1049l'; sleep 0.2; printf 'KEEP_LINE\\n'\n",
    );
    let _ = collect_data(&mut stream, Instant::now() + Duration::from_secs(3));

    let hist = base.join("tuihist/history/0001.txt");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while Instant::now() < deadline {
        if hist.exists() {
            text = fs::read_to_string(&hist).unwrap_or_default();
            if text.contains("KEEP_LINE") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(text.contains("KEEP_LINE"), "expected primary-screen line: {text}");
    assert!(
        !text.lines().any(|l| l.trim() == "TUI_JUNK"),
        "alt-screen output should not be captured as its own line: {text}"
    );

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "tuihist");
}
