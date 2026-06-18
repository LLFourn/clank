# status-tui-watch-cpu

`clank status --tui` panes + their zellij servers burn ~80% CPU at
IDLE (observed 2026-06-18: two zellij servers at 42% + 37%, a dozen
status-tui panes across worktrees at 2–5% each). **No build is
running** when this happens — so the cost is a self-sustaining loop,
not `target/` churn.

## Root cause — a derived-cache wake storm

The engine is a feedback loop between the tui's fs-watch and the fold
cache. Confirmed empirically: with no commits, `.clank/cache/repo-
state/*` mtimes advance every few seconds across multiple sha files,
and `zellij action` subprocesses fire continuously.

The loop:

1. `WakeFilter::wakes` (status.rs:636) wakes the tui on **ANY** path
   under `.clank/` — including `.clank/cache/`, the FOLD CACHE.
2. The tui folds with `CachePolicy::Use`, which writes spaced
   checkpoints under `.clank/cache/repo-state/` (rebuild.rs
   `fold_events` → `state_cache::write`).
3. `state_cache::write` is **non-idempotent**: it builds a temp file
   and `fs::rename`s it over `cache_file_for(head, depth)` **even when
   a byte-identical checkpoint already exists**. The payload is a pure
   function of `(head sha, depth)`, so this is pure churn — but it
   bumps the mtime.
4. That mtime event is under `.clank/`, so step 1 wakes **every**
   clank process folding this repo — the `--tui` pane, the `wfw`
   reviewer long-poll, the master session, sibling panes. They all
   re-fold; racing each other's writes/prunes they don't cleanly hit
   the no-write fast path (`rebuild_with_diagnostics`: `cp.sha == h →
   return`), so they re-write checkpoints → back to step 1.
5. Each woken render also shells `zellij action list-panes` + renames
   (see Amplifier). The **zellij SERVER** does the work of processing
   the repaints + client connections, which is why the servers sit at
   ~40% while the pane processes stay light at 2–5%.

The core modeling error: **the cache is DERIVED state, and derived
state is being used as a wake source.** A fold reads the cache and
re-derives the fold; it must never be woken BY the cache. The genuine
wake signals are the SOURCE state — `.clank/agents/*/feedback`,
`blocks`, `plans`, `queue`, `config`, and git refs — all of which the
watch already covers independently. Waking on the cache is both
redundant (the source change already woke us) and self-triggering.

### Why the existing "no-write cache hit" doesn't save it

`rebuild_with_diagnostics` returns with no write when a checkpoint
sits exactly at HEAD (`cp.sha == h`). That path is real and correct —
but it's defeated because (a) writes are non-idempotent rename-
replaces that churn mtime whenever a fold *does* write, (b) the watch
turns each such write into a wake for every folding process, and (c)
concurrent folders race so they don't all cleanly hit. Fixing (a) and
(b) makes "no commit ⇒ no cache change ⇒ no wake" actually hold.

## Fix 1 (primary, modeling) — wake only on real signals (allowlist)

`WakeFilter::wakes` must wake ONLY on the workflow-state signal dirs
under `.clank` — `plans`, `queue`, `blocks`, `agents` (feedback),
`finished`, `config.json` — plus the git dir, plus tracked working-
tree edits (via the existing gitignore matcher, so `dirty:` stays
fresh). Everything ELSE under `.clank` must NOT wake.

This must be an ALLOWLIST, not the original "anything under `.clank`
wakes except `cache`" denylist — that denylist had a hole ruthless
caught: `path.starts_with(clank_root)` short-circuits BEFORE the
gitignore matcher, so a nested worktree's build
(`.clank/worktrees/<name>/target/*`) and a nested worktree's OWN cache
(`.clank/worktrees/<name>/.clank/cache/*`, a different prefix than the
top-level `cache_root`) BOTH woke the main pane. With ~12 worktrees
under `.clank/worktrees`, every worktree `cargo build` was waking the
main tui. The allowlist closes worktree builds, worktree caches, the
derived `cache`/`html`, and the `zellij` layout by construction —
matching the plan's own "derived / other-repo activity is not a wake
source" model.

- Breaks the self-trigger storm (cache writes no longer fan out into a
  re-fold) AND the cross-worktree storm in one rule.
- Working-tree paths still go through the matcher, so a tracked source
  edit still wakes (instant `dirty:`); build artifacts (`target/`,
  `*.log`) still don't.

## Fix 2 (idempotent write) — no rewrite when nothing changed

