# status-incremental-snapshot-fast-path

Implement the core of the finished `status-incremental-snapshot` design (which
was DESIGN-ONLY — it specified the model and never shipped code, which is why
`clank status --tui` still pegs the CPU). This plan delivers the high-leverage
slice that actually kills the bug. The full materialized-view delta stream
(classified-wake channel, per-input updaters) from that design remains a
follow-up; it is NOT required to stop the storm.

Sharpened by the intro-review derivation ruthless did on the (now-dropped)
`status-tui-fsync-storm-research` plan — that research is subsumed here.

## The bug (live evidence)

`clank status --tui` in a worktree whose files churn (a running app) sat at
**35.5% CPU for 3h17m**. `sample` collapsed onto one path:

```
status::run → build_async → log_rows_windowed → rebuild_from → fold_events
  → state_cache::write → File::sync_all → fcntl(F_FULLFSYNC)   729/821 samples
```

Every watcher event throws the whole snapshot away and `build_async` re-gathers
every input from scratch; the log-window rebuild re-folds and the fold writes a
checkpoint with a full fsync — per frame. (This is the "sustained-33%-CPU bug"
the design already named.)

## Root cause (ruthless, independently derived)

The write policy and the evict policy DISAGREE on the survivor set:

- `should_checkpoint` (write, `crates/core/src/checkpoint.rs`) allows a gap of
  `distance/2`, so near the tip (distances 0,1,2,3) it writes EVERY commit:
  tip, tip-1, tip-2, tip-3.
- `prune_plan` (evict) keeps one checkpoint per power-of-two distance bucket
  (bucket0={0,1}, bucket1={2,3}, …), keeping the deepest per bucket — so it
  keeps tip, tip-1, tip-2 but **DELETES tip-3** (and deeper bucket collisions).

So every `rebuild_from` rewrites depth tip-3 (write + F_FULLFSYNC), and the
trailing `prune_checkpoints` deletes tip-3 again. Next frame: identical. The
idempotency guard `status-tui-watch-cpu` added (`final_path.exists()` at
`state_cache.rs:197`) CANNOT help — prune removed the file between frames, so
`exists()` is false every time. Eviction-policy-fights-the-write-policy.

This thrash is latent on EVERY fold, not just the TUI render; it is merely
masked off the hot path elsewhere.

## The fixes (all three; the first two are the model, the third is hygiene)

### Fix 1 — the fold is pure; callers write the cache explicitly

`log_rows_windowed` is a RENDER path but called `rebuild_from`, which folded
with checkpoint writes AND pruned — a read with the side effect of writing and
deleting cache files. A flag-based fix (`checkpoint: bool` buried in
`fold_events`) just hides the write behind which function you call; "the cache
is written only by `rebuild_with_diagnostics`" is no more deliberate-sounding
than `rebuild_from` (lloyd). Make the write EXPLICIT at the call site instead:

- `fold_events` is PURE: it returns the log events and the policy-spaced
  checkpoints it PROPOSES (`FoldOutput { log_events, checkpoints }`), touching
  no disk.
- A deliberate rebuild (`rebuild_with_diagnostics`) calls `persist_checkpoints`
  + `prune_checkpoints` in its own body — every cache write is a visible line.
- The render (`rebuild_from`, and one-shot `clank log` / `clank html`) DROPS
  the proposed checkpoints. It cannot write the cache because it never calls the
  writer — no flag to get wrong.

This deletes the `checkpoint: bool` flag and makes "a render mutates the cache"
structurally impossible — the class of bug, not just this instance. (Cost: the
render clones state at the ~O(log window) proposed-checkpoint depths and drops
them — a few clones per frame, dominated by Fix 2 making renders rare. Worth it
for an explicit write boundary over a hidden flag.)

### Fix 2 — nothing-changed gate at the snapshot level (lloyd's invariant)

> "it shouldn't write the cache if nothing has happened."

In `build_async`: if NOTHING the snapshot depends on changed since the last
snapshot, REUSE it — zero re-fold, zero write, zero fsync. ONE gate, named at
the snapshot level — NOT a `cp.sha==h` fast path bolted onto `rebuild_from`
(that only mirrors `rebuild_with_diagnostics` and leaves the render-writes-cache
smell). This is the first concrete slice of the incremental-snapshot model: the
per-event fast path does no full rebuild.

