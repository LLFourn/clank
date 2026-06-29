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

### Part 1 — one "who acts now" reduction (fixes the symptom) — DONE

Make `work_for` the single answer. The drift was that
`status_tui/derive.rs::awaited_reviewers` re-derived "who is awaited"
by matching `waiting_on` directly, so it missed what `work_for` knows.
The fix: a single predicate **`WorkStatus::is_actionable(agent, role)`**
(= `!work_for(...).is_empty()`), and the TUI's per-reviewer indicator
DECIDES through it — enumerating candidate labels from the `missing`
sets / PR is fine, but the actionability ruling is `is_actionable`, so
the panel and the stop-hook agree by construction.

`head_correction` is a **repo-GLOBAL preempt, not per-plan**:
`work_for` checks `self.head_correction` and returns BEFORE the
per-plan loop (`core/wait.rs:892`), so a broken HEAD idles ALL
reviewers regardless of any plan's `waiting_on`. Routing the TUI
through `is_actionable` inherits that global preempt automatically —
including the cross-plan case (a reviewer missing on a NON-implicated
plan is now idle in the panel, matching the hook).

**The per-plan `MasterToFixCommitTag` marking stays.** It is NOT an
independent second source: `derive_status` computes it from the SAME
`head_correction` (`core/wait.rs:584`), so it cannot drift, and it
gives the implicated plan ROW its correct verb. The duplicate that
caused the bug was the actionability *re-derivation*, now removed.
`attention_state`/`master_is_active` already read `head_correction`
directly, so the master/fixup indicator was never the drift; master
keeps its existing derivation (it also models queue-promote, which
`work_for` does not, so it is a correct superset — full master
routing through `work_for` would need queue-promote modeled there and
is out of scope).

### Part 2 — one core watcher (reduce three → one, shared) — DONE

New module `crate::repo_watch` provides the shared core watcher.

**Design note (corrected from the intro):** the intro proposed
`fs_watcher::path_to_signal` as "the ONE wake classifier." That is too
NARROW — `path_to_signal` only recognizes HEAD / plans / feedback, but
the gate also wakes on `.clank/queue/`, `.clank/blocks/`, and
`config.json`. So the shared core wake rule is
`repo_watch::is_core_wake(path, git_dir, clank_root)` = "under the
gitdir (commits/refs) OR under a `.clank/` gate dir
([`CLANK_WAKE_DIRS`])" — the allowlist extracted from the old
`WakeFilter`. `path_to_signal` stays the STRUCTURED classifier for the
console `runtime` (HEAD/plan/feedback signals); it is unchanged and out
of scope here (the runtime is not yet live-wired to a notify producer).

- `repo_watch::RepoStateWatcher::attach(repo, poll_mode, tx)` watches
  `.clank/` + the resolved gitdir (gitdir skipped in poll mode —
  `wait`'s Codex-sandbox behavior, with the periodic refold as the
  git signal) and sends a wake for each `is_core_wake` event. Storm-safe
  by construction: it never watches the working tree.
- `clank wait` and `clank status` both consume it. Deleted
  `wait.rs::WatchContext` + `build_watcher` (+ `git_resolve_dir`) and
  `status.rs`'s combined `watch_status_paths`/`WakeFilter` gate logic;
  `watch_status_paths` now returns `StatusWatchers { core, diff }`.
  `CLANK_WAKE_DIRS` moved to `repo_watch` (single source; `status`'s
  reuse fingerprint imports it). The console `runtime` is left as-is
  (not live-wired; folding it in is future work, noted not done).
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

- `repo_watch::RepoStateWatcher` is the single gate-state wake producer,
  shared by `clank wait` and `clank status`. `wait.rs::WatchContext` +
  `build_watcher` are deleted; `status.rs`'s combined gate watcher is
  gone (`watch_status_paths` now returns `StatusWatchers { core, diff }`
  and `WakeFilter` is the diff-only filter). The single core wake rule
  is `repo_watch::is_core_wake` (gitdir + `CLANK_WAKE_DIRS` allowlist),
  unit-tested. (`path_to_signal` is NOT the wake rule — it's too narrow;
  it stays the structured classifier for the runtime, unchanged.)
- Poll-mode behavior preserved: existing `wait` poll/heartbeat tests
  pass; gitdir-watch-native-only + periodic refold intact.
- `status --tui`'s per-reviewer "needs to act" DECIDES through
  `WorkStatus::is_actionable` (= `work_for` non-empty), the same
  predicate the stop-hook uses. The `head_correction` case is covered
  **cross-plan** at two levels: a core test
  (`work_for_reviewer_idle_cross_plan_under_head_correction`) proves a
  reviewer missing on a NON-implicated plan is non-actionable under a
  broken HEAD, and a derive test
  (`awaited_reviewers_idle_under_head_correction`) proves the panel
  shows that reviewer idle. The per-plan `MasterToFixCommitTag` marking
  is KEPT (a derived projection of the same `head_correction`, used for
  the implicated row's verb — not an independent source); master's
  indicator is unchanged (already reads `head_correction` directly).
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