`state_cache::write` should skip the rename when `cache_file_for(head,
depth)` already exists. The payload is determined by `(head sha,
depth)`, so an existing file is already correct — rewriting it only
churns the mtime and burns IO. A pruned/missing checkpoint still gets
written (file absent ⇒ write); an identical one is a no-op.

- This directly enforces "no commit ⇒ the cache doesn't change," and
  is defense-in-depth behind Fix 1 (even processes that legitimately
  fold-forward once won't re-churn on the steady state).

## Fix 3 (amplifier) — `PaneStatus` caches the pane-id map

`PaneStatus::update` (status_tui.rs, from tui-agent-pane-status-emoji)
calls `zellij_list_panes()` UNCONDITIONALLY at the top of every
update — the `self.last` dedup gates the RENAME, not the query. The
pane-id map (`label (role)` → pane id) is session-stable, so:

- Fetch `list-panes` ONCE (lazily on first update); cache the map.
- On later updates compute each agent's emoji (pure) and rename ONLY
  the changed ones (existing `self.last` dedup) using the cached id.
- Re-query `list-panes` only when a desired rename target isn't in the
  cache (a pane was added). `TabIndicator` is already efficient —
  leave it.

This cuts the per-render zellij-server load even while the loop is
being fixed, and is correct regardless of Fix 1/2.

## Fix 4 (narrow the OS watch) — DELIBERATELY NOT DONE

Originally proposed: stop the whole-repo recursive watch from making
FSEvents monitor `target/` during `cargo build`, by watching only
`.clank` + the git dir (as `wfw`'s `WatchContext` does).

Dropped after review, for two reasons:

1. **It wasn't the cause, and the wake side is now handled.** The idle
   CPU was the cache-churn → wake loop (Fixes 1–2) plus the
   cross-worktree wakes (Fix 1's allowlist) — none of which need a
   build. Fix 4 only targets the *residual FSEvents cost*: the watch is
   whole-repo recursive, so FSEvents still MONITORS the worktree trees
   under `.clank/worktrees` and DELIVERS their `target/` events during
   a build — the loop just drops them now (allowlist), so they no
   longer drive renders. The leftover cost is event delivery+filtering,
   which FSEvents coalesces; it's build-only and modest next to the
   render storm that's already gone. (Earlier drafts of this note
   wrongly claimed the *denylist* already dropped those events — it did
   not; that was the hole Fix 1's allowlist closes.)

2. **The clean version has a real regression; the no-regression
   version is fiddly.** A recursive OS watch can't exclude a subtree —
   there's no "watch worktree but skip `target/`" knob; you either
   watch the whole tree (current) or watch narrower roots. Narrowing
   the WATCH to `.clank` + git wouldn't even help here — `.clank`
   contains `.clank/worktrees`, so it'd still monitor every worktree
   tree. Truly excluding them means watching each top-level entry
   except `target/`/`worktrees`, a gitignore-inexact heuristic that
   misses nested dirs and newly-created top-level dirs — more machinery
   than the build-only benefit justifies.

If build-time FSEvents load ever proves material in practice, revisit
with the per-sibling watch — but it's out of scope here. The WAKE
storm (the actual CPU) is fully closed by Fixes 1–3.

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- `WakeFilter`: signal dirs (`plans`/`queue`/`blocks`/`agents`/
  `finished`/`config.json`) + git refs + tracked working-tree edits
  wake; `.clank/cache`, `.clank/html`, `.clank/zellij`, and — the
  regression case — `.clank/worktrees/<name>/target/*` and
  `.clank/worktrees/<name>/.clank/cache/*` do NOT wake; gitignored
  worktree paths (`target/`, `*.log`) do NOT wake. (Pure —
  `WakeFilter::with_rules`.)
- `state_cache::write`: writing a checkpoint whose `(head, depth)`
  file already exists is a no-op — assert the file's mtime/inode is
  unchanged (or that no rename occurred via a seam).
- `PaneStatus`: inject a counting fake for `list-panes`; assert it's
  invoked once across N updates (not per update), and that a rename
  fires only when an agent's emoji changes.
- The aggregate CPU drop is verified manually (`ps`, close panes) —
  not unit-testable.

## Non-goals

- `wfw`'s wake reliability ([[wfw-indefinite-wait-resilient]]) —
  separate plan.
- The checkpoint SPACING / prune policy itself — only its write
  idempotency (Fix 2) and the watch's treatment of the cache (Fix 1)
  change; how often/where checkpoints land does not.
- Changing the emoji vocab or which states map to which glyph.
- The OS watch scope (Fix 4, dropped above) — `dirty:` keeps its
  current instant refresh on worktree edits.
