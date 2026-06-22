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
just that input, then re-derive the (pure, cheap) display. The per-event
fast path NEVER does a full rebuild; the only full input re-reads are at
startup, on a signalled rescan, and on a slow periodic backstop (below).
**Use what the watcher tells us.**

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

## Resolved (round 1)

- **Correctness vs missed events — TWO-LAYER fallback.** Both layers are the
  SAME operation — a full **input re-init** (today's `build_async`: re-fold,
  re-read `dirty`, re-read all displayed `reviews`, re-open gix), the thing
  the fast path avoids. They differ only in trigger. (1) SIGNALLED drops
  (notify queue overflow / rescan / error / an `Unknown` path) → immediate
  full re-init. (2) SILENT staleness (a misclassified or silently-dropped
  event) → the same full re-init on a SLOW periodic timer (≈30–60s). It must
  be a full re-init, NOT a pure display re-derive: only re-READING the inputs
  can repair an input change whose event we missed (a re-derive from stale
  cached inputs would reproduce the staleness). Because it's amortized (one
  `build_async` per ≈30–60s ≈ a few-% duty cycle, vs many per second today),
  it does NOT reintroduce the per-wake CPU we're removing — it just guarantees
  the view can't stay stale longer than the period. Both layers ship; this
  periodic backstop is the correctness guarantee that makes the incremental
  fast-path safe.
- **Classification must be EXHAUSTIVE-or-rescan.** Every displayed input maps
  to a delta class, and any unmapped path falls to `Unknown → rescan`
  (correct, just non-incremental). High-frequency paths (`feedback/<sha>.md`,
  working-tree, ref/HEAD) get targeted deltas — they drive the CPU.
  Low-frequency displayed inputs (`.clank/plans/`, `finished/`, `queue/`,
  config, blocks) may take a COARSE re-derive of their part (or fall to
  rescan) — fine, they're rare. The impl plan must enumerate the displayed
  inputs and prove the mapping is total (mapped-or-rescan).
- **Fold reuse — don't duplicate the checkpoint incrementality.** The model
  holds the latest `RepoState` (the fold's OUTPUT); a ref/HEAD delta calls
  the EXISTING cached/incremental rebuild to extend it. No second fold.
- **Big deltas (rebase / huge HEAD jump) — accept the freeze.** Keep fold
  extension on the render loop; a rare big delta freezes the spinner briefly,
  which is the intended busy signal. Chunking/off-loop is DEFERRED (revisit
  only if big deltas prove common) — consistent with "keep work on the loop."
- **Displayed-set scoping + eviction.** reviews/log scoped to (plan tips +
  log window); the window grows on pager; evict off-window shas; re-scope on
  HEAD move / history rewrite (the post-rewrite hook migrates feedback onto
  equivalent commits — re-associate the affected shas).
- **Startup.** First build is full (today's `build_async`), then incremental.
- **No new sources of truth (load-bearing).** The model holds the fold's
  OUTPUT (`RepoState`) plus the live inputs (`dirty`, `reviews`-by-sha) — and
  NEVER a second copy of state the fold owns. The display is derived from
  these; no parallel state to drift, which is the trap the bolt-on caches had.

## Acceptance

- The architecture is fully specified: `StatusSnapshot` as a materialized
  view over independent inputs; the watcher as a typed-delta stream; targeted
  per-input updates. The per-event fast path does NO full rebuild; the only
  full input re-inits are startup, a signalled rescan, and the slow periodic
  backstop — one operation, three triggers (consistent with §Resolved).
- Round-1 challenges RESOLVED (above): two-layer missed-event fallback
  (signalled rescan + slow periodic full re-init, NOT a display-only re-derive);
  exhaustive-or-rescan
  classification; fold reuse (no duplicate fold); big deltas accept the
  freeze; no new sources of truth.
- Reviewed from first principles; the doc reflects the challenges.
- NO code — research only; splits into impl plans (classified-wake channel;
  materialized model + delta application; per-input updaters; rescan +
  periodic-backstop correctness).

## Out of scope

- gix conversion of dirty/worktree (`status-dirty-stats-via-gix`).
- The git-shell-out enforcement gate (`gix-not-git-gate`).
- Implementation — this is the architecture; it splits into impl plans
  (classified-wake channel; the materialized model + delta application;
  per-input updaters; the rescan-fallback for correctness).
