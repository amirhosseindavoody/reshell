//! Regression: large PTY output (TUI redraws) must not freeze or drop the attach client.
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

mod common;
use common::*;

#[test]
fn large_pty_output_reaches_client() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();

    new_detached(base, "flood");
    let sock = wait_sock(base, "flood");

    let mut stream = UnixStream::connect(&sock).expect("connect");
    attach_winsize(&mut stream, 24, 80);

    // ~200KB of output — enough to fill small socket buffers if writes are naive.
    let cmd = "python3 -c 'import sys; sys.stdout.write((\"X\"*100+\"\\n\")*2000); sys.stdout.flush()'\n";
    write_msg(&mut stream, 1, cmd.as_bytes());

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let mut total = 0usize;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && total < 180_000 {
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
    kill_session(base, "flood");
}
