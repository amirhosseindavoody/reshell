# Re-shell

A lightweight tool to keep shells alive and running after SSH disconnects.

## Features

- Keep shells alive and running after SSH disconnects
- Minimal footprint so CLI tools, TUI apps, and scripts just work — no prefix keys stolen
- Explicit sessions: `new` / `attach` / `list` / `kill`
- Detach with **Ctrl+\** (client exits; session keeps running)
- Targeted at SSH sessions into Linux servers
- Supports bash, zsh, and fish (any interactive `$SHELL`)

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
# Create a detached session (prints the name)
reshell new demo
# or: reshell new          # auto-generated name
# or: reshell new demo --shell /bin/zsh

# Attach (Ctrl+\ detaches without killing the shell)
reshell attach demo

# Create and attach in one step
reshell new work -a

# List sessions
reshell list

# Kill a session
reshell kill demo
```

Session files live under `$XDG_RUNTIME_DIR/reshell` (fallback `/tmp/reshell-$UID`). Override with `--dir` or `RESHELL_DIR`.

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
