# status-incremental-snapshot

**RESEARCH / DESIGN — no code this plan.** Deliverable = this design; splits
into impl plans. This supersedes the earlier bolt-on-cache plans (coalesce /
dirty-stats-cache / review-cache / wake-pipeline) — they were all symptoms of
the wrong model; this names the right one.

## The architectural inversion

**Current (false) model:** ANY watcher event → throw the whole snapshot away
→ `build_async` re-gathers EVERY input from scratch (re-fold [cached], re-run
`dirty_stats` [git subprocess], re-read all displayed reviews, re-open gix) →
render. The watcher's PATH — which says exactly what changed — is discarded
(`tx.send(())`). Per-input caches then get bolted on as damage control. Every
wake pays for inputs that didn't change. (This is the sustained-33%-CPU bug.)

**The right model:** `StatusSnapshot` is a MATERIALIZED VIEW over a set of
INDEPENDENT INPUTS, and the watcher is a stream of typed DELTAS. Each event
already tells us which input changed (its path). Apply a TARGETED update to
just that input, then re-derive the (pure, cheap) display. No full rebuild
except at startup or a signalled rescan. **Use what the watcher tells us.**

## Inputs (each independently maintained, with its delta source)

- `fold` (committed history → `RepoState`) ← **ref/HEAD** events. Already
  incremental (checkpoint fold); extend on HEAD advance — reuse, don't
  duplicate, the existing fold incrementality.
- `dirty` (working tree) ← **working-tree** file events. Recompute via gix
  (see `status-dirty-stats-via-gix`).
- `reviews` (`feedback/<sha>`) ← **`feedback/<sha>.md`** events. Per-sha,
  SCOPED to the displayed commits (active plan tips + log window) — NOT all
  history. Update just the affected sha (handles late reviews on OLD
  displayed commits, not only HEAD).
- `blocks`, `config`/roster, `head_info` ← their own events.
- The DISPLAY (bar, log rows, gate/`waiting_on`, in-progress) is a PURE
  function of these inputs — re-derive after applying deltas (cheap, no IO).
  The gate/`waiting_on` math doesn't change; only input-gathering does.

## Wake → delta

- WorkingTree → recompute `dirty`.
- GitRef/HEAD → extend `fold` + `head_info`; re-scope reviews to the new tips.
- Feedback{sha} → re-read that sha's reviews (if displayed).
- Config/block → reload that input.
- Coalesce a burst → UNION of deltas → apply each affected input once → one
  re-derive + paint.
- Unknown / rescan / notify-overflow / error → full re-init (the correctness
  fallback for DROPPED events).

## What this subsumes

The separately-floated plans (classify wakes, coalesce, dirty-stats cache,
review cache) are all FACETS of this one model — they fall out of "maintain
inputs, apply deltas," instead of being bolt-on caches on a full-rebuild. The
caches were treating symptoms of the missing model.

## First-principles questions for review

- **Correctness vs missed events.** notify drops events on queue overflow →
  a missed delta = stale view. The Unknown/rescan → full-rebuild fallback
  covers the SIGNALLED drop; is a periodic full re-derive needed as
  belt-and-suspenders, or does that reintroduce the cost we're removing? What
  guarantees no silent staleness?
- **Fold reuse.** Hold a live `RepoState` and extend it, or call the existing
  cached rebuild per ref-delta? (Don't duplicate the checkpoint incrementality.)
- **Displayed-set scoping + eviction.** reviews/log scoped to (plan tips +
  log window); the window grows on pager; evict off-window shas; re-scope on
  HEAD move / history rewrite (the post-rewrite hook migrates feedback onto
  equivalent commits — re-associate the affected shas).
- **Startup.** First build is full (today's `build_async`), then incremental —
  clean initialization.
- **Keep work ON the render loop** (the spinner-freeze busy signal is
  intended and load-bearing — it's how this was caught). Per-delta work is
  now tiny; do rare big deltas (a huge HEAD jump) still need handling?
- **No new sources of truth.** The model holds inputs, not a second copy of
  state the fold already owns — avoid drift.

## Out of scope

- gix conversion of dirty/worktree (`status-dirty-stats-via-gix`).
- The git-shell-out enforcement gate (`gix-not-git-gate`).
- Implementation — this is the architecture; it splits into impl plans
  (classified-wake channel; the materialized model + delta application;
  per-input updaters; the rescan-fallback for correctness).
