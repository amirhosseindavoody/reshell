# Re-shell

A lightweight tool to keep shells alive and running after SSH disconnects.

## Features

- Keep shells alive and running after SSH disconnects
- Minimal footprint so CLI tools, TUI apps, and scripts just work — no prefix keys stolen
- Explicit sessions: `new` / `attach` / `list` / `info` / `context` / `kill`
- Detach with **Ctrl+\** by default (overridable); client exits, session keeps running
- Reattach restores TUI terminal modes (mouse, alt-screen, …), the window/tab title, and forces a redraw
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
# Attach to the most recent session, or create one if none exist
reshell
# same as: reshell attach

# Create a session and attach (default shell is /bin/zsh)
reshell new demo
# or: reshell new          # auto-generated name
# or: reshell new demo --shell /bin/bash

# Create without attaching (prints the name)
reshell new demo --detach

# Keep ~1 MiB of output while detached and replay it on attach
reshell --scrollback 1M new demo --detach

# Attach (Ctrl+\ detaches without killing the shell by default)
reshell attach demo
# or: reshell a demo       # short aliases: n/a/ls/i/c/r/k
# or: reshell attach       # most recently active (or new if none)
# or: reshell --detach-key '^a' attach demo

# List sessions (created + last-active relative times; --json for scripts)
reshell list
reshell ls --json

# Session details (paths, pid, state, …)
reshell info demo
# or: reshell i        # current session when inside one; else most recent

# Recent shell context (last command + trailing output; read-only)
reshell context demo
# or: reshell c        # current session when inside one; else most recent

# Rename a live session
reshell rename demo demo2
# or: reshell r demo demo2

# Remove dead-session leftovers (also runs as part of `list`)
reshell clean

# Kill a session
reshell kill demo
# or: reshell k demo
# or: reshell kill --all   # terminate every live session
```

Short subcommand aliases (also listed in `reshell --help`): `n` new, `a` attach, `ls` list, `i` info, `c` context, `r` rename, `k` kill.

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
**detachable** session names only, while `info` / `context` / `kill` / `rename`
suggest all live sessions (honoring `--dir` / `RESHELL_DIR`). Option flags
(`--dir`, `--scrollback`, …) are not offered on Tab — use `--help` for those.

Session files live under `$XDG_RUNTIME_DIR/reshell` (fallback `/tmp/reshell-$UID`). Override with `--dir` or `RESHELL_DIR`.

Inside a session shell, `RESHELL_SESSION` is set to the session name. Bare
`reshell info` / `reshell context` use the current session (even after `rename`);
outside a session they fall back to the most recently active one.

`reshell context` prints the last known command (when OSC 633 shell-integration
markers are present) and the last ~100 lines of primary-screen output. It does
not attach or replay into the live PTY — useful to recall what a session is
doing. Full-screen apps pause line capture while they own the alternate screen.

Daemon logs go to `$session/daemon.log` by default. Override with `--log` / `RESHELL_LOG`.

Detach key defaults to **Ctrl+\**. Override with `--detach-key` / `RESHELL_DETACH_KEY` (`^\`, `^a`, `0x1c`, or a single ASCII char).

Optional detached scrollback (set when creating a session): `--scrollback` /
`RESHELL_SCROLLBACK` keeps a bounded ring of PTY output while detached and
replays it on the next attach (default `0` = off; examples: `1M`, `512K`; max
`16M`). This is raw byte history for shells — not a multiplexer scrollback UI;
full-screen apps still redraw on attach.

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
| **Session UX** | `new` / `attach` / `list` / `kill`; bare `reshell` attaches (or creates) the most recent session | Minimal CLI; dtach has little session management; abduco lists sessions but is otherwise sparse |
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

Internal architecture notes: [docs/internal/](docs/internal/).

## License

MIT — see [LICENSE](LICENSE).
