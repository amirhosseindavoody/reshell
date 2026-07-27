# reshell Improvements

Ideas for hardening and extending reshell without turning it into a multiplexer.
These are proposals for review — not a committed roadmap. Prefer changes that stay
closer to `dtach` / `abduco` than to tmux/screen/Zellij.

## 1. Scope

**Out of scope (intentionally):** panes, tabs, status bars, and prefix key chords.
Those fight the product thesis (one PTY, minimal key interception, nested TUIs just work).

## 2. Implementation Hardening

### 2.1 Harden attach exclusivity — done

**Was:** Exclusivity is soft — an `attached` lock file plus a flag in `meta.json`.
A second client can race past a check-then-connect window (TOCTOU between the
client’s pre-check and the daemon accepting the socket).

**Now:** The daemon holds an exclusive advisory `flock` on `attached` for the
life of the connection. A second socket is closed immediately; `attach` refuses
when the flock is held. Leftover files without a flock holder are recovered as
stale.

### 2.2 Fold PTY `EAGAIN` into the poll loop — done

**Was:** `write_all_fd` to the PTY busy-waits on `EAGAIN` with a short sleep
instead of participating in the main `poll` loop.

**Now:** Client→PTY bytes go into a `pty_outbound` buffer; the daemon polls
`POLLOUT` on the PTY master and resumes partial writes without sleeping.

### 2.3 Safer auto-generated session names — done

**Was:** Auto names look like `session-{unix_secs}`, which can collide if two
sessions are created in the same second.

**Now:** Names are `session-{unix_secs}-{4 hex digits}` (from `/dev/urandom`),
with a retry if the directory already exists.

### 2.4 Daemon observability — done

**Was:** Failures may land in an ad-hoc `/tmp/reshell-daemon-error.log`. Attach
and kill paths have limited structured exit reasons.

**Now:** Per-session `daemon.log` under the session directory; optional `--log` /
`RESHELL_LOG` for fatal errors. Attach/kill errors name dead pid, lock held,
socket missing, etc.

### 2.5 Continuous integration — done

**Was:** No GitHub Actions (or similar) in-repo; quality depends on local /
pixi `cargo test` runs.

**Now:** `.github/workflows/ci.yml` runs `cargo test --locked` and `pixi run test`
on Linux.

### 2.6 Test DRY and race coverage — done

**Was:** Protocol encode/decode helpers are duplicated across integration tests.
Blind spots include concurrent attach, stale `attached` lock recovery, and `kill`
SIGTERM → SIGKILL escalation. The full raw-TTY client path is not exercised in CI.

**Now:**

- Shared framing helpers in `tests/common/`.
- Cases for concurrent attach, stale-lock recovery, and kill escalation.
- Full client TTY path still needs a manual or external PTY driver; CI keeps
  talking the wire protocol to avoid flaky TTY tests.

## 3. Small Features

### 3.1 Configurable detach key — done

**Was:** Only **Ctrl+\** (`0x1c`) detaches — intentional for nested TUI safety.

**Now:** `--detach-key` / `RESHELL_DETACH_KEY` accept dtach-style forms (`^\`, `^a`,
`0x1c`, or a single ASCII char). Default remains Ctrl+\.

### 3.2 Human-readable `list` output — done

**Was:** `list` prints raw unix timestamps (“good enough for v1”).

**Now:** Relative times by default (`2h ago`). `list --json` for stable
machine-readable output.

### 3.3 `reshell info <name>` — done

**Now:** Prints pid, shell, state, created / last-active, and all session paths.
Optional `--json`. Name omitted → current session when inside one (ancestor pid /
`$RESHELL_SESSION`), else most recently active session. Session shells export
`RESHELL_SESSION=<name>`.

### 3.4 `reshell rename` and cleaner stale cleanup — done

**Now:** `reshell rename old new` moves a live session directory and updates
`meta.name` (daemon holds a directory fd so file ops survive the rename).
`reshell clean` (and automatic cleanup during `list` / `new`) removes dead-pid
sessions, orphan dirs without meta, and stale attach locks.

## 4. Bigger Features

### 4.1 Optional scrollback / session log — superseded

**Was / briefly:** In-memory `--scrollback` byte ring replayed on attach, plus
`reshell context` line buffer over the socket.

**Now:** Scraped. Primary-screen output is written to rotating text files under
`$session/history/` (~2000 lines each). Capture pauses on the alternate screen.
`reshell info` lists history paths. No attach replay of history. When a session
ends, history + `daemon.log` + meta are moved to a durable archive
(`$XDG_STATE_HOME/reshell/archive` by default) and listed with
`reshell list --all`.

### 4.2 `reshell context` — removed

Replaced by on-disk history files (§4.1). Use `reshell info` for paths and read
the files directly.

### 4.3 Broader VS Code / Cursor shell integration — done

**Was:** OSC 633 sticky-scroll handling plus bash/zsh inject when detectable;
other shells get passthrough only.

**Now:** Fish is injected the same way VS Code/Cursor do (`--init-command` to
`source` `shellIntegration.fish` after config). Bash/zsh unchanged. Other shells
still get raw PTY passthrough.

### 4.4 Interactive session picker for bare `reshell` — done

**Was:** Bare `reshell` / `attach` with no name attached to the most recently
active session (or created one if none existed).

**Now:** On a TTY, shows a small picker: a table of sessions (name / state /
created / last-active / shell / current history file path; detached by recent
activity, then attached; long names and paths truncate with `…`). The session
this process is inside is marked with `*`. Enter / `s` attach (switch; attached
sessions confirm detach-first), `n` creates (name prompt), `k` kills with
confirmation, `q`/Esc cancel. From inside a session, attach / create / `new`
always leave the current session first (outer client detach + reattach); they
never nest a second attach client. Pressing `n` (or bare `reshell` with no
sessions) prompts for a session name pre-filled with an allocated `session-…`
default. Non-TTY (scripts) keeps the most-recent fallback; empty non-TTY still
auto-creates. `reshell detach` can also free an attached session without killing
the shell.

### 4.5 `reshell ssh …` wrapper — done

**Was:** Explicit non-goal — no transparent SSH wrap.

**Now:** `reshell ssh [dest]` (or `reshell ssh -- <ssh-args…>`) SSHes to a Linux
host, checks that remote `reshell` matches the local version (else
`pixi global install --git …`), creates or attaches a named session, and
reconnects on link drop with backoff (1s → 60s) plus **R** / **Q**. Local process
is a thin `ssh -t` relay; the daemon stays on the server. See [DESIGN.md](DESIGN.md)
§4.4 and the root README.

## 5. Suggested Priority

| Priority | Items | Why |
|----------|-------|-----|
| First | §§2.1–2.6 (hardening, CI, tests) | Correctness and maintainability without product drift |
| Next | §§3.1–4.5 done | Low surface area; matches dtach/abduco ergonomics |
| Later | Native Windows ssh-only build, deeper polish | Optional; WSL + Linux client cover the main path |

When implementing any item, update user-facing README and/or [DESIGN.md](DESIGN.md) /
[PROTOCOL.md](PROTOCOL.md) in the same change if behavior or interfaces change
(see workspace docs rule).
