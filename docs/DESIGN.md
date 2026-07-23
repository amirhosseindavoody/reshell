# reshell Design

Keep interactive shells alive across SSH disconnects — one PTY per session, minimal key interception, closer to `dtach` / `abduco` than to `tmux`.

## 1. Problem

SSH disconnects kill the remote interactive shell and everything attached to that TTY. Users who want a long-lived shell today reach for multiplexers (`tmux`, `screen`, Zellij) — which bring windows, tabs, status bars, and a prefix key chord that steals shortcuts from nested TUIs.

`reshell` fills the narrower niche: keep one shell (and its children) alive after the SSH client exits, then reattach later. No panes, no screen-buffer UI, and only a single detach byte by default.

## 2. Goals and Non-Goals

### Goals

- Survive SSH hangup: client exit or `SIGHUP` must not kill the session shell.
- Minimal input interception: only a single detach byte (default **Ctrl+\** / ASCII `0x1c`; overridable via `--detach-key` / `RESHELL_DETACH_KEY`).
- Explicit sessions: `new`, `attach`, `list`, `info`, `rename`, `clean`, `kill` — no transparent SSH wrap in v1.
- Shell-agnostic: PTY passthrough so bash, zsh, fish, and full-screen apps work.
- Linux servers only (`linux-64` pixi platform).

### Non-Goals (v1)

- Window splitting, tabs, or status bars.
- VT screen-buffer emulation / multiplexer-style scrollback UI (optional byte-ring replay of detached output is supported; apps still redraw on attach).
- Multi-client shared attach (second attach is rejected).
- macOS / Windows.
- Automatic `reshell ssh …` wrapper.

## 3. Prior Art

### 3.1 dtach / abduco

Closest relatives: one PTY, detach/reattach, almost no UI chrome. reshell follows that model — raw PTY passthrough, exclusive attach, and a single detach key — while adding explicit session management (`list` / `info` / picker), DEC-mode restore, optional detached scrollback, and VS Code/Cursor sticky-scroll fixes.

### 3.2 tmux / screen / Zellij

Multiplexers solve a different problem (many windows inside one connection). They own a screen buffer, steal a prefix chord, and wrap OSC sequences. reshell intentionally does **not** compete there: nested TUIs should see a normal TTY with only the detach byte intercepted.

### 3.3 What reshell adds over classic dtach

| Capability | Notes |
|------------|-------|
| Named sessions + picker | Bare `reshell` / `attach` opens a small TUI when stdin is a TTY |
| In-session switch | Switching frees the original attach lock (no nested clients) |
| DEC + title restore | Reattach restores mouse / alt-screen / window title, then forces redraw |
| Optional scrollback | Bounded ring of detached PTY bytes; not a VT buffer |
| `context` snapshot | Last OSC 633 command + ~100 primary-screen lines without attaching |
| VS Code / Cursor SI | Finish outer command + inject shell integration into the session shell |

## 4. Product Shape

### 4.1 User experience

```bash
# Interactive session picker (create / attach / switch / kill)
reshell
# same as: reshell attach

# Create a session and attach (default shell is /bin/zsh)
reshell new demo
reshell new                    # auto-generated name
reshell new demo --shell /bin/bash
reshell new demo --detach      # create only; print name on stdout

# Keep ~1 MiB of detached output and replay it on attach
reshell --scrollback 1M new demo --detach

# Attach (Ctrl+\ detaches without killing the shell by default)
reshell attach demo
reshell a demo                 # short aliases: n/a/ls/i/c/r/k
reshell --detach-key '^a' attach demo

# Inspect / manage
reshell list
reshell ls --json
reshell info demo
reshell context demo
reshell rename old new
reshell clean
reshell kill demo
reshell kill --all
```

### 4.2 Session picker (TTY)

When `attach` has no name and stdin is a TTY:

