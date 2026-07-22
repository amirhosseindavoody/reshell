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
| `5` | `ContextReq` | empty | client → server | Request a context snapshot (no attach lock) |
| `6` | `ContextRes` | UTF-8 JSON | server → client | `ContextSnapshot` then socket close |

### Winsize payload (`Attach` / `Resize`)

Little-endian:

| Offset | Field |
|--------|-------|
| 0–1 | `rows` (`u16`) |
| 2–3 | `cols` (`u16`) |

### `Attach` vs `Resize`

| Message | Server action |
|---------|---------------|
| `Attach` | Replay tracked DEC private modes and the last OSC 0/2 window title to the client as `Data`, clear the local screen, optionally replay detached scrollback as further `Data`, apply a temporary winsize so differential TUIs full-paint, then restore the real winsize shortly after. PTY forwarding starts only after this message. |
| `Resize` | Apply size with `TIOCSWINSZ` only (normal live resize). |

## Client conventions

- After connect, send `Attach` with the current local winsize before any `Data`.
- **Detach key:** by default byte `0x1c` (Ctrl+\) in the local stdin stream.
  Overridable with `--detach-key` / `RESHELL_DETACH_KEY` (`^\`, `^a`, `0x1c`, or a
  single ASCII char). The client must not forward that byte to the session; it
  sends `Detach` and exits instead.
- On local `SIGHUP`, send `Detach` (best effort) and exit.
- On local `SIGWINCH`, send `Resize`.
- On exit, write a DEC mode cleanup sequence to the local TTY (disable mouse /
  alt-screen / bracketed paste) before restoring termios.

## Server conventions

- At most one attached client. Extra accepts are closed immediately; the daemon
  holds an advisory `flock` on `attached` while connected.
- `ContextReq` is classified from the first framed message on accept and answered
  with `ContextRes` **without** taking the attach lock (works while attached).
- After `Detach` or client EOF/error, clear the attach lock and keep the shell.
- Track DEC private modes and the last OSC 0/2 window title from all PTY output
  (even while detached).
- Keep a rolling primary-screen line history (~100 lines) plus last OSC 633
  command markers for `reshell context` (independent of attach-replay scrollback).
- When `--scrollback` / `RESHELL_SCROLLBACK` is set at session create, keep a
  bounded ring of PTY bytes while no ready client is attached and replay them
  after mode restore + clear on the next `Attach`.
- Unrecognized types are a protocol error for the reader; clients ignore unexpected
  control messages from the server (server emits `Data` and `ContextRes`).

### `ContextRes` JSON

```json
{
  "name": "demo",
  "last_command": "cargo test",
  "last_exit_code": 0,
  "lines": ["…", "…"],
  "alt_screen": false
}
```

`last_command` / `last_exit_code` are null when no OSC 633 `E`/`D` markers have
been seen. `alt_screen` is true when a full-screen app currently owns the PTY;
`lines` then reflect history captured before alt-screen entry.

## Example sequence

```text
client                         daemon                         shell
  |-- Attach(24,80) ---------->|                                |
  |<- Data(DEC modes + clear) -|                                |
  |<- Data(scrollback…) -------|  (if enabled and non-empty)    |
  |                            |-- TIOCSWINSZ(23,80)+SIGWINCH ->|
  |<- Data(full paint @23) ----|<-- ratatui invalidates buffer -|
  |                            |-- (≈50ms later) --------------->|
  |                            |-- TIOCSWINSZ(24,80)+SIGWINCH ->|
  |<- Data(full paint @24) ----|<-- second full paint ----------|
  |-- Data("echo hi\n") ------>|-- write PTY ------------------>|
  |<- Data(prompt + "hi\n") ---|<-- read PTY -------------------|
  |-- Detach ----------------->|                                |
  |  (client exits; local DEC cleanup)
  |                            |  (shell still running; optional|
  |                            |   scrollback captures PTY out) |
  |-- connect + Attach ------->|  restore + scrollback + winch  |
  |-- Data(...) -------------->| ...                            |
```
