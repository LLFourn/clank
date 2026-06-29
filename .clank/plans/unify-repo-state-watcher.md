# unify-repo-state-watcher
# Unify the repo-state watcher and the "who acts now" reduction

## Problem

"Who is the waiting-upon agent?" is answered by two subsystems that
re-derive it independently, so they drift — `clank status --tui` can
show an agent as needing to act while that agent's stop-hook
`clank wait` returns nothing (the hook never exits with work).

Two independent layers each duplicate work:

### A. Two reductions of the same gate state

Both paths share the projection — `RepoState::derive_status` →
`compute_gate` produces one `WaitingOn` per plan
(`crates/core/src/wait.rs:428,586`). But "does agent X act now?" is
then computed twice:

- `clank wait`: `RepoState::work_for(author, role)` matches on
  `(role, waiting_on)` (`core/wait.rs:884`).
- `status --tui`: `status_tui/derive.rs` re-matches `waiting_on`
  variants directly (`derive.rs:35,95-100,139-140`) — it never calls
  `work_for`.

They agree only by hand-kept parallel `match` arms. The proven
divergence is `head_correction` (`core/wait.rs:892`): `work_for` has a
TOP-LEVEL short-circuit (broken HEAD tag → master gets a fixup item,
**reviewers get empty**), while the per-plan `MasterToFixCommitTag`
marking the TUI reads is — per the code's own comment
(`core/wait.rs:889`) — "for display only." So a broken commit tag can
show a reviewer as active in the TUI while their `wait` returns
nothing. Reviewer identity binding (`work_for` needs the resolved
`author` to be in the `missing` set the TUI renders as a bare label)
is a second drift vector.

### B. Three watcher layers, two of them duplicate notify wirings

- `fs_watcher::path_to_signal` (`fs_watcher.rs:46`) — a PURE, tested
  path→`FilesystemSignal` classifier (`HeadChanged` /
  `PlanFileChanged` / `FeedbackWritten` / …). Its module doc says the
  "async wiring to notify lives elsewhere" — but that elsewhere was
  never built: **every caller is a test**, zero production use.
- `status.rs::watch_status_paths` (`status.rs:1067`) — hand-rolled
  `notify` wiring; watches the WHOLE repo recursively + gitdir,
  filtered by `WakeFilter` (`status.rs:907`).
- `wait.rs::WatchContext` + `build_watcher` (`wait.rs:798,841`) —
  hand-rolled `notify` wiring; watches only `.clank/` + gitdir
  (gitdir native-mode-only; poll mode relies on a 1.5s heartbeat
  refold), wakes on ANY event.

So "what counts as a wake-worthy change" is defined in three places
(WakeFilter, wake-on-any, and the unused path_to_signal), with
different watch roots and poll handling. This causes refresh-timing
jitter between the TUI and the stop-hook. (It is NOT a permanent
miss — `wait`'s heartbeat self-heals — so B is a smell; A is the
likely root of the "never exits" symptom.)

## Goal

One core watcher and one "who acts now" answer, shared by every
consumer that needs the current waiting-upon agent (`clank wait`,
`clank status`, and the console `runtime`). `status --tui` MAY keep a
SEPARATE watcher for working-tree/diff content (dirty diff lines) —
but that watcher must NOT be a source of waiting-upon state.

## Design

### Part 1 — one "who acts now" reduction (fixes the symptom)

Make `work_for` the single answer. `status_tui/derive.rs`'s per-agent
"needs to act" must be defined as `!work_for(agent, role).is_empty()`
(or both consume one shared `actionable_for`), so the TUI indicator
and the stop-hook are equal BY CONSTRUCTION rather than by parallel
`match` arms.

Remove the display-only second source — but note `head_correction` is
a **repo-GLOBAL preempt, not per-plan**: `work_for` checks
`self.head_correction` and returns BEFORE the per-plan loop
(`core/wait.rs:889`), so a broken HEAD makes master fix it and ALL
reviewers idle, regardless of any plan's `waiting_on`. Folding it into
the implicated plan's `waiting_on` would therefore CHANGE behavior
(reviewers on other plans would stay active); folding it into every
plan conflates a repo-level gate with per-plan state. So the single
representation is a shared top-level **`actionable_for(agent, role)`**
that both `work_for` and the TUI consume — it keeps the global
`head_correction` preempt in one place. Delete the "for display only"
`MasterToFixCommitTag` parallel representation; the TUI's
fixup/attention indicator derives from `actionable_for` too.

### Part 2 — one core watcher (reduce three → one, shared)

Build the missing "notify wiring" the `fs_watcher` doc promised:

