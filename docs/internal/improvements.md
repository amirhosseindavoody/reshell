# Proposed improvements

Ideas for hardening and extending reshell without turning it into a multiplexer.
These are proposals for review — not a committed roadmap. Prefer changes that stay
closer to `dtach` / `abduco` than to tmux/screen/Zellij.

**Out of scope (intentionally):** panes, tabs, status bars, and prefix key chords.
Those fight the product thesis (one PTY, minimal key interception, nested TUIs just work).

---

## Implementation hardening

### 1. Harden attach exclusivity — done

**Was:** Exclusivity is soft — an `attached` lock file plus a flag in `meta.json`.
A second client can race past a check-then-connect window (TOCTOU between the
client’s pre-check and the daemon accepting the socket).

**Now:** The daemon holds an exclusive advisory `flock` on `attached` for the
life of the connection. A second socket is closed immediately; `attach` refuses
when the flock is held. Leftover files without a flock holder are recovered as
stale.

### 2. Fold PTY `EAGAIN` into the poll loop — done

**Was:** `write_all_fd` to the PTY busy-waits on `EAGAIN` with a short sleep
instead of participating in the main `poll` loop.

**Now:** Client→PTY bytes go into a `pty_outbound` buffer; the daemon polls
`POLLOUT` on the PTY master and resumes partial writes without sleeping.

### 3. Safer auto-generated session names — done

**Was:** Auto names look like `session-{unix_secs}`, which can collide if two
sessions are created in the same second.

**Now:** Names are `session-{unix_secs}-{4 hex digits}` (from `/dev/urandom`),
with a retry if the directory already exists.

### 4. Daemon observability — done

**Was:** Failures may land in an ad-hoc `/tmp/reshell-daemon-error.log`. Attach
and kill paths have limited structured exit reasons.

**Now:** Per-session `daemon.log` under the session directory; optional `--log` /
`RESHELL_LOG` for fatal errors. Attach/kill errors name dead pid, lock held,
socket missing, etc.

### 5. Continuous integration — done

**Was:** No GitHub Actions (or similar) in-repo; quality depends on local /
pixi `cargo test` runs.

**Now:** `.github/workflows/ci.yml` runs `cargo test --locked` and `pixi run test`
on Linux.

### 6. Test DRY and race coverage — done

**Was:** Protocol encode/decode helpers are duplicated across integration tests.
Blind spots include concurrent attach, stale `attached` lock recovery, and `kill`
SIGTERM → SIGKILL escalation. The full raw-TTY client path is not exercised in CI.

**Now:**

- Shared framing helpers in `tests/common/`.
- Cases for concurrent attach, stale-lock recovery, and kill escalation.
- Full client TTY path still needs a manual or external PTY driver; CI keeps
  talking the wire protocol to avoid flaky TTY tests.

---

## Small features (still on-brand)

### 7. Configurable detach key — done

**Was:** Only **Ctrl+\** (`0x1c`) detaches — intentional for nested TUI safety.

**Now:** `--detach-key` / `RESHELL_DETACH_KEY` accept dtach-style forms (`^\`, `^a`,
`0x1c`, or a single ASCII char). Default remains Ctrl+\.

### 8. Human-readable `list` output — done

**Was:** `list` prints raw unix timestamps (“good enough for v1”).

**Now:** Relative times by default (`2h ago`). `list --json` for stable
machine-readable output.

### 9. `reshell info <name>` — done

**Now:** Prints pid, shell, state, created / last-active, and all session paths.
Optional `--json`. Name omitted → current session when inside one (ancestor pid /
`$RESHELL_SESSION`), else most recently active session. Session shells export
`RESHELL_SESSION=<name>`.

### 10. `reshell rename` and cleaner stale cleanup — done

**Now:** `reshell rename old new` moves a live session directory and updates
`meta.name` (daemon holds a directory fd so file ops survive the rename).
`reshell clean` (and automatic cleanup during `list` / `new`) removes dead-pid
sessions, orphan dirs without meta, and stale attach locks.

---

## Bigger features (still not a multiplexer)

### 11. Optional scrollback / session log — done

**Was:** Explicit v1 non-goal — PTY output discarded while detached; DEC modes
tracked so reattach can restore and force redraw.

**Now:** `--scrollback` / `RESHELL_SCROLLBACK` (set at session create; default
`0` = off) keeps a bounded in-memory ring of detached PTY bytes (max 16M;
suffixes `K`/`M`). On attach, history is replayed as `Data` after DEC mode
restore + clear, then the usual two-phase winsize. Not a VT screen buffer —
TUIs still redraw. Append-only file logging remains a possible later add-on.

### 12. `reshell context` — done

**Now:** `reshell context [name]` asks the daemon for a read-only snapshot: last
OSC 633 command (when shell integration markers are present), exit code, and the
last ~100 lines of primary-screen output. Uses `ContextReq` / `ContextRes` and
does not take the attach lock or replay into the live PTY. Line capture pauses
while the alternate screen is active. Name omitted → current session when inside
one, else most recently active.

### 13. Broader VS Code / Cursor shell integration — done

**Was:** OSC 633 sticky-scroll handling plus bash/zsh inject when detectable;
other shells get passthrough only.

**Now:** Fish is injected the same way VS Code/Cursor do (`--init-command` to
`source` `shellIntegration.fish` after config). Bash/zsh unchanged. Other shells
still get raw PTY passthrough.

### 14. Interactive session picker for bare `reshell` — done

**Was:** Bare `reshell` / `attach` with no name attached to the most recently
active session (or created one if none existed).

**Now:** On a TTY, shows a small picker: **Create new session** first, then a
table of sessions (name / state / created / last-active / shell; detached by
recent activity, then attached; long names truncate with `…`). The session this
process is inside is marked with `*`. Enter / `s` attach (switch), `k` kills
with confirmation, `q`/Esc cancel. An in-session switch asks the outer attach
client to detach the current session (freeing its lock) and attach to the
target. Choosing create-new (or bare `reshell` with no sessions) prompts for a
session name pre-filled with an allocated `session-…` default. Non-TTY (scripts)
keeps the most-recent fallback; empty non-TTY still auto-creates.

### 15. `reshell ssh …` wrapper (post-v1)

**Today:** Explicit non-goal — no transparent SSH wrap.

**Proposal:** A thin wrapper that SSHes to a host and runs `reshell` remotely.
Matches what many people expect from this niche; keep it optional and explicit
so local session semantics stay clear.

---

## Suggested priority

| Priority | Items                        | Why                                                   |
| -------- | ---------------------------- | ----------------------------------------------------- |
| First    | §§1–6 (hardening, CI, tests) | Correctness and maintainability without product drift |
| Next     | §§7–14 done; §§15+ next  | Low surface area; matches dtach/abduco ergonomics     |
| Later    | §§15+ (optional depth)     | Real capability gains; still avoid multiplexer chrome |

When implementing any item, update user-facing README and/or `design.md` /
`protocol.md` in the same change if behavior or interfaces change (see workspace
docs rule).
