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

Server applies size with `TIOCSWINSZ` on the PTY master (shell typically gets `SIGWINCH`).

## Client conventions

- After connect, send `Attach` with the current local winsize before any `Data`.
- **Detach key:** byte `0x1c` (Ctrl+\) in the local stdin stream. The client must
  not forward that byte to the session; it sends `Detach` and exits instead.
- On local `SIGHUP`, send `Detach` (best effort) and exit.
- On local `SIGWINCH`, send `Resize`.

## Server conventions

- At most one attached client. Extra accepts are closed immediately.
- After `Detach` or client EOF/error, clear the attach lock and keep the shell.
- Unrecognized types are a protocol error for the reader; clients ignore unexpected
  control messages from the server (server currently only emits `Data`).

## Example sequence

```text
client                         daemon                         shell
  |-- Attach(24,80) ---------->|                                |
  |                            |-- TIOCSWINSZ ----------------->|
  |-- Data("echo hi\n") ------>|-- write PTY ------------------>|
  |<- Data(prompt + "hi\n") ---|<-- read PTY -------------------|
  |-- Detach ----------------->|                                |
  |  (client exits)            |  (shell still running)         |
  |-- connect + Attach ------->|                                |
  |-- Data(...) -------------->| ...                            |
```
