# reshell Protocol

Length-prefixed framing over a Unix domain stream socket between the attach
client and the session daemon.

Transport: `$base/$session/session.sock` (see [DESIGN.md](DESIGN.md) §6).
Implementation: [`src/protocol.rs`](../src/protocol.rs).

## 1. Framing

Every message:

| Field | Size | Notes |
|-------|------|-------|
| `type` | 1 byte | Message kind |
| `length` | 4 bytes | Little-endian `u32` payload length |
| `payload` | `length` bytes | Kind-specific |

## 2. Message Types

| `type` | Name | Payload | Direction | Meaning |
|--------|------|---------|-----------|---------|
| `1` | `Data` | raw bytes | both | Terminal I/O (stdin → PTY, or PTY → stdout) |
| `2` | `Resize` | 4 bytes | client → server | Window size change |
| `3` | `Detach` | empty | client → server | Client leaving; keep shell |
| `4` | `Attach` | 4 bytes | client → server | First message after connect |

### 2.1 Winsize payload (`Attach` / `Resize`)

Little-endian:

| Offset | Field |
|--------|-------|
| 0–1 | `rows` (`u16`) |
| 2–3 | `cols` (`u16`) |

### 2.2 `Attach` vs `Resize`

| Message | Server action |
|---------|---------------|
| `Attach` | Replay tracked DEC private modes and the last OSC 0/2 window title to the client as `Data`, clear the local screen, apply a temporary winsize so differential TUIs full-paint, then restore the real winsize shortly after. PTY forwarding starts only after this message. |
| `Resize` | Apply size with `TIOCSWINSZ` only (normal live resize). |

## 3. Client Conventions

- After connect, send `Attach` with the current local winsize before any `Data`.
- **Detach key:** by default byte `0x1c` (Ctrl+\) in the local stdin stream.
  Overridable with `--detach-key` / `RESHELL_DETACH_KEY` (`^\`, `^a`, `0x1c`, or a
  single ASCII char). The client must not forward that byte to the session; it
  sends `Detach` and exits instead.
- On local `SIGHUP`, send `Detach` (best effort) and exit.
- On local `SIGWINCH`, send `Resize`.
- On local `SIGUSR1` (in-session switch), read `switch_to`, send `Detach`, and
  attach to the target session on the same TTY.
- On exit, write a DEC mode cleanup sequence to the local TTY (disable mouse /
  alt-screen / bracketed paste) before restoring termios.

## 4. Server Conventions

- At most one attached client. Extra accepts are closed immediately; the daemon
  holds an advisory `flock` on `attached` while connected.
- Record the peer pid (`SO_PEERCRED`) in `client.pid` while attached; clear on
  detach.
- After `Detach` or client EOF/error, clear the attach lock and keep the shell.
- Track DEC private modes and the last OSC 0/2 window title from all PTY output
  (even while detached).
- Append primary-screen text to on-disk history files (see [DESIGN.md](DESIGN.md)
  §8.2); pause while the alternate screen is active.
- Unrecognized types are a protocol error for the reader; clients ignore unexpected
  control messages from the server (server emits `Data`).

## 5. Example Sequence

```text
client                         daemon                         shell
  |-- Attach(24,80) ---------->|                                |
  |<- Data(DEC modes + clear) -|                                |
  |                            |-- TIOCSWINSZ(23,80)+SIGWINCH ->|
  |<- Data(full paint @23) ----|<-- ratatui invalidates buffer -|
  |                            |-- (≈50ms later) --------------->|
  |                            |-- TIOCSWINSZ(24,80)+SIGWINCH ->|
  |<- Data(full paint @24) ----|<-- second full paint ----------|
  |-- Data("echo hi\n") ------>|-- write PTY ------------------>|
  |<- Data(prompt + "hi\n") ---|<-- read PTY -------------------|
  |                            |  (also append "hi" to history)  |
  |-- Detach ----------------->|                                |
  |  (client exits; local DEC cleanup)
  |                            |  (shell still running; history  |
  |                            |   still captures primary screen)|
  |-- connect + Attach ------->|  restore + winch redraw        |
  |-- Data(...) -------------->| ...                            |
```
