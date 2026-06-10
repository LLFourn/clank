# clank-status-tui

A full-screen, READ-ONLY, live-updating status view —
`clank status --tui` — sized to live in a small zellij pane and
show the user where the agents are up to. No input handling
(strictly a display). The more terminal space it gets, the more
it shows; when tiny it still shows the most important thing.

## Build on `clank status --watch`, don't fork it

`--tui` is a third RENDER TARGET over the machinery `--watch`
already has (status.rs), NOT a new data path:
- `build_watcher` + `attach_watcher` — notify on `.clank/` +
  `.git/`, already race-free.
- `StatusSnapshot::build_async` — rebuilt per change; already the
  single source of truth (also feeds `clank html`).
- the existing human renderer produces the status text.
Reuse all three. The snapshot stays the one source of truth;
`--tui` only changes HOW it's drawn (cursor-home + clear-to-EOL
in place, instead of stacked text blocks).

## Responsive layout — the core requirement

Render a PRIORITY-ORDERED list of sections, including from the
top until available rows run out, truncating each line to the
terminal width (ellipsis). Tiers, smallest space first:

1. **Active agent (ALWAYS — even at 1 row).** Who the gate is
   currently waiting on, and doing what, e.g.
   `* ruthless - reviewing - finished-means-impl @ approved_pending_gate`
   If nobody is active: `idle`. This line must always render.
2. **Current plan** — the active plan's name (+ short sha).
3. **State** — gate state + waiting-on reason
   (`approved_pending_gate - commit reviewers approved; waiting on ruthless`).
4. **Queue** — `Queued (N):` then queued plan names in PRIORITY
   ORDER, as many as fit.
5. **Extras (lots of space)** — branch/head/dirty, reviewer-tier
   breakdown, all in-flight plans if more than one, a "watching"
   tick.

Greedy fit: include section K only if the remaining rows hold its
minimum height; otherwise stop. Width: truncate per line. The
smallest useful view is a single line; it degrades gracefully
upward.

## Snapshot gap to close (promote-time note)

`StatusSnapshot` today carries only `queue_count: usize`, but
tier 4 renders queued plan NAMES in priority order. Extend the
snapshot (e.g. `queue: Vec<String>` of stems in priority order,
from the same scan the count comes from) rather than side-loading
a second queue scan in the TUI renderer — the snapshot stays the
single source of truth; `queue_count` then derives from it.
Everything else the tiers need is already on the snapshot
(`plans: Vec<PlanWorkState>` carries `gate` + `waiting_on` for
the active-agent line; branch/head/dirty for the extras tier).

## "Active agent" is derived, not a process probe

The active agent comes from the existing `WorkStatus` /
`waiting_on` (derive_status) — not from probing running
processes:
- `MasterTo{Continue,Commit,Revise,Finalize}` -> master active.
- `ReviewerApprovalsMissing{missing}` -> that commit reviewer.
- `GateReviewersMissing{missing}` -> that gate reviewer.
- `Blocked` -> nobody (awaiting human).
So "active" = whose turn it is to unblock progress. If multiple
in-flight plans each await a different agent, the 1-line headline
summarizes (count / most-salient) and the extras tier lists them.
True "process running right now" liveness would be a separate
signal — out of scope.

## Implementation notes