**The reuse key MUST be the FULL snapshot input set, not just HEAD+dirty
(ruthless, intro review — load-bearing).** `StatusSnapshot` reflects head_sha,
dirty, blocks, queue, master/config, the LOG (commits AND reviews/feedback), and
pr_reviews (status.rs:26-59). Of those, blocks, queue, reviews/feedback, and
config live in gitignored `.clank/` and change WITHOUT touching HEAD or the git
working-tree dirty state. A HEAD+dirty-only gate would reuse a stale snapshot on
a new review verdict (a gitignored `.clank/agents/<label>/feedback` file) — the
verdict would be INVISIBLE in the TUI, exactly what the master watches for — and
likewise on a new block / queued-or-promoted plan / roster change. So the gate
reuses IFF nothing the snapshot depends on changed: ONE signature over ALL inputs
(HEAD, dirty, blocks, queue, reviews/feedback, config, pr_reviews), made EQUAL to
the watcher's wake-worthy set (fs_watcher already wakes on exactly these `.clank/`
paths; it ignores cache/, done/, non-clank, outside-root). A reuse key narrower
than the watcher's wake set hides real changes; wider just costs a rebuild. Name
that one signature; do not let HEAD+dirty stand in for the whole snapshot.

### Fix 3 — align write & evict policies (do regardless)

Make `should_checkpoint` and `prune_plan` agree on the SAME survivor set so a
fresh fold's output survives its own trailing prune (no write→delete→rewrite).
Kills the latent per-fold fsync everywhere, independent of the render fix.

## Testing (no-binary-spawning; in-process cores)

- **Policy fixed-point (would have caught the storm):** write a fold's
  checkpoints, run `prune`, assert ZERO deletions among the just-written
  depths. (Today `cold_fold_write_pattern`, `prune_rebalances`,
  `prune_is_idempotent` each check one policy in isolation; nothing pins their
  AGREEMENT.)
- **Render is read-only:** a `log_rows_windowed`-equivalent over a fixture repo
  writes and deletes ZERO files under `.clank/cache/` (snapshot the dir before/
  after).
- **Nothing-changed reuse:** a second `build_async` with the FULL input set
  unchanged performs no fold and no cache write (assert via the existing
  ODB-open / cache-write fitness counters — extend
  `status_build_opens_the_odb_once_per_phase`). And the dual: mutating a
  gitignored input the watcher wakes on (drop a `feedback/<sha>.md`) does NOT
  reuse — the new verdict appears (guards against a HEAD+dirty-only key).

## Reproduce-first, then validate (settles design Q1 empirically)

ruthless's falsifiable prediction: the mechanism is independent of the guard,
so the FRESH binary still thrashes. Before fixing: restart `clank status --tui`
on the freshly-installed binary in a churning worktree and re-`sample` to
confirm the fsync thrash (proves live bug, not stale binary). After fixing:
re-sample and confirm the process sits at idle CPU when nothing changes.

## Acceptance

- `clank status --tui` sits at idle CPU when nothing the snapshot depends on
  changes (no per-frame fsync). Honesty about "churning repo" (ruthless): if the
  churn is TRACKED, git-dirty changes every frame so Fix 2 can't gate in that
  window — Fix 1 (render reads, no fsync) is what kills the dominant cost there,
  and a per-frame log re-fold remains as residual CPU (the deferred per-input
  incremental model removes that). If the churn is GITIGNORED, the watcher
  shouldn't wake at all. So this line means idle-when-nothing-relevant-changed,
  NOT a claim that continuous tracked churn goes idle. The reproduce-first
  sample names which case the live bug was.
- A render NEVER mutates `.clank/cache/` (Fix 1, pinned by test).
- `build_async` reuses the prior snapshot IFF the full input signature is
  unchanged, and does NOT reuse when a gitignored watched input changes
  (Fix 2, pinned by both directions of the test).
- `should_checkpoint` and `prune_plan` agree on the survivor set (Fix 3, pinned
  by the fold-then-prune-is-a-fixed-point test).
- Reproduced on the fresh binary before the fix; re-sampled idle after.

## Out of scope (follow-ups from the status-incremental-snapshot design)

- The full typed-delta wake stream: classified-wake channel, per-input updaters
  (`dirty`, per-sha `reviews`, `blocks`/config), burst coalescing into a delta
  union. Fix 2 is the reuse-gate that makes these incremental updates the next
  step, not a prerequisite.
- gix conversion of dirty/worktree (`status-dirty-stats-via-gix`).
