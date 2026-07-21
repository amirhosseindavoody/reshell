# Proposed improvements

Ideas for hardening and extending reshell without turning it into a multiplexer.
These are proposals for review — not a committed roadmap. Prefer changes that stay
closer to `dtach` / `abduco` than to tmux/screen/Zellij.

**Out of scope (intentionally):** panes, tabs, status bars, and prefix key chords.
Those fight the product thesis (one PTY, minimal key interception, nested TUIs just work).

---

## Implementation hardening

### 1. Harden attach exclusivity

**Today:** Exclusivity is soft — an `attached` lock file plus a flag in `meta.json`.
A second client can race past a check-then-connect window (TOCTOU between the
client’s pre-check and the daemon accepting the socket).

**Proposal:** Use an advisory lock (`flock` on the session dir or lock file), or
accept the connection then reject under a real lock held by the daemon so only one
writer can own the session.

### 2. Fold PTY `EAGAIN` into the poll loop

**Today:** `write_all_fd` to the PTY busy-waits on `EAGAIN` with a short sleep
instead of participating in the main `poll` loop.

**Proposal:** Drive PTY writes from the same poll path as the Unix socket so large
TUI redraws apply backpressure without burning CPU in a sleep loop.

### 3. Safer auto-generated session names

**Today:** Auto names look like `session-{unix_secs}`, which can collide if two
sessions are created in the same second.

**Proposal:** Add a short random suffix or a monotonic counter so auto names are
unique under concurrent `new`.

### 4. Daemon observability

**Today:** Failures may land in an ad-hoc `/tmp/reshell-daemon-error.log`. Attach
and kill paths have limited structured exit reasons.

**Proposal:** Prefer a per-session log under the session directory, and/or an
optional `--log` / env flag. Surface clearer errors when attach or kill fails
(dead pid, lock held, socket missing, etc.).

### 5. Continuous integration

**Today:** No GitHub Actions (or similar) in-repo; quality depends on local /
pixi `cargo test` runs.

**Proposal:** CI that runs `cargo test` on Linux, and ideally a `linux-64` pixi
build, so regressions (e.g. TUI flood / socket backpressure) are caught on every
change.

### 6. Test DRY and race coverage

**Today:** Protocol encode/decode helpers are duplicated across integration tests.
Blind spots include concurrent attach, stale `attached` lock recovery, and `kill`
SIGTERM → SIGKILL escalation. The full raw-TTY client path is not exercised in CI.

**Proposal:**

- Share framing helpers in a small test utility module/crate path.
- Add cases for concurrent attach, stale-lock recovery, and kill escalation.
- Keep the design note that a full client TTY path may still need a manual or
  external PTY driver; expand coverage where practical without flaky TTY tests.

---

## Small features (still on-brand)

### 7. Default shell from `$SHELL`

**Today:** Default is hardcoded `/bin/zsh` (override with `--shell`).

**Proposal:** Prefer `$SHELL` when set and executable; keep `/bin/zsh` (or a
documented fallback chain) when unset. Update README / design docs accordingly.

### 8. Configurable detach key

**Today:** Only **Ctrl+\** (`0x1c`) detaches — intentional for nested TUI safety.

**Proposal:** Allow override via flag and/or env (dtach-style), with Ctrl+\ as the
default so existing muscle memory and TUI compatibility stay intact.

### 9. Human-readable `list` output

**Today:** `list` prints raw unix timestamps (“good enough for v1”).

**Proposal:** Show relative or formatted times by default. Optional `--json` (or
similar) for scripts that need stable machine-readable output.

### 10. `reshell info <name>`

**Proposal:** Print pid, shell path, socket path, attached/detached, created /
last-active times, and related paths. Useful when debugging “why won’t attach?”
without digging through the session directory by hand.

### 11. `reshell rename` and cleaner stale cleanup

**Proposal:** Rename a live session’s directory + meta without recreate/kill.
Continue improving stale-session cleanup (dead pid, leftover `attached` lock)
especially when people rely on auto-generated names.

---

## Bigger features (still not a multiplexer)

### 12. Optional scrollback / session log

**Today:** Explicit v1 non-goal — PTY output is discarded while detached; DEC
modes are still tracked so reattach can restore and force redraw.

**Proposal:** Optional ring buffer or append-only log while detached; either
replay a limited history on attach or dump to a file. Largest practical gap vs
tmux without adding panes or a status UI.

### 13. Read-only second attach

**Today:** Second attach is rejected (single client per session).

**Proposal:** A “peek” / read-only attach that does not steal the writer seat —
useful for pair debugging while keeping one interactive client.

### 14. Broader VS Code / Cursor shell integration

**Today:** OSC 633 sticky-scroll handling plus bash/zsh inject when detectable;
other shells get passthrough only.

**Proposal:** Extend shell-integration inject for fish (and other shells where
VS Code/Cursor SI is well-defined), without breaking raw PTY passthrough.

### 15. `reshell ssh …` wrapper (post-v1)

**Today:** Explicit non-goal — no transparent SSH wrap.

**Proposal:** A thin wrapper that SSHes to a host and runs `reshell` remotely.
Matches what many people expect from this niche; keep it optional and explicit
so local session semantics stay clear.

---

## Suggested priority

| Priority | Items | Why |
|----------|-------|-----|
| First | §§1–6 (hardening, CI, tests) | Correctness and maintainability without product drift |
| Next | §§7–11 (CLI polish) | Low surface area; matches dtach/abduco ergonomics |
| Later | §§12–15 (optional depth) | Real capability gains; still avoid multiplexer chrome |

When implementing any item, update user-facing README and/or `design.md` /
`protocol.md` in the same change if behavior or interfaces change (see workspace
docs rule).
