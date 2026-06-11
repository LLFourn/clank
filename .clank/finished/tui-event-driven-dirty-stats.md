# tui-event-driven-dirty-stats

Make the status TUI (and `--watch`) fully event-driven — no 1s
poll — and show +/− changed lines when the worktree is dirty.
These are one change, not two: `worktree_dirty` is recomputed per
frame and editing a tracked file emits NO `.clank`/`.git` event,
so today the poll is the only thing keeping `dirty:` fresh.
Dropping the poll REQUIRES watching the working tree.

## Current state (post fold-checkpoint-cache)

`run_tui` (status_tui.rs) and `run --watch` (status.rs) loop on
`rx.recv_timeout(1s)`: watcher events (`.clank/` + `.git/`,
recursive) or a 1s heartbeat; 200ms drain as debounce. The
heartbeat covers three things, none of which need a poll:

1. Resize detection (documented: "heartbeat doubles as the
   resize poll").
2. Watcher-loss backstop (undocumented): `build_watcher` drops
   error events (`if res.is_ok()`, status.rs) — notify signals
   queue overflow AS an error event, so today a missed event is
   papered over by the next heartbeat.
3. Worktree dirty freshness (undiscovered until now): tracked-
   file edits generate no watched event; only the heartbeat
   notices them.

Nothing rendered is clock-relative (no "ago", no countdowns) —
verified — so between events there is genuinely nothing to
redraw.

## Design

### Event-driven core

- SIGWINCH → the existing mpsc channel. NOT from the signal
  handler itself (mpsc send is not async-signal-safe): a small
  forwarding thread via the signal_hook iterator API (or tokio
  `SignalKind::window_change()` task) sends `()` from normal
  context. Resize becomes just another wake.
- Watcher ERROR events also send `()` (fix the `is_ok()` drop):
  an overflow then triggers exactly one rebuild — now cheap —
  instead of silent staleness.
- Loop blocks on `recv_timeout(60s)` — a slow backstop against
  watcher pathologies the error channel doesn't surface (60×
  fewer wakeups than today; self-corrects within a minute). Keep
  the 200ms drain (that is debounce/coalescing, not polling).
- Same treatment for the `--watch` loop in status.rs.

### Worktree watching with ignore filtering

- Watch the repo ROOT recursively (covers the working tree;
  `.clank` and `.git` remain covered as today — drop the now-
  redundant separate watches if the root watch subsumes them;
  keep the separate `.git` watch when the git dir lives outside
  the root, i.e. linked worktrees).
- Filter events through gitignore rules (`ignore` crate
  matcher): a `cargo build` writing thousands of `target/` files
  must not storm the TUI. Exemptions:
  - the `.clank/` subtree is NEVER filtered — feedback/queue/
    agent-config events under gitignored `.clank` dirs are load-
    bearing wake sources today;
  - `.git/` events are never filtered (HEAD moves, ref updates).
  - a change to `.gitignore` itself refreshes the matcher (and
    wakes).
- Watching only "tracked files" individually was considered and
  rejected: directory watchers don't track file churn well, and
  UNTRACKED non-ignored files legitimately dirty the tree.
  Root-watch + ignore-filter achieves the intent robustly.
- Platform note: macOS FSEvents recursive root watch is cheap
  (kernel-coalesced, directory-level). Linux inotify pays per
  directory; acceptable for clank-sized host repos.

### Self-wake hazard: our own probes must be write-free

Once worktree+.git events drive repaints AND each repaint
probes git, plain `git status` can rewrite `.git/index` (stat
refresh) → watcher event → repaint → loop. The 1s heartbeat
currently masks this latent feedback loop; event-driven flushes
it out. Fix in the same change:

- All snapshot probes run `git --no-optional-locks …` (built for
  background tooling) or use gix reads (no index writes).
- Audit StatusSnapshot::build_async's subprocess calls for index
  writers (`status`, `diff`).
- Our own checkpoint writes land under `.clank/cache/` — they DO
  wake the loop once; the drain coalesces it and the rebuilt
  frame is a cache hit. Verify no sustained self-wake (test: N
  consecutive loop iterations with no external changes settle).

### Dirty +/− display

- When dirty, the status/TUI repo line shows shortstat:
  `dirty: +12 −3 · 2 untracked` (exact format implementer's
  choice; one line, no new pane).
- Source: one `git --no-optional-locks diff HEAD --shortstat`
  (staged + unstaged vs HEAD) plus an untracked count
  (`status --porcelain` lines starting `??`, or gix dirwalk).
  Untracked lines are NOT pretended into the +/− numbers.
- Computed per repaint behind the 200ms debounce — milliseconds
  even on bdk-sized repos.
- Pure seam: parsing shortstat/porcelain output into the display
  struct is a pure function with unit tests; the spawn stays at
  the shell.

## Testing

In-process (no clank-binary spawning; git spawns fine):
- Pure: shortstat/porcelain parsing matrix; ignore-filter
  decision function (path → wake/drop) including .clank
  exemption and .gitignore-change refresh.
- Integration: watcher fires on a tracked-file edit (dirty
  transition observed without any poll); build-artifact path
  events are dropped (write under an ignored dir, assert no
  wake); self-wake settles (loop iterations quiesce after one
  rebuild with no external changes).
- Resize: SIGWINCH handling is hard to integration-test
  in-process — keep the forwarding seam thin and pure-test the
  event plumbing around it.

## Non-goals

- In-memory fold session (separate shelf idea; frames are cheap
  enough post-checkpoint that event COUNT, not frame cost, is
  the remaining waste).
- Full diffstat pane / per-file breakdown — one summary line
  only.
- Windows console resize events.