- A single `RepoStateWatcher` (new module, or the wiring half of
  `fs_watcher`) that attaches `notify` to the canonical repo-state
  roots — `.clank/` + the resolved gitdir, poll-mode-aware (preserve
  `wait`'s Codex-sandbox behavior: gitdir watch native-only +
  periodic refold) — and classifies every event through
  `fs_watcher::path_to_signal`. `path_to_signal` becomes the ONE
  definition of a wake-worthy change (it is already test-covered;
  this finally wires it into production).
- `clank wait` and `clank status`'s gate refold both consume this
  watcher. Delete `status.rs::watch_status_paths` + `WakeFilter` and
  `wait.rs::WatchContext` + `build_watcher`; both call the shared
  watcher. The console `runtime` (already a `FilesystemSignal`
  consumer, `runtime.rs:10`) should be fed by the same producer if
  cheap.
- **The TUI's working-tree/diff watcher stays separate and clearly
  scoped.** `status --tui` updates dirty diff lines from working-tree
  writes; that is a presentation concern, not gate state. Keep (or
  split out) a dedicated watcher for it that explicitly does NOT feed
  the waiting-upon projection. The core watcher watches `.clank/` +
  gitdir only (the gate inputs) — storm-safe, never the working tree;
  the whole-repo recursive watch that `status` does today is exactly
  the diff/dirty concern and moves to this separate watcher.
- **Carry the storm fix onto the diff watcher.** `WakeFilter` is not
  just a path filter — it holds the git-accurate `git_io::PathIgnore`
  (`status.rs:911`) and feeds the trailing-edge rebuild debounce that
  `status-watch-nested-ignore` landed to kill the ignored-`build/`
  churn CPU storm (~250 wakes/s in the Flutter sim). The new core
  watcher is storm-safe by construction (it never watches the working
  tree), but the DIFF watcher inherits the working-tree watch and MUST
  keep `PathIgnore` filtering AND the deferred-wait/trailing-edge
  rebuild debounce. Deleting `WakeFilter` means MOVING that logic onto
  the diff watcher, not dropping it — re-watching the working tree raw
  reintroduces the storm (a behavior change this plan forbids).

## Acceptance criteria

- One notify-wiring module is the sole producer of repo-state wakes;
  `WakeFilter`, `watch_status_paths`, `WatchContext`, and
  `build_watcher` are deleted. `clank wait` and `clank status` both
  use the shared watcher.
- `fs_watcher::path_to_signal` is the single wake classifier and now
  has production callers (no longer test-only).
- Poll-mode behavior preserved: existing `wait` poll/heartbeat tests
  pass; gitdir-watch-native-only + periodic refold intact.
- `status --tui`'s "needs to act" per agent derives from the shared
  `actionable_for` (= `work_for`). A test asserts the TUI indicator
  and `work_for` agree for every `WaitingOn` variant. The
  `head_correction` case is covered **cross-plan**: with a broken HEAD
  tag, a reviewer on a NON-implicated plan also gets nothing (the
  global preempt), and the TUI shows that same reviewer as idle — not
  just the single-plan case. The display-only `MasterToFixCommitTag`
  duplicate is gone.
- `status --tui` still live-updates dirty diff lines via its separate,
  explicitly-scoped diff watcher, which **retains `git_io::PathIgnore`
  filtering and the trailing-edge rebuild debounce** from
  `status-watch-nested-ignore` — a regression test (or the existing
  storm test) proves the ignored-`build/`-churn storm does NOT return.
- No behavior change to gate semantics, hook firings, the `WaitItem`
  set, the global `head_correction` preempt, or watch CPU cost; this
  is a dedup, not a redesign.

## Out of scope

- Gate/`compute_gate` semantics (unchanged — only where the preempt
  is represented changes).
- The state cache / `input_signature` "nothing-changed" probe
  (`status.rs`) — leave as-is unless it falls out of the watcher
  merge for free.
- PR-review wait items and hook-firing wiring (`firings_from_items`).

## Notes / risks

- Watch-root change for `status`: today it watches the whole repo
  recursively for gate state too; after this, gate state comes from
  `.clank/` + gitdir (same as `wait`), and only the diff watcher sees
  the working tree. Verify the TUI still reacts to plan/feedback/HEAD
  changes (it will — those are under the core roots).
- Two behavior-preservation traps this dedup must not spring (per the
  intro gate review): the global `head_correction` preempt (don't
  per-plan-ize it) and the `status-watch-nested-ignore` CPU storm
  (carry `PathIgnore` + the rebuild debounce onto the diff watcher).
  Both are pinned in the acceptance criteria above.
- Part 1 and Part 2 are independently committable; land Part 1 first
  (it fixes the user-visible symptom) so the watcher refactor can be
  reviewed without behavior risk riding on it.
