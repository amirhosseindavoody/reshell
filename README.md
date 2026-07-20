# Re-shell

A lightweight tool to keep shells alive and running after SSH disconnects.

## Features

- Keep shells alive and running after SSH disconnects
- Minimal footprint so CLI tools, TUI apps, and scripts just work — no prefix keys stolen
- Explicit sessions: `new` / `attach` / `list` / `kill`
- Detach with **Ctrl+\** (client exits; session keeps running)
- Reattach restores TUI terminal modes (mouse, alt-screen, …) and forces a redraw
- VS Code/Cursor sticky scroll: finishes the outer `reshell` command and injects shell integration into the session
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

# Attach (Ctrl+\ detaches without killing the shell)
reshell attach demo
# or: reshell attach       # most recently active (or new if none)

# List sessions
reshell list

# Kill a session
reshell kill demo
```

Session files live under `$XDG_RUNTIME_DIR/reshell` (fallback `/tmp/reshell-$UID`). Override with `--dir` or `RESHELL_DIR`.

## Why reshell instead of tmux, screen, or Zellij?

reshell is a **session manager**, not a terminal multiplexer. It keeps one interactive shell (and its children) alive across SSH disconnects, then lets you reattach. It is closer to [dtach](https://github.com/crigler/dtach) / [abduco](https://github.com/martanne/abduco) than to tmux.

| | reshell | tmux / screen / Zellij |
|---|---|---|
| **Job** | Survive hangups; reattach to the same PTY | Windows, panes, tabs, status bars, layouts |
| **Keys** | Only **Ctrl+\\** detaches | Prefix chord (`Ctrl+b`, `Ctrl+a`, …) steals shortcuts from nested apps |
| **Nested TUIs** | Raw passthrough — editors, ratatui apps, and mouse just work | Often need extra config; mouse/keys can conflict with the multiplexer |
| **VS Code / Cursor** | OSC 633 passes through; sticky scroll tracks commands *inside* the session | Multiplexers often eat or rewrite escape sequences unless specially wrapped |
| **Complexity** | One session ↔ one shell | Full virtual terminal + UI chrome |

**Choose reshell when** you mainly want “SSH died but my shell is still there,” especially with full-screen editors or other TUIs, and you do not want another layer of keybindings.

**Choose tmux / screen / Zellij when** you need split panes, multiple windows, shared sessions, or a persistent scrollback/UI inside the multiplexer itself.

You can also run a multiplexer *inside* a reshell session if you want both hangup survival and panes — reshell will not fight it for keys.

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
