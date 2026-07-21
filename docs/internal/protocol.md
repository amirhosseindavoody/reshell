# Client–daemon protocol

Transport: Unix domain stream socket at
`$base/$session/session.sock` (see [design.md](design.md)).

Implementation: [`src/protocol.rs`](../../src/protocol.rs).

## Framing

Every message:

| Field | Size | Notes |
|-------|------|-------|
| `type` | 1 byte | Message kind |
| `length` | 4 bytes | Little-endian `u32` payload length |
| `payload` | `length` bytes | Kind-specific |

## Message types

| `type` | Name | Payload | Direction | Meaning |
|--------|------|---------|-----------|---------|
| `1` | `Data` | raw bytes | both | Terminal I/O (stdin → PTY, or PTY → stdout) |
| `2` | `Resize` | 4 bytes | client → server | Window size change |
| `3` | `Detach` | empty | client → server | Client leaving; keep shell |
| `4` | `Attach` | 4 bytes | client → server | First message after connect |

### Winsize payload (`Attach` / `Resize`)

Little-endian:

| Offset | Field |
|--------|-------|
| 0–1 | `rows` (`u16`) |
| 2–3 | `cols` (`u16`) |

### `Attach` vs `Resize`

| Message | Server action |
|---------|---------------|
| `Attach` | Replay tracked DEC private modes to the client as `Data`, clear the local screen, apply a temporary winsize so differential TUIs full-paint, then restore the real winsize shortly after. PTY forwarding starts only after this message. |
| `Resize` | Apply size with `TIOCSWINSZ` only (normal live resize). |

## Client conventions

- After connect, send `Attach` with the current local winsize before any `Data`.
- **Detach key:** byte `0x1c` (Ctrl+\) in the local stdin stream. The client must
  not forward that byte to the session; it sends `Detach` and exits instead.
- On local `SIGHUP`, send `Detach` (best effort) and exit.
- On local `SIGWINCH`, send `Resize`.
- On exit, write a DEC mode cleanup sequence to the local TTY (disable mouse /
  alt-screen / bracketed paste) before restoring termios.

## Server conventions

- At most one attached client. Extra accepts are closed immediately; the daemon
  holds an advisory `flock` on `attached` while connected.
- After `Detach` or client EOF/error, clear the attach lock and keep the shell.
- Track DEC private modes from all PTY output (even while detached / discarded).
- Unrecognized types are a protocol error for the reader; clients ignore unexpected
  control messages from the server (server currently only emits `Data`).

## Example sequence

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
  |-- Detach ----------------->|                                |
  |  (client exits; local DEC cleanup)
  |                            |  (shell still running)         |
  |-- connect + Attach ------->|  restore modes + two-phase winch
  |-- Data(...) -------------->| ...                            |
```
