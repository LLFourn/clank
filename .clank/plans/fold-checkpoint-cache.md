# fold-checkpoint-cache

Checkpoint the fold state to disk DURING folds, with
exponentially-spaced retention: dense near HEAD (every commit),
gaps doubling as you go back in history. Any fold then resumes
from a nearby checkpoint instead of the repo root.

Depends on: gix-fold-walker (queued before this; the walker makes
per-commit depth tracking free).

## Motivation

Two cache-strategy gaps surfaced by the bdk TUI incident
(70% CPU; see gix-fold-walker for the producer half):

1. Caches are written at exactly one place — the HEAD a command
   just folded to (`state_cache::write` only ever stores
   `state.head`). They cluster at recently-visited tips. So
   `rebuild_from(HEAD~30, HEAD)` — the TUI log pane, once per
   second — looks for a checkpoint at-or-BEFORE HEAD~30, finds
   none (all caches are NEWER; folds can't run backwards from a
   later state), falls to `RepoState::empty`, and re-folds the
   entire history every frame.
2. `rebuild_from` never writes a cache at all (`write_and_prune`
   is only called by `rebuild_with_diagnostics`) — so even its
   root fold is discarded and redone the next second.

## Design

### Core: pure policy (the decision)

New pure functions in clank_core, alongside the fold — table-
driven unit tests, no IO, per the pure-seam pattern
(`should_open`, `classify_gitignore_body`):

```
pub fn should_checkpoint(gap_since_last: u64, distance_from_tip: u64) -> bool
pub fn prune_plan(checkpoint_depths: &[u64], tip_depth: u64) -> Vec<u64> // depths to delete
```

Spacing rule: allowed gap grows with distance from tip, e.g.
`allowed_gap(d) = max(1, d/2)` (powers-of-two flavored). Near the
tip every commit checkpoints; far back, gaps double → O(log N)
checkpoints total (~14 + tip cluster for a 10k-commit repo, at
~13KB each). Exact function is the implementer's choice; pin the
invariants with tests: (a) gap 1 always allowed near tip,
(b) total checkpoints O(log N), (c) prune_plan keeps a valid
spacing and never deletes the tip checkpoint.

`prune_plan` re-runs after every fold, so as the tip advances,
formerly-dense regions drift out of spec and get thinned —
self-rebalancing. Checkpoints on abandoned branches stop being
refreshed and age out via the existing mtime fallback (keep it
as backstop).

### CLI: mechanical wiring (the write)

- The fold driver loop (rebuild.rs) tracks
  `(last_checkpoint_depth, current_depth)`, consults
  `should_checkpoint` per applied commit, and calls
  `state_cache::write` on true. BOTH pathways: the
  rebuild_with_diagnostics/fold_forward loops AND both phases of
  rebuild_from. Caching becomes a byproduct of any fold —
  self-healing: the first expensive fold pays once and seeds
  checkpoints for everyone.
- Depth = first-parent commit count from root. The walker knows
  it incrementally; a fold resuming from a checkpoint starts from
  that checkpoint's stored depth.

### state_cache: depth in the filename

`<sha>.<depth>.v<N>.bin` (cache format/generation bump; old files
go stale and age out — pre-release, acceptable). Depth must be
readable from one `read_dir` without deserializing payloads:
`prune_plan`'s input and the lookup index come from listing the
dir.

### Lookup (rebuild_from + ancestor path of rebuild_with_diagnostics)

Replace "probe is_ancestor against every cached head" with:
candidates sorted by depth, take the deepest with
depth <= depth(target), confirm with ONE ancestry check (guards
parallel branches at equal depth), walk down candidates on miss.

### Concurrency

Unchanged model: atomic temp+rename writes, lenient try_load,
prune is deletion-only and tolerant of losing races (several
TUI/wfw processes fold concurrently today).

## Testing

- Core policy: pure table tests (spacing, prune invariants).
- CLI: in-process integration — fold a synthetic repo, assert
  checkpoint files exist at expected depths; advance HEAD,
  re-fold, assert thinning; assert rebuild_from(HEAD~k) loads a
  checkpoint and applies only ~k commits (observable via
  diagnostics counter, not timing).
- No binary spawning.

## Non-goals

- TUI in-memory session (don't re-derive at all when nothing
  changed) — composes on top later if the once-per-second
  cheap re-derive ever shows up in top again.
- Caching log events (checkpoints store fold STATE only; events
  are replayed from the nearest checkpoint, which this plan makes
  cheap).