1. First row: **Create new session**.
2. Then a table: NAME / STATE / CREATED / LAST ACTIVE / SHELL (detached by recent activity, then attached).
3. Current session marked `*` (bold); other attached sessions dimmed; long names truncate with `…`.
4. Keys: ↑/↓ move, Enter or `s` attach/switch, `k` kill (y/N), `q` / Esc cancel.
5. Create-new (or empty list) prompts for an editable name prefilled with `session-{unix}-{hex}`.

Non-TTY: most recently active session, or auto-create when none exist.

### 4.3 Detach key and env overrides

| Knob | Default | Override |
|------|---------|----------|
| Detach key | Ctrl+\ (`0x1c`) | `--detach-key` / `RESHELL_DETACH_KEY` (`^\`, `^a`, `0x1c`, or one ASCII char) |
| Session base dir | `$XDG_RUNTIME_DIR/reshell` or `/tmp/reshell-$UID` | `--dir` / `RESHELL_DIR` |
| Daemon log | `$base/$name/daemon.log` | `--log` / `RESHELL_LOG` |
| Scrollback | `0` (off) | `--scrollback` / `RESHELL_SCROLLBACK` (`512K`, `1M`; max 16M) |
| Default shell | `/bin/zsh` | `--shell` on `new` |

## 5. Architecture

```text
┌──────────────┐     Unix socket      ┌──────────────────────────┐
│ SSH TTY /    │ ───────────────────► │ reshell session daemon   │
│ attach client│ ◄─────────────────── │  - owns PTY master       │
│ (raw mode)   │   framed messages    │  - poll loop + flock     │
└──────────────┘                      └────────────┬─────────────┘
                                                   │ PTY
                                                   ▼
                                      ┌──────────────────────────┐
                                      │ shell + children         │
                                      │ (PTY slave = controlling │
                                      │  TTY; RESHELL_SESSION)   │
                                      └──────────────────────────┘
```

### 5.1 Process model

Three OS processes matter after `reshell new`:

| Process | Role |
|---------|------|
| CLI parent (`new`) | Forks the daemon, waits on a readiness pipe, then exits (or attaches) |
| Session daemon | Owns the PTY master, Unix listener, and poll loop |
| Shell | Child of the daemon; controlling TTY is the PTY slave |

`reshell attach` is a fourth short-lived process: raw local TTY ↔ socket ↔ daemon.

### 5.2 Rust module layout

Single crate; binary name `reshell`.

```text
reshell/
├── Cargo.toml / Cargo.lock
├── pixi.toml / pixi.lock
├── recipe/recipe.yaml
├── src/
│   ├── main.rs          # clap CLI + attach_or_switch
│   ├── picker.rs        # raw-TTY session picker + name prompt
│   ├── session.rs       # base dir, meta, list/info/rename/clean/kill, switch_to
│   ├── server.rs        # daemonize, openpty, accept, multiplex I/O
│   ├── client.rs        # raw TTY, detach key, SIGWINCH / SIGHUP / SIGUSR1
│   ├── protocol.rs      # length-prefixed framing (see PROTOCOL.md)
│   ├── scrollback.rs    # bounded ring of detached PTY bytes
│   ├── context.rs       # primary-screen lines + OSC 633 for `context`
│   ├── termstate.rs     # DEC private mode + OSC title tracking
│   └── vscode_si.rs     # VS Code/Cursor OSC 633 inject (bash/zsh/fish)
├── tests/
│   ├── common/          # shared framing helpers
│   ├── session_smoke.rs
│   ├── attach_restore.rs
│   ├── attach_race.rs
│   └── switch_frees.rs
├── docs/DESIGN.md
└── README.md
```

| File | Responsibility |
|------|----------------|
| [`src/main.rs`](../src/main.rs) | Clap CLI: `new` / `attach` / `list` / `info` / `context` / `rename` / `clean` / `kill` / `completion` (aliases `n`/`a`/`ls`/`i`/`c`/`r`/`k`); dynamic session-name completion; detach-key + log + scrollback flags; default shell `/bin/zsh` |
| [`src/picker.rs`](../src/picker.rs) | Small raw-TTY session picker + name prompt for bare `reshell` / `attach` with no name |
| [`src/session.rs`](../src/session.rs) | Base dir, name validation, `meta.json`, list/info/rename/clean/kill, attach lock, most-recent / current session, `client.pid` / `switch_to` |
| [`src/server.rs`](../src/server.rs) | Daemonize, openpty, spawn shell, accept clients, multiplex I/O, scrollback replay, context snapshots, peer pid |
| [`src/client.rs`](../src/client.rs) | Raw TTY, configurable detach key, `SIGWINCH` / `SIGHUP` / `SIGUSR1`, protocol I/O, context fetch |
| [`src/protocol.rs`](../src/protocol.rs) | Length-prefixed framing (see [PROTOCOL.md](PROTOCOL.md)); context req/res |
| [`src/scrollback.rs`](../src/scrollback.rs) | Bounded ring of detached PTY bytes; size parsing (`1M`, `512K`) |
| [`src/context.rs`](../src/context.rs) | Rolling primary-screen lines + OSC 633 last-command for `reshell context` |
| [`src/termstate.rs`](../src/termstate.rs) | DEC private mode + OSC window-title tracking for restore-on-attach |
| [`src/vscode_si.rs`](../src/vscode_si.rs) | VS Code/Cursor OSC 633 sticky-scroll + shell-integration inject (bash/zsh/fish) |

## 6. Session Storage

Default base directory:

1. `$XDG_RUNTIME_DIR/reshell` if set
2. else `/tmp/reshell-$UID`

Override with `--dir` or `RESHELL_DIR`.

Per session name `$name`:

```text
$base/$name/
  meta.json       # name, daemon pid, shell path, created_unix, last_active_unix, attached
  session.sock    # Unix domain socket (mode 0600)
  attached        # flock-backed lock file held while a client is connected
  client.pid      # pid of the interactive attach client (SO_PEERCRED); cleared on detach
  switch_to       # optional one-shot target name for in-session switch (SIGUSR1)
  daemon.log      # per-session daemon log (startup, attach/detach, errors)
```

Session names are limited to `[A-Za-z0-9._-]`, max 64 characters.
Auto-generated names look like `session-{unix_secs}-{4 hex digits}` so concurrent
`new` calls in the same second do not collide.

`list` skips directories whose daemon pid is dead and removes stale files (also
available explicitly as `reshell clean`). It recovers a leftover `attached` file
when nobody holds the advisory flock (e.g. after a crashed daemon), and removes
orphan session dirs that lack `meta.json`.

`list` shows relative created and last-active times by default (`2h ago`);
`list --json` is stable for scripts (includes `created_unix` / `last_active_unix`).
`info` prints pid, shell, state, timestamps, and all session paths (`info --json` too).
`context` prints the last known command (OSC 633 when present) and ~100 lines of
primary-screen output via a short-lived `ContextReq` (no attach lock, not replayed
into the PTY). With no name, `info` / `context` prefer the session this process is
inside (daemon pid among process ancestors, else `$RESHELL_SESSION`), then the most
recently active session.

`rename old new` renames a live session directory and updates `meta.name`. The
daemon keeps a directory fd open so meta/lock/log writes survive the move; the
Unix socket path moves with the directory.

`kill` sends `SIGTERM` (then `SIGKILL`) to the daemon pid and deletes the session dir.
`kill --all` terminates every live session under the session base dir.
Attach/kill failures include concrete reasons (dead pid, lock held, socket missing, …).

## 7. Session Lifecycle

### 7.1 Creation (`new`)

1. Validate name; refuse if a live session with that name already exists.
2. Resolve shell: `--shell <path>` if given, otherwise **`/bin/zsh`** (not `$SHELL`).
3. Create the session directory.
4. `pipe` + `fork`:
   - **Parent:** close write end; block until child writes one readiness byte (or timeout / EOF).
   - **Child:** `setsid`, ignore `SIGHUP`/`SIGINT`/`SIGPIPE`, reopen stdio to `/dev/null`, run the daemon.
5. Daemon `openpty`, forks the shell on the slave (`TIOCSCTTY`, dup2 0/1/2,
   sets `RESHELL_SESSION=<name>`, `exec` shell).
6. Daemon binds `session.sock`, writes `meta.json` (pid = daemon), signals readiness.
7. Parent prints the session name to stderr and **attaches** (default). Pass `--detach` / `-d`
   to create only and print the name on stdout (for scripts / CI).

The daemon ignores `SIGHUP` so an SSH disconnect of the creating terminal does not
tear it down. The shell keeps default signal disposition so Ctrl+C reaches it via
the PTY when a client is attached (raw mode sends `0x03` as data).

### 7.2 Attach

1. Require a local TTY on stdin (for named attach and for the interactive picker).
2. Resolve the session name (explicit argument, picker, most-recent, or create — see §4.2).
   Bare `reshell` (no subcommand) is an alias for `reshell attach`. When attaching
   to an existing session, prints `attaching to <name>` on stderr before connecting
   (or `switching from <old> to <new>` for an in-session switch).
3. Refuse if meta missing, daemon dead, or an attach flock is already held
   (a leftover `attached` file without a live flock is treated as stale and cleared).
4. Connect to `session.sock`.
5. Put local TTY in raw mode; restore on exit (`TermiosGuard`).
6. Send `Attach` with current winsize; enter poll loop:
   - stdin → `Data` (or `Detach` if the configured detach byte is seen)
   - socket `Data` → stdout
   - `SIGWINCH` → `Resize`
   - `SIGHUP` → send `Detach` and exit (session keeps running)
   - `SIGUSR1` → read `switch_to`, send `Detach`, attach to the new session
     (same process / TTY; previous session is freed)
7. On client exit, write a best-effort DEC mode cleanup sequence (disable mouse /
   alt-screen / bracketed paste) before restoring termios, so the local shell is
   not left with sticky TUI modes.

`last_active_unix` is updated whenever a client attaches or detaches.

### 7.3 In-session switch

When the picker (or `attach <name>`) runs **inside** a session, switch asks the
outer attach client (`SIGUSR1` + `switch_to`) to detach the current session
(freeing its attach lock) and attach to the target instead of nesting a second
client. The daemon records the attach client's pid in `client.pid` via
`SO_PEERCRED` while connected.

### 7.4 Detach vs kill

| Event | Client | Daemon | Shell |
|-------|--------|--------|-------|
| Detach key (default Ctrl+\) | exits | drops client, clears attach lock | keeps running |
| SSH hangup (`SIGHUP` to client) | exits after `Detach` | same as above | keeps running |
| Client crash / socket close | gone | drops client | keeps running |
| Shell exits | eventually EOF on socket | cleans up session files, exits | — |
| `reshell kill` | n/a | terminated | terminated with PTY teardown |

Only one client may be attached. Exclusivity is enforced by an advisory `flock`
on the `attached` file held by the daemon for the life of the connection: a second
socket is accepted then immediately closed, and `reshell attach` refuses early
when the flock is held. A leftover `attached` file with no flock holder is treated
as stale and cleared.

## 8. Reattach Semantics

### 8.1 DEC mode restore and full-screen redraw

reshell does not keep a VT screen buffer. The daemon always:

1. Parses PTY output for DEC private modes (alt-screen, mouse tracking, bracketed
   paste, focus events, cursor visibility, …) and the last OSC 0/2 window title,
   including while detached.
2. On `Attach`, sends those modes (and the remembered title) back to the new
   client as the first `Data` payload, then clears the local screen.
3. If scrollback is enabled and non-empty, replays captured detached bytes as
   further `Data` frames (then clears the ring).
4. Forces a full child redraw that differential TUIs (notably ratatui/crossterm
   apps such as [fresh](https://github.com/sinelaw/fresh)) will actually emit:
   - Apply a temporary winsize (rows±1) so the app invalidates its previous cell
     buffer and dumps a full frame to the newly attached client.
   - After ~50ms of PTY output (or 250ms max), restore the real winsize for a
     second full paint at the correct geometry.

Instant same-size `SIGWINCH` is not enough: fresh redraws in memory, but
crossterm only writes cells that differ from its previous buffer, so a blank
reattach TTY stays blank until the user moves the mouse over dirty regions.

PTY bytes are not forwarded to a client until `Attach` has been processed, so
mode restore (and optional scrollback replay) runs before live redraw data.

### 8.2 Optional scrollback

`--scrollback` / `RESHELL_SCROLLBACK` (set at session create; default `0` = off)
keeps a bounded in-memory ring of raw PTY bytes while detached and replays them
on the next attach — useful for plain-shell history, not a substitute for TUI
redraw. Max 16M; suffixes `K` / `M`.

### 8.3 VS Code / Cursor sticky scroll

VS Code sticky scroll follows **OSC 633** shell-integration command markers, not
the OS process tree. Running `reshell` leaves the outer shell’s “current command”
open, so the sticky line stays on `reshell`.

reshell fixes that when `TERM_PROGRAM` is `vscode` (or Cursor is detected):

1. **On attach**, the client writes `OSC 633;D` to the local TTY to finish the
   outer `reshell` command so sticky scroll can move on.
2. **On session create**, the daemon injects VS Code’s shell-integration script
   into bash (`--init-file`), zsh (`ZDOTDIR` + `VSCODE_INJECTION=1`), or fish
   (`--init-command 'source …'`) when it can locate the script
   (`code`/`cursor --locate-shell-integration-path`, or a `.vscode-server` /
   `.cursor-server` install). The session shell then emits `A/B/E/C/D` for each
   command; those bytes pass through the PTY pipe unchanged.
3. Sessions created outside VS Code still work if the user’s rc manually sources
   shell integration when `TERM_PROGRAM=vscode`.

This is the same model as dtach/abduco (raw passthrough), not tmux (which must
DCS-wrap OSC sequences).

## 9. Daemon I/O Loop

The daemon `poll`s:

- PTY master `POLLIN` (readable → enqueue framed `Data` for the attached client, if any)
- PTY master `POLLOUT` when client→PTY bytes are pending (partial writes resume here;
  no busy-wait on `EAGAIN`)
- Listen socket (accept; at most one live client, with attach flock)
- Client socket `POLLIN` / `POLLOUT` (decode frames into an inbound buffer; flush an outbound buffer)

Client sockets and the PTY master are **non-blocking**. Complete frames are encoded
into an outbound byte buffer and written with partial-write retry on `POLLOUT`.
This matters for TUI apps (ratatui/crossterm): a full-screen redraw can exceed the
Unix socket buffer; naive `write_all` on a non-blocking socket used to fail
mid-frame, corrupt the stream, and freeze the attach client.

When the outbound (client) buffer exceeds a high-water mark, the daemon stops
reading the PTY until it drains (backpressure). When the PTY write buffer is
backed up, the daemon pauses reading the client socket. When no client is
attached (or the client has not yet sent `Attach`), PTY output is still read:
DEC modes are updated, and bytes are pushed into the optional scrollback ring
when `--scrollback` / `RESHELL_SCROLLBACK` is non-zero (otherwise discarded).
When the shell exits (`waitpid`), the daemon cleans up and exits.

Wire format details live in [PROTOCOL.md](PROTOCOL.md).

## 10. CLI Surface

| Command | Aliases | Purpose |
|---------|---------|---------|
| `reshell` / `reshell attach [name]` | `a` | Attach; no name → picker (TTY) or most-recent / create |
| `reshell new [name]` | `n` | Create session; attach unless `--detach` |
| `reshell list` | `ls` | List live sessions (relative times; `--json`) |
| `reshell info [name]` | `i` | Show pid, shell, state, paths (`--json`) |
| `reshell context [name]` | `c` | Snapshot last command + primary-screen lines |
| `reshell rename <old> <new>` | `r` | Rename a live session directory |
| `reshell clean` | | Remove dead / orphan session dirs and stale locks |
| `reshell kill [name]` | `k` | Terminate daemon (+ `--all`) |
| `reshell completion <shell>` | | Shell completions |

Shared flags: `--dir`, `--detach-key`, `--log`, `--scrollback`, `--shell` (on `new`).

## 11. Packaging and Toolchain

Mirrors the csv-utils dual-manifest pattern:

| File | Role |
|------|------|
| `Cargo.toml` / `Cargo.lock` | Rust crate; lockfile used with `--locked` in conda builds |
| `pixi.toml` / `pixi.lock` | Conda env: Rust from conda-forge; tasks; pixi-build |
| `recipe/recipe.yaml` | rattler-build → `$PREFIX/bin/reshell` |
| `scripts/update-version.sh` | CalVer `YYYY.M.D+N` across Cargo / pixi / recipe |

Dev commands go through pixi (`pixi run build`, `pixi run -- cargo …`) so the
conda Rust toolchain is used, not an older system rustup.

## 12. Testing Strategy

- **Unit**: protocol roundtrip, session name validation, meta read/write,
  DEC mode parse/restore (`termstate`), attach flock exclusivity / stale recovery,
  kill SIGTERM→SIGKILL escalation.
- **Integration** (`tests/session_smoke.rs`): `new` → speak protocol over the socket →
  detach → reconnect → confirm the same shell is still alive → `kill`.
- **Integration** (`tests/attach_restore.rs`): child enables mouse/alt-screen → detach →
  reattach observes restored CSI modes; SIGWINCH reporter confirms temporary then
  final winsize (two-phase full paint for differential TUIs).
- **Integration** (`tests/attach_race.rs`): concurrent attach (one survivor), stale
  `attached` recovery, kill / `kill --all`, missing-session errors, auto-name
  uniqueness, daemon log.
- **Integration** (`tests/switch_frees.rs`): in-session switch detaches the original
  session so its attach lock is freed.
- Shared framing helpers live in `tests/common/` so integration tests stay DRY.

Attach’s TTY path is exercised manually or via an external PTY driver; the smoke
test intentionally talks the wire protocol so CI does not need a controlling TTY.

CI (`.github/workflows/ci.yml`) runs `cargo test --locked` and `pixi run test` on
Linux.

## 13. Open Questions

1. **`reshell ssh …` wrapper** — Thin SSH + remote `reshell` helper remains an
   explicit post-v1 idea (see [IMPROVEMENTS.md](IMPROVEMENTS.md) §4).
2. **Append-only session log file** — Optional on-disk log beside the in-memory
   scrollback ring; still not a VT buffer.
3. **Full client TTY path in CI** — Needs a reliable external PTY driver; wire
   protocol coverage stays the CI default to avoid flakes.

### Resolved

- **Attach exclusivity** — Advisory `flock` on `attached` for the life of the
  connection; stale files without a holder are cleared.
- **In-session switch** — Outer attach client handles `SIGUSR1` + `switch_to`
  and reattaches on the same TTY (no nested clients).
- **Scrollback** — Opt-in byte ring at session create; replay after DEC restore.
- **Interactive picker** — Bare `reshell` / `attach` on a TTY; non-TTY keeps
  most-recent / auto-create fallbacks.

## 14. Success Criteria

- After SSH hangup, `reshell attach <name>` reconnects to the same shell and
  children without restarting them.
- Nested TUIs (ratatui/crossterm, etc.) work with only the configured detach key
  intercepted; reattach restores mouse/alt-screen/title and forces a full paint.
- Bare `reshell` on a TTY can create, attach, switch (freeing the previous
  session), and kill sessions from the picker.
- Second attach is rejected while the first holds the flock; stale `attached`
  files recover cleanly.
- `pixi run test` / CI `cargo test --locked` pass on Linux without a controlling TTY.
