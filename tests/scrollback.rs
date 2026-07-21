//! Optional detached scrollback is replayed on attach after DEC restore.
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

mod common;
use common::*;

fn new_bash_detached(base: &Path, name: &str, scrollback: Option<&str>) {
    // Wrapper so user bashrc cannot inject noise into the PTY stream.
    let wrap = base.join("bash-norc");
    std::fs::write(
        &wrap,
        "#!/bin/bash\nexec /bin/bash --noprofile --norc \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrap, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut args = vec!["--dir".to_string(), base.to_str().unwrap().to_string()];
    if let Some(sb) = scrollback {
        args.push("--scrollback".into());
        args.push(sb.into());
    }
    args.extend([
        "new".into(),
        name.into(),
        "--detach".into(),
        "--shell".into(),
        wrap.to_str().unwrap().to_string(),
    ]);

    let out = Command::new(reshell_bin())
        .args(&args)
        .output()
        .expect("run reshell new --detach");
    assert!(
        out.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_sock(base, name);
}

fn wait_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {}", path.display());
}

/// Produce a line on the PTY while detached; sync via a side-channel file.
fn print_while_detached(sock: &Path, marker: &str, done_file: &Path) {
    let mut stream = UnixStream::connect(sock).expect("connect");
    attach_winsize(&mut stream, 24, 80);
    std::thread::sleep(Duration::from_millis(80));
    let cmd = format!(
        "(sleep 0.25; printf '{}\\n'; echo ok > '{}') &\n",
        marker,
        done_file.display()
    );
    write_msg(&mut stream, 1, cmd.as_bytes());
    std::thread::sleep(Duration::from_millis(50));
    write_msg(&mut stream, 3, &[]); // Detach before the sleep finishes
    wait_file(done_file, Duration::from_secs(3));
    // Let the daemon drain the PTY after the printf.
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn detach_output_replayed_when_scrollback_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_bash_detached(base, "sb", Some("64K"));
    let sock = wait_sock(base, "sb");
    let done = base.join("done-on");

    print_while_detached(&sock, "DETACHED_SCROLLBACK_OK", &done);

    let mut stream = UnixStream::connect(&sock).expect("reattach");
    attach_winsize(&mut stream, 24, 80);
    // Short window: scrollback is sent immediately on Attach.
    let data = collect_data(&mut stream, Instant::now() + Duration::from_millis(400));
    let text = String::from_utf8_lossy(&data);
    assert!(
        text.contains("DETACHED_SCROLLBACK_OK"),
        "expected detached output replayed on attach: {text:?}"
    );

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "sb");
}

#[test]
fn scrollback_off_discards_detached_output() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    new_bash_detached(base, "nosb", None);
    let sock = wait_sock(base, "nosb");
    let done = base.join("done-off");

    print_while_detached(&sock, "SHOULD_NOT_REPLAY", &done);

    let mut stream = UnixStream::connect(&sock).expect("reattach");
    attach_winsize(&mut stream, 24, 80);
    let data = collect_data(&mut stream, Instant::now() + Duration::from_millis(400));
    let text = String::from_utf8_lossy(&data);
    assert!(
        !text.contains("SHOULD_NOT_REPLAY"),
        "scrollback off should discard detached output: {text:?}"
    );

    write_msg(&mut stream, 3, &[]);
    kill_session(base, "nosb");
}