- No TUI framework. No input -> no raw mode, no event loop.
- Alt-screen via an RAII guard (`Drop` restores show-cursor +
  leave-alt-screen on EVERY exit path) PLUS a
  `std::panic::set_hook` that prints the restore sequence (covers
  `panic=abort`, where Drop won't run). Ctrl-C left to close the
  pane in v1.
- Paint = `\x1b[H` (home), each line + `\x1b[K` (clear to EOL),
  then `\x1b[J` to clear leftover rows from a taller previous
  frame. No full `2J` clear -> no flicker.
- `recv_timeout` on the notify channel (~1s heartbeat) so the view
  re-reads terminal size and repaints on resize without a SIGWINCH
  handler.

## Terminal size: unsafe `TIOCGWINSZ` ioctl via libc (DECIDED)

Responsiveness needs the terminal rows/cols each paint. Do it with
a raw ioctl — `libc` is ALREADY a direct dependency of the cli
crate (libc 0.2.x), so this is zero new deps and no framework.

Portability is the whole point: do NOT hardcode the request
constant — it differs per platform (Linux `0x5413`, macOS/BSD
`0x40087468`, computed via the BSD `_IOR` macro). Use libc's
per-platform definitions so it works on any nix:

```rust
fn term_size() -> (u16, u16) {            // (rows, cols)
    use std::os::unix::io::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: ws is a valid winsize; ioctl fills it or returns -1.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80) // not a tty / piped (e.g. tests) — sane fallback
    }
}
```

Notes:
- `libc::TIOCGWINSZ`, `libc::winsize`, `libc::ioctl` carry the
  correct per-target constant + struct layout — that's what makes
  it nix-portable. Hardcoding the number is the trap.
- Query the stdout fd (the pane's tty). On failure (rc != 0, or
  zero dims when output isn't a terminal) fall back to 24x80 so
  the renderer and tests still work headless.
- Re-query every paint (cheap) so resize is picked up by the
  `recv_timeout` heartbeat — no SIGWINCH handler needed.

## Sizing concerns resolved (ruthless 9d01e47)

1. **Layout is a pure unit-tested function.**
   `status_tui::render(snapshot, rows, cols) -> Vec<String>` —
   the paint loop just calls it. Tests pin: 1 row → active-agent
   headline ALWAYS renders; idle → `idle` at 1 row; every line
   ≤ cols (char-based truncation, `…` ellipsis); greedy cutoff
   (queue tier absent at 3-4 rows, appears at 5 with header +
   ≥1 item — never a bare header); queue names in priority order
   as space allows. Only the ioctl / escape codes / loop are
   untested.
2. **Multi-plan headline rule (deterministic):** N>1 active plans
   → `* N plans: actor1(plan1), actor2(plan2), …` in snapshot
   order (lexicographic by plan key — derive_status folds a
   BTreeMap); width truncation trims the tail; extras tier lists
   the full per-plan breakdown. Exact 2-plan string pinned in
   test. `Blocked` renders actor `human` (awaiting the human, not
   the block creator).
3. **Ctrl-C handled, not documented away:** SIGINT + SIGTERM
   handlers write the restore sequence (async-signal-safe `write`
   + `_exit(128+sig)`) so a direct-terminal user is never left on
   the alt screen with a hidden cursor. RAII Drop covers normal
   exits; the panic hook covers panic=abort.
4. **Snapshot/JSON addition intentional:** `StatusSnapshot.queue:
   Vec<String>` (names, priority order; replaces `queue_count` —
   counts derive from it). `--json` gains an additive `queue`
   array; `queue_count` stays for existing consumers. Emitted
   only when non-empty, same as before.

Extras-tier deviation from the stub: the "reviewer-tier
breakdown" extra is dropped in v1 — reviewer tiers aren't on the
snapshot, and side-loading team config into the renderer would
break the snapshot-is-the-single-source-of-truth rule. Add it by
extending the snapshot if wanted later. Extras shipped:
branch/head/dirty, per-plan breakdown when N>1, last-finished
(idle only), pending-blocks count.

## Out of scope

- Any input / interactivity (quit keys, scrolling, tabs). If ever
  wanted, THEN reach for ratatui+crossterm.
- `--watch`'s existing text/JSON modes — unchanged.
- Adding a `--tui` status pane to `clank open zellij`'s generated
  layout — natural follow-on, separate plan.

## Status

IMPLEMENTED — landed in 6472334 (codex's ffb94b9 review note
"still needs its actual status --tui implementation" predates
seeing that commit; the gate had already moved to the adhoc
commit ffb94b9, so 6472334's diff was never reviewed on its own —
reviewers: it contains the whole feature).

AS SHIPPED, 6472334:
- `crates/cli/src/cli/status_tui.rs` — pure
  `render(snapshot, rows, cols) -> Vec<String>` (tiers + greedy
  fit, char-truncation), `headline`/`actor_of`/`verb_of`,
  `term_size` (libc TIOCGWINSZ), `AltScreen` RAII +
  panic-hook + SIGINT/SIGTERM restore, `paint`
  (home/clear-to-EOL/clear-below), `run_tui` loop over
  `build_watcher`/`attach_watcher` with 1s heartbeat.
- `StatusSnapshot.queue: Vec<String>` (names, priority order)
  replaced `queue_count`; `--json` gains additive `queue` array
  (queue_count kept); html.rs count derives.
- `StatusArgs.tui` (conflicts with --json/--watch/--plan);
  dispatch at the top of `status::run`.
- 8 in-lib unit tests on the pure renderer (1-row invariant,
  idle, truncation, greedy cutoff, queue order, blocked actor,
  extras, 2-plan deterministic headline).

Interaction with the adhoc binary-spawning-test purge (ffb94b9,
attributed to this plan): the TUI's coverage is UNAFFECTED — it
was written as in-process unit tests of the pure renderer from
the start (per the testing rule: test the subroutines the CLI
calls, never spawn the binary). No TUI test was deleted; nothing
needs re-pinning.

Originally queued lloyd 2026-06-09: live read-only status
dashboard for a zellij pane; must stay useful when tiny — always
show the active agent — and progressively reveal current plan,
state, and the ordered queue as space grows.
