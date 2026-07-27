# Re-shell

A lightweight tool to keep shells alive and running after SSH disconnects.

## Features

- Keep shells alive and running after SSH disconnects
- Minimal footprint so CLI tools, TUI apps, and scripts just work — no prefix keys stolen
- Explicit sessions: `new` / `attach` / `list` / `info` / `kill`
- Bare `reshell` opens a small session picker (`n` new / switch / kill; `*` marks the current session)
- Detach with **Ctrl+\** by default (overridable); client exits, session keeps running
- Reattach restores TUI terminal modes (mouse, alt-screen, …), the window/tab title, and forces a redraw
- Primary-screen output is logged to rotating text history files under the session dir (skipped while a full-screen TUI owns the alternate screen)
- VS Code/Cursor sticky scroll: finishes the outer `reshell` command and injects shell integration into the session (bash, zsh, fish)
- Targeted at SSH sessions into Linux servers
- Defaults to **zsh** (`/bin/zsh`); override with `--shell` for bash, fish, etc.

## Quick start

### Prerequisites

- [Pixi](https://pixi.sh/latest/)

### From source

```bash
git clone https://github.com/amirhosseindavoody/reshell.git
cd reshell
pixi install
pixi run build
pixi run reshell -- --help
```

### Install with pixi (another workspace)

Enable git source builds, then add from GitHub:

```toml
# pixi.toml
[workspace]
preview = ["pixi-build"]
```

```bash
pixi add --git https://github.com/amirhosseindavoody/reshell.git --branch main reshell
```

Install globally (adds `reshell` to your PATH):

```bash
pixi global install --git https://github.com/amirhosseindavoody/reshell.git --branch main reshell
```

## Usage

```bash
# Interactive session picker (`n` to create / attach)
reshell
# same as: reshell attach

# Create a session and attach (default shell is /bin/zsh)
reshell new demo
# or: reshell new          # auto-generated name
# or: reshell new demo --shell /bin/bash

# Create without attaching (prints the name)
reshell new demo --detach

# Attach (Ctrl+\ detaches without killing the shell by default)
reshell attach demo
# or: reshell a demo       # short aliases: n/a/d/ls/i/r/k
# or: reshell attach       # interactive picker (non-TTY: most recent / new)
# or: reshell --detach-key '^a' attach demo

# Detach a client from a session (shell keeps running)
reshell detach demo
# or: reshell d            # current session when inside one; else most recent

# List sessions (created + last-active relative times; --json for scripts)
reshell list
reshell ls --json

# Session details (paths, pid, state, history files, …)
reshell info demo
# or: reshell i        # current session when inside one; else most recent

# Rename a live session
reshell rename demo demo2
# or: reshell r demo demo2

# Remove dead/orphan *live* session dirs (archives history/logs first; does not
# delete archives). Also runs as part of `list`.
reshell clean

# List live + ended sessions (forgotten names, crash forensics)
reshell list --all
reshell info demo                  # live, or newest matching archive
reshell info demo@1730000000       # exact archive id
reshell clean --all                # also purge archived history/logs

# Kill a session
reshell kill demo
# or: reshell k demo
# or: reshell kill --all   # terminate every live session
```

Short subcommand aliases (also listed in `reshell --help`): `n` new, `a` attach,
`d` detach, `ls` list, `i` info, `r` rename, `k` kill.

Bare `reshell` / `reshell attach` (no name) opens a small picker when stdin is a
TTY: a table of sessions (newest activity first among detached, then attached:
name, state, created, last-active, shell, and the current history file path).
The session you are inside is marked with `*` and bolded. Already-attached
sessions (other than the current one) are shown dimmed. Long names and paths
truncate with an ellipsis so columns stay aligned. Keys: ↑/↓ move, Enter or `s`
switch/attach (attached sessions confirm detach-first), `n` create (name prompt),
`k` kill (with y/N confirm), `q` / Esc cancel. Pressing `n` (or bare `reshell`
with no sessions) prompts for a session name pre-filled with a generated
`session-…` default you can edit. Switching from **inside** a session detaches
(frees) that session before attaching to the target — it does not nest a second
client. A session still admits only one attached terminal at a time. Without a
TTY (scripts) it still falls back to the most recently active session.

### Shell completion

```bash
# bash
eval "$(reshell completion bash)"

# zsh
eval "$(reshell completion zsh)"

# fish
reshell completion fish | source
```

To load on every shell start, add the matching line to `~/.bashrc`, `~/.zshrc`, or `~/.config/fish/config.fish`.

Completions call back into `reshell` at tab time, so `attach` suggests
**detachable** session names only, while `info` / `detach` / `kill` / `rename`
suggest all live sessions (honoring `--dir` / `RESHELL_DIR`). After `reshell`,
Tab lists long subcommand names (and short aliases in the description where the
shell shows them — e.g. `new (n)`). Option flags (`--dir`, …)
are not offered on Tab — use `--help` for those.

Session files live under `$XDG_RUNTIME_DIR/reshell` (fallback `/tmp/reshell-$UID`). Override with `--dir` or `RESHELL_DIR`.

Ended sessions (after `kill`, shell exit, or stale cleanup) keep their **history** and **daemon.log** under an archive directory:

- Default: `$XDG_STATE_HOME/reshell/archive` (fallback `$HOME/.local/state/reshell/archive`) so logs survive reboot and `/tmp` cleanup
- With `--dir`: `$dir/archive` (keeps tests and custom layouts self-contained)
- Override: `--archive-dir` / `RESHELL_ARCHIVE_DIR`

Archive entries are named `name@unix` (e.g. `demo@1730000000`). Use `reshell list --all` to see live and ended sessions together, and `reshell info <name-or-id>` to inspect one. `reshell clean` only sweeps dead *live* dirs (after archiving); `reshell clean --all` also deletes archives.

Inside a session shell, `RESHELL_SESSION` is set to the session name. Bare
`reshell info` uses the current session (even after `rename`);
outside a session it falls back to the most recently active one. If that live
session is gone, `info` falls back to the newest matching ended archive.

Primary-screen shell output is appended to rotating text files under
`$session/history/` (`0001.txt`, `0002.txt`, …; ~2000 lines each). Capture
pauses while a full-screen app owns the **alternate screen** (DEC 1049/1047/47)
— that is TUI mode, not the attach client's local raw TTY mode.
`reshell info` lists the history directory and file paths. History stays a
plain-text line log (not a VT screen buffer): interactive redraws are collapsed
with a current-line cursor model (`\r`, backspace, same-line CSI erase/moves);
other CSI/OSC is stripped.

Daemon logs go to `$session/daemon.log` by default. Override with `--log` / `RESHELL_LOG`.

Detach key defaults to **Ctrl+\**. Override with `--detach-key` / `RESHELL_DETACH_KEY` (`^\`, `^a`, `0x1c`, or a single ASCII char).

## Troubleshooting

### Sessions disappeared after SSH disconnect

An SSH hangup should only detach the client — the session daemon and shell keep
running. If `reshell list` is empty afterward, the session itself exited (shell
quit, daemon crash, OOM killer, reboot, or someone ran `kill` / `clean`).

1. **List what is still live**
   ```bash
   reshell list
   reshell list --json
   ```
2. **List live and ended sessions** (names you forgot, crash leftovers)
   ```bash
   reshell list --all
   reshell list --all --json
   ```
   Default archive root is under `$XDG_STATE_HOME/reshell/archive` (see above).
   With a custom `--dir`, look in `$dir/archive`.
3. **Inspect an ended session** (daemon log + history paths)
   ```bash
   reshell info <name-or-id>
   # examples:
   reshell info demo
   reshell info demo@1730000000
   ```
   Or read files directly:
   ```bash
   ls "$XDG_STATE_HOME/reshell/archive" 2>/dev/null \
     || ls "$HOME/.local/state/reshell/archive"
   less …/archive/demo@*/daemon.log
   less …/archive/demo@*/history/0001.txt
   ```
4. **Confirm the process was not killed by the host**
   ```bash
   dmesg -T | tail -50          # OOM / kill signals (needs privileges)
   journalctl --user -n 50      # if you use systemd user sessions
   ```
5. **Live session still present but attach fails**
   ```bash
   reshell info <name>          # paths, pid, state
   reshell clean                # clear dead live leftovers (archives first; keeps archives)
   reshell attach <name>
   ```

### What does `reshell clean` do?

| Command | Effect |
|---------|--------|
| `reshell clean` | Remove **dead / orphan live session directories** under the session base dir. History and `daemon.log` are moved to the archive first. Running sessions are untouched. Archives are **not** deleted. (Same sweep `list` / `new` already run.) |
| `reshell clean --all` | Same as `clean`, **plus** delete archived ended sessions (history + daemon logs) from the archive dir. |

### Where are the logs?

| What | Live session | After end (`kill` / shell exit / stale clean) |
|------|--------------|-----------------------------------------------|
| Shell output (primary screen) | `$session/history/*.txt` | `$archive/<name>@<unix>/history/*.txt` |
| Daemon events | `$session/daemon.log` | `$archive/<name>@<unix>/daemon.log` |
| Metadata | `$session/meta.json` | same under archive (`ended_unix`, `end_reason`) |

`end_reason` is one of `shell_exit`, `killed`, `stale`, or `replaced`.

Purge archives when you no longer need them: `reshell clean --all`.

## Why reshell?

reshell is a **session manager**: it keeps one interactive shell (and its children)
alive across SSH disconnects, then lets you reattach. It is intentionally closer
to [dtach](https://cr.yp.to/dtach.html) / [abduco](https://github.com/martanne/abduco)
than to a full terminal multiplexer.

### vs tmux, GNU Screen, Zellij, Byobu

These are **multiplexers** (Byobu is a convenience layer on Screen/tmux). They add
windows, panes, status bars, and a prefix key chord.

| | reshell | tmux / screen / Zellij / Byobu |
|---|---|---|
| **Job** | Survive hangups; reattach to the same PTY | Layouts, panes, tabs, shared scrollback UI |
| **Keys** | Detach byte only (default **Ctrl+\\**; configurable) | Prefix chord (`Ctrl+b`, `Ctrl+a`, …) steals shortcuts from nested apps |
| **Nested TUIs** | Raw passthrough — editors, ratatui apps, and mouse just work | Often need extra config; mouse/keys conflict with the multiplexer |
| **VS Code / Cursor** | OSC 633 passes through; sticky scroll tracks commands *inside* the session | Often eat or rewrite escape sequences unless specially wrapped |
| **Complexity** | One session ↔ one shell | Full virtual terminal + UI chrome |

**Prefer reshell** when you mainly want “SSH died but my shell is still there,”
especially with full-screen editors, and you do not want another layer of
keybindings.

**Prefer a multiplexer** when you need split panes, multiple windows, shared
attach, or scrollback/UI owned by the multiplexer.

You can still run tmux/Zellij *inside* a reshell session if you want both hangup
survival and panes — reshell will not fight it for keys.

### vs dtach and abduco

Same niche (detach/reattach without multiplexing). reshell aims at the same
job with a more complete everyday UX and better behavior for modern TUIs and
editors:

| | reshell | dtach / abduco |
|---|---|---|
| **Core model** | One PTY per named session; raw byte pipe | Same idea |
| **Session UX** | `new` / `attach` / `list` / `kill`; bare `reshell` opens a small session picker (`n` new / attach) | Minimal CLI; dtach has little session management; abduco lists sessions but is otherwise sparse |
| **Reattach redraw** | Restores DEC modes (mouse, alt-screen, …) and forces a two-phase resize so differential TUIs (e.g. ratatui / Fresh) full-paint | Passthrough only — terminal modes and screen contents are not restored; abduco recommends nesting [dvtm](https://github.com/martanne/dvtm) for that |
| **Detach** | **Ctrl+\\** by default; overridable (`--detach-key` / `RESHELL_DETACH_KEY`) | Configurable detach key (similar spirit) |
| **Editor / IDE terminals** | VS Code/Cursor sticky scroll: closes the outer `reshell` command and injects shell integration into the session | No awareness of OSC 633 / sticky scroll |
| **Stack** | Modern Rust binary; Linux-focused packaging via pixi | Small C tools; widely packaged, very mature |

**Prefer reshell** if you live in editors/TUIs and want reattach + mouse + redraw
to work without nesting another terminal layer.

**Prefer dtach/abduco** if you want the absolute smallest C dependency that is
already on the machine, or you already pair abduco with dvtm by habit.

### vs other “keep it running” tools

| Tool | What it solves | Why it is not a substitute |
|---|---|---|
| **mosh** / **Eternal Terminal** | Roaming / high-latency SSH (predictive echo, reconnect) | Great on flaky networks; they are not a general “leave an interactive job on the server and come back tomorrow” session manager for arbitrary TUIs |
| **nohup** / **disown** / **`systemd-run`** | Keep a *non-interactive* process alive after logout | No reattach to a live interactive TTY |
| **`ssh -t` + background tricks** | Ad-hoc survival | Fragile; no first-class attach/list/kill |

**Bottom line:** use reshell as the thin hangup layer; use a multiplexer when you
need panes; use mosh/ET when the *network* is the problem; use nohup/systemd when
you do not need an interactive terminal at all.

## Development

```bash
pixi install
pixi run build
pixi run test
pixi run reshell -- list
pixi run update-version    # bump CalVer YYYY.M.D+N
pixi run conda-package     # build .conda into dist/
```

Always use `pixi run` / `pixi run -- cargo …` so the conda Rust toolchain is used.

## Design docs

Architecture and protocol notes: [docs/DESIGN.md](docs/DESIGN.md), [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT — see [LICENSE](LICENSE).
