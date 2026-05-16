# Architecture Tech-Debt Sweep

## Reviewer's Checklist

The audit below was scoped against the following six questions, posed
verbatim by the user. Reviewers should weigh the findings and the
proposed phases against each:

1. Are there places where we are not leveraging the simplicity and invariants of the trinity design in the code archiecture.
2. Are there things that are repeated or done ad-hoc throughout the codebase that should be centralized
3. Is the archiecture built around and does it make full use of a core ans-io domain independent strongly typed (ids are not String etc) that figures out the state from an abstract representation of the on-disk state of a repo being watched. Is it as simple as it needs to be (but no simpler).
4. Are there any places where design decisions that could have gone either way chose a direction that made code and archiecture much more complicated when they could have easiyl gone the other way and no one would have cared.
5. Does test code suffer from lack of tooling or missing APIs
6. Is the projected designed about clearly separated layers

## Summary

Trinity's just-completed commit-centric-reviews cutover left the core
shape in good condition — the sans-io derivation in
`disk_snapshot.rs`/`projection.rs` is genuinely pure, the
filesystem-truth invariant holds, and the layer boundaries between
`lifecycle` → `repo_state` → `projection` → `disk_snapshot` →
`git_io`/`runtime`/`server` are mostly respected. But several pivots
left vestigial scaffolding behind that the cutover didn't fully clean
up. The pattern across the audit is the same in each case: a value
that used to be load-bearing is now a derived shadow of something
simpler, but the original lives on as a parallel walker, a bridge
struct, or a wire-stringified copy of a strong type.

This plan enumerates the specific instances, organized against the six
questions above, and proposes a phased sweep to retire them. Most of
the items are pure carrying-cost reduction. One item — Phase 1's
strong-ID validation — closes a latent path-traversal smell where
unchecked `PlanKey::new("../../...")` flows from URL path segments
into filesystem operations. Not a known active exploit, but worth
landing first for that reason.

## Scope Boundaries

Two adjacent plans already capture work that overlaps with parts of
this audit. They are referenced rather than duplicated:

- **`.trinity/stubs/typed-wire-contracts.md`** owns the work of
  introducing a shared typed wire-contract layer between the daemon
  and the Leptos frontend (replacing `serde_json::Value` assembly with
  `Serialize` structs, eliminating the `commits_array` /
  `commits_array_rich` and `gate_value` duplication between
  `mcp_response.rs` and `ui_response.rs`, and giving the frontend
  typed mirrors of the strong-typed IDs).
- **`.trinity/stubs/wfw-alias-and-post-commit-driver.md` §12** owns
  the work of deleting `PlanState::Done` entirely. Done becomes a
  derived predicate over gate state, not a stored axis.

This plan assumes those two land first or in parallel; phases below
that touch the same files note the interaction explicitly.

## Findings

### Q1 — Simplicity and invariants we already have but don't lean on

**`string_newtype!` macro generates unchecked constructors.**
`lifecycle.rs:23` defines `pub fn new(s: impl Into<String>) -> Self`
and `lifecycle.rs:46-55` further provides `impl From<&str>` and
`impl From<String>`, all unchecked. `CommitSha::new("not-a-hex-sha")`,
`PlanKey::new("../../etc/passwd")`, and `AgentLabel::new("")` all
compile and run. Only `PlanId::parse` (`lifecycle.rs:104-122`)
validates. The strong-typed IDs advertise themselves as invariants,
but the macro silently makes most of them strings-in-a-trench-coat.
This is a latent security smell: HTTP handlers in `server/http.rs`
(e.g. `http.rs:247` `RepoBasename::from(repo_basename)`, `http.rs:304`
`CommitSha::from(sha.clone())`) take URL path segments and feed them
into the type as-is.

**`commit_kind_for` is recomputed on every wire-builder pass.**
`projection.rs:610` is the pure function; `mcp_response.rs:310`,
`ui_response.rs:302`, and `projection.rs:141/393/415/700` all call it
each time a response is built. The framing here is *clarity*, not
performance — `commit_kind_for` is microseconds and Trinity holds
small N. The fix is to expose `Plan::commit_kind_of(&CommitSha) ->
Option<CommitKind>` (or a free fn) so callers don't have to know
which four arguments to pass each time. **Explicitly not proposed:
memoizing into a `BTreeMap` cache on `RepoState`.** That would be
premature optimization.

**Three parallel phase enums.** `Phase::{Planning,Implementing,Done}`
(`repo_state.rs:429`), `TimelinePhase::{Plan,Impl}`
(`repo_state.rs:292`) used only inside `TimelineEvent::Review.phase`
(line 284), and `PlanState::{Active,Done}` (`repo_state.rs:359`).
Each has its own `as_str`. Each stringifies to a different wire
value. `PlanState::Done` and `Phase::Done` are computed from the
same predicate (does the plan file live under `done/`?) and exist
simultaneously. `TimelinePhase` is a relic of the pre-cutover
plan/impl split that survived because pre-existing timeline events
had it baked in.

**`WaitArgs` accepts bare `String` for typed fields.**
`server/wait.rs:33-54` declares `role: String`, `plan_id: String`,
`author_label: String`. Validation defers to `PlanId::parse(&args.plan_id)`
inside the handler (`server/wait.rs:120-128`), then the parsed value
is *re-stringified* via `.to_string()` at `server/wait.rs:139` before
it's used. The right shape is a `serde(try_from = "String")` impl on
the newtype so invalid JSON fails at deserialize time, not 20 lines
deep in the handler.

**Cumulative-participant invariant is computed correctly but
re-projected.** `projection::build_commit_gates`
(`projection.rs:689-767`) is the authoritative fold and produces
`CommitGate` per reviewable commit. Then
`commit_gate_to_review_decision` (`projection.rs:498-512`) re-wraps
each `CommitGate` into the legacy `ReviewGateDecision` shape so that
`waiting_on` and the wire builders can consume it. The comment at
`projection.rs:492-497` explicitly flags this as a Phase 2.3 leftover
that should be deleted once callers move onto `&CommitGate` directly.
It wasn't.

**`runtime_snapshot.rs` is a parallel in-memory domain model.**
`runtime_snapshot.rs:15` defines `RepoSnapshot` and line 27 defines
`PlanSnapshot` — both mirror `RepoState`/`Plan` field-for-field.
`RepoSnapshot::from_state` (line 50) clones every map while holding
the runtime mutex; `to_repo_state` (line 63) then synthesizes a
fake `RepoState` from the snapshot for response builders. This is
exactly the kind of speculative defensive copy / parallel
representation that the sans-io direction argues against — readers
should borrow from the live `RepoState` under the mutex, or take
narrow typed query DTOs, not clone the whole world and rebuild it.

**Fake `Option<T>` fields encode variant shape instead of using
typed enums.** Three concrete instances:

1. `server/wait.rs:71-87` — `WaitResponse::Work` carries
   `target_sha: Option<String>`, `commit_kind: Option<String>`,
   `prompt_hint: Option<String>` with `skip_serializing_if`.
   `WorkItem` (`server/wait.rs:175-188`) mirrors the same three
   Options. These aren't "really optional" — they're present for
   every `review_commit` / `address_commit_changes` work item
   and absent only for terminal `SessionDone`. The shape would be
   honest as one variant per work kind:
   `ReviewCommit { target_sha, commit_kind, prompt_hint, .. }`,
   `AddressCommitChanges { ... }`, `CommitPlanRevision { ... }`,
   `MoveForward { ... }`, `SessionDone`.
2. `frontend/src/api.rs:69-79` — `ReviewGate.phase:
   Option<String>` (it's only `None` for the now-deleted held-
   feedback case) and timeline event structs with `phase` /
   `plan_touch` as Options (variant-tag smell). These overlap
   with the `Phase`/`TimelinePhase` deletion in Phase 9.
3. `frontend/src/store.rs:38-46` — live-event struct with
   `plan_id`, `slug`, `state` as `Option<String>` because some
   events are "repo-level" and some are "plan-scoped". The wire
   shape lets a repo-level event drift through the UI as if it
   were plan-scoped with garbage fields. The honest shape is a
   tagged event enum (`LiveEvent::Plan { plan_id, slug, state,
   .. } | LiveEvent::Repo { .. }`).

The pattern: `Option<T>` is being used as "the caller knows when
this is present" — a JSON-assembly convenience that hides what
should be a variant invariant. Same class of debt as fake caches
and fake bridges.

**WFW / get-context plan selection is split across surfaces.**
The same conceptual "which plan?" question resolves differently in
three places: MCP `wait_for_work` (`server/mcp.rs:48-88`) infers
from caller cwd → single-active-plan; HTTP `/api/wait_for_work` and
core `server/wait.rs:115-121` still reject empty `plan_id` outright;
tests construct their own. The resulting bug (HTTP returns
`plan_id is required` while MCP succeeds for the same daemon state)
is a layering-violation symptom: the *policy* of how to resolve a
selector is duplicated rather than centralized. Fix is a typed
`PlanSelector` enum (`Explicit(PlanId) | InferFromCwd(Path) |
InferFromRepoFilter(RepoBasename)`) consumed by one resolver, used
by every entry point.

### Q2 — Repetition / ad-hoc patterns

**Response builders are 95% duplicated.** The audit identified four
100%-duplicate functions across `mcp_response.rs` and `ui_response.rs`:

| Function | mcp_response.rs | ui_response.rs | Lines | Drift risk |
| --- | --- | --- | --- | --- |
| `gate_value` | 443–472 | 448–474 | 32 | hardcoded `"plan"`/`"impl"` strings |
| `waiting_on_value` | 434–441 | 439–446 | 7 | low |
| `pr_hint_value` | 392–432 | 403–437 | 40 | hardcoded prompt strings |
| `commits_array` vs `commits_array_rich` gate block | 306–347 | 289–338 | 49 | 95% identical, differs only in feedback payload richness |

`ui_response::timeline_value` (476–536) additionally *rebuilds* the
timeline from `commit_order` and `attribution` instead of calling
`state.timeline_for()`, which `mcp_response::timeline_value`
(351–390) already does. (Typed-wire-contracts will absorb most of
this, but the timeline rebuild is a logic divergence the contracts
plan won't surface on its own — it needs to be called out as a fix
target.)

**Wire-string constants hardcoded instead of derived from enum.**
`gate_value` hardcodes `"plan"` / `"impl"` (mcp:457–460, ui:458–462)
rather than calling `phase.as_str()`. `commits_array` hardcodes
`"commit_mixed"` / `"commit_plan"` / `"commit_impl"` (mcp:363–366,
ui:500–504) rather than `commit_kind.as_str()`. Adding a `CommitKind`
variant means grepping for these strings rather than chasing a
compile error.

**Test helpers `gate()` and `agents()` duplicated identically.**
`projection.rs:782, 789` and `server/wait.rs:495, 499` define the
same two helpers byte-for-byte. `disk_snapshot.rs:213-262` has its
own parallel set (`sha()`, `sess()`, `plan_file()`, `entry()`,
`feedback()`) used only inside its own tests. Across the three
modules that's 17 helper functions covering largely the same shape.

**Hardcoded feedback-path literals in tests.**
`runtime.rs:748, 789` builds `format!(".trinity/feedback/foo/commits/{}/alice.md", intro.as_str())`
inline. `server/wait.rs:804, 805, 873` does the same. `disk_format.rs`
has `parse_feedback_path` (the inverse) but no
`FeedbackPath::canonical_str(...)` for tests to use. The path schema
is now in two places: the parser and ~15 inline `format!` calls.

**Four "latest_*" wrappers in `projection.rs` (351-372) are pure
delegators.** `latest_plan_touching_commit` and `latest_impl_commit`
just call `all_plan_revisions().last()` / `all_implementation_commits().last()`.
Inlining at callers would remove 4 one-liner functions; if any of
them needed to grow logic that's still a single edit.

### Q3 — Sans-io core + strong typing at the boundary

**Sans-io core itself is in good shape.** `disk_snapshot.rs` does no
IO, `projection.rs` is pure, `attribution.rs` and `disk_format.rs`
are pure parsing. The layering is real. The problems are at the
boundary, not the centre.

**Stringification at the wire boundary is everywhere.** `.as_str()`
appears 20+ times across `mcp_response.rs` and `ui_response.rs`
converting `CommitSha`, `PlanKey`, `AgentLabel`, `Phase`,
`ReviewGateState`, `ReviewVerdict`, `PlanTouchKind` back to plain
strings before insertion into a `serde_json::Value` (see audit table:
mcp:146/249/271/276/279/316–318/332/333/340/370/371/384/386/395/438/463/476
and equivalent in ui). The newtype macro already derives
`#[serde(transparent)]`, so a `Serialize`-driven path would emit the
same JSON without the manual round-trip. Typed wire contracts (see
Scope Boundaries) will fix most of this; the structural debt
identified here is that the manual stringification grew up *because*
the wire shape was untyped `Value`, so the two issues are the same
problem from two angles.

**Strong types accept any `String` at construction.** Covered in Q1;
restating here because it directly contradicts question 3's premise.
A `CommitSha` that doesn't reject `"hello"` isn't doing the job a
strong type implies.

**`Trinity` dual-index is justified.** `repo_state.rs:16-23` holds
`repos: BTreeMap<RepoRoot, RepoState>` plus
`repo_basenames: BTreeMap<RepoBasename, RepoRoot>`. The reverse
index is hit at every ID resolution (`server/http.rs:171`,
`server/wait.rs:434`, `server/mcp.rs:165, 311`) — not speculative.
No change proposed.

### Q4 — Design decisions that went the complicated way for no reason

**`ReviewGateDecision` is a legacy bridge with no remaining
consumers that couldn't take `CommitGate` directly.**
`review_state.rs:69-91` defines it; `projection.rs:498-512` projects
from `CommitGate`; `projection::waiting_on` takes
`gate: Option<&ReviewGateDecision>`; everything downstream of
`waiting_on` only reads `state`, `participants`, `missing_approvals`
— all of which exist verbatim on `CommitGate` (just under different
field names: `requesters`/`ambiguous`/`unmarked` is a slightly
different slicing, but the consumers don't use the distinction).
Changing `waiting_on` to take `&CommitGate` and deleting
`ReviewGateDecision` is a ~50-line removal. No one would notice.

**Frontend `FeedbackEntry` vs `CommitFeedback` bridging.**
`frontend/src/api.rs:101-109` defines `CommitFeedback` (per-commit:
`author, verdict, body_raw, body_html, path, created_at`) and
`api.rs:129-137` defines `FeedbackEntry` (legacy flat:
`target_sha, author, verdict, body_raw, body_html, path, created_at`).
The only difference is the explicit `target_sha` field, which is
trivially derivable from the parent commit when reading `commits[]`.
`frontend/src/components/session_detail.rs` has a
`commit_fb_to_feedback_entry` bridge for exactly this. Pick one
shape; delete the bridge.

**Four `latest_*` wrappers in `projection.rs`.** Already noted under
Q2 but the deeper observation is that they exist because
`build_commit_gates` returns a `BTreeMap<CommitSha, CommitGate>` and
callers want "the most recent reviewable thing of kind X". A single
function `latest_commit_matching(&self, pred: F)` plus the
`is_reviewable()` helper on `CommitKind` covers all four cases.

**`self_writes` ring is dead-allow'd.** `runtime.rs:27, 243-264`. The
ring was a watcher de-bouncer for self-initiated renames (plan moves
to `done/`). Post-2.4 the runtime no longer initiates plan moves
(the user/agent does it via shell), so nothing populates the ring.
The two methods (`mark_self_writes`, `should_skip_self_write`)
carry `#[allow(dead_code)]`. Either delete or document the
re-introduction plan.

**`api_move_to_done` HTTP endpoint contradicts the
filesystem-truth-only direction.** `server/http.rs:31` routes
`POST /api/plan/{repo}/{stem_md}/done` to `api_move_to_done`
(`http.rs:414`), which does a raw `std::fs::create_dir_all` +
`std::fs::rename` (around `http.rs:429`/`http.rs:436`) to move the
plan into `done/`. This is the "master bookkeeping button" that
`wfw-alias-and-post-commit-driver §12` argues should not exist as a
concept. Strictly speaking the endpoint's deletion is owned by
§12, but the plan should cross-reference it so the
§12 reviewer knows the rename code is the artifact to delete.


### Q5 — Test tooling

**Three discrete pain points, all addressable with one shared module:**

1. **Helper duplication.** 17 helpers across 3 modules
   (`projection.rs`, `server/wait.rs`, `disk_snapshot.rs`); two pairs
   are byte-identical (`gate`, `agents`).
2. **Verbose struct literals.** `CommitGate` and `ReviewGateDecision`
   are built in tests with 6–7 `Vec::new()` / boilerplate fields per
   call. Examples at `projection.rs:1116-1124` and
   `projection.rs:796-804`. A `CommitGateBuilder::default().with_approvers([...])`
   pattern would cut each call by 4-5 lines.
3. **Path construction.** `format!(".trinity/feedback/{sid}/commits/{sha}/{author}.md")`
   appears 15+ times across the test suite. The path schema's
   parser is in `disk_format.rs`; the inverse builder isn't, so
   every test owns its own copy.

The recommended shape is a `src/test_helpers.rs` (with
`#[cfg(any(test, debug_assertions))]` or a `test-helpers` feature
flag) that consolidates the three.

**Tests reach into `Runtime` internals.** `runtime.rs:578, 619` use
`rt.state().lock_owned().await` directly. `runtime.rs:642, 655, 668`
use `rt.read_repo(...)` (a public accessor, so legitimate, but
suggests `Runtime` has no higher-level snapshot API). Worth a
test-focused method that returns the read-only projection a test
typically wants without holding the lock.

**`tests/end_to_end.rs` is 1707 lines** (a single file). Serializes
test execution via a global `HOME_LOCK` mutex because tests mutate
`$HOME`. Two problems: the file size (refactor target) and the
global-state mutation (design smell — tests should inject the home
directory rather than serialize on a mutex). Splitting the file
into `tests/wait_for_work.rs`, `tests/feedback_ingest.rs`,
`tests/multi_repo.rs`, plus a shared `tests/common/` module that
exposes an injectable test-harness, addresses both.

**Frontend wire structs masked by blanket `#[allow(dead_code)]`.**
`frontend/src/api.rs:100, 112, 122, 128` carry blanket
`#[allow(dead_code)]` on `PlanRow`, `CommitFeedback`, `CommitEntry`,
`FeedbackEntry`. Most of these aren't really dead — they're
deserialized via `serde` and the compiler doesn't see field reads
through derive. But the blanket allow masks fields that genuinely
*are* unused. The right shape is `#[serde(deny_unknown_fields)]` on
each struct plus removing the blanket allow so the compiler can
warn on actually-dead fields.

### Q6 — Layer separation

**Pure layers are clean by two criteria.** First, `grep "^use crate::"`
across `src/` confirms no pure layer imports `runtime`, `server`, or
`tokio`: `projection.rs` imports only `lifecycle`, `repo_state`,
`review_state`; `disk_snapshot.rs` imports only `lifecycle`,
`disk_format`, `attribution`; `attribution.rs` imports only
`lifecycle`, `disk_format`. Second, `grep -rn 'std::fs\|tokio::fs'`
returns zero hits inside `projection.rs`, `attribution.rs`,
`disk_format.rs`, `disk_snapshot.rs`, `repo_state.rs`,
`review_state.rs`. Both criteria pass.

**`runtime.rs` mixes five distinct responsibilities** (890 lines):

1. Mutex/state ownership (lines 86–203)
2. Filesystem signal dispatch (lines 276–420)
3. Broadcast channel / SSE event bus (lines 231–446, interleaved)
4. Self-writes ring (lines 243–264, dead)
5. Feedback ingestion + commit refresh (lines 447–519)

Of these, #4 is dead code (Phase 4 deletes it). The other four are
cohesive enough that splitting into separate modules right now would
be premature; there's no second caller, no testability gap, no
measured maintainability problem beyond "the file is long". Flagged
as a finding so a future plan can re-evaluate when a concrete reason
appears (a unit test that can't reach the seam, a second caller).

**Test-only layer crossing.** `mcp_response.rs:492` imports
`crate::runtime::Runtime` inside `#[cfg(test)]`. Tests for a pure
response builder shouldn't need the live runtime. Either move that
test to `tests/end_to_end.rs` or rewrite it against a synthetic
`RepoState`. This is a symptom rather than a root cause: the deeper
problem (the entire response surface being assembled as
`serde_json::Value`) is owned by `typed-wire-contracts.md`.

**HTTP handlers bypass `Runtime` for state-mutating filesystem
writes.** `server/http.rs:414` `api_move_to_done` is the worst
offender: it calls `std::fs::create_dir_all` and `std::fs::rename`
directly (around lines 429 and 436), moving the plan file without
going through the watcher's mutex, without checking gate state
invariants, and without coordinating with the (now-dead) `self_writes`
ring. Plan-mutating actions should live behind a `Runtime` method
that holds the mutex, validates, and emits the event. (The endpoint
itself goes away with `wfw-alias §12`; if it stays for the interim,
the IO must be routed through Runtime.)

**Hand-rolled JSON serialization of fully-typed Rust diff data.**
`server/http.rs:616` `serialize_diff_files` walks
`crate::diff_parser::FileDiff` / `Hunk` / `Line` and manually
builds `json!({...})` objects from each field. The structs are
already Rust types; deriving `Serialize` with `rename_all =
"snake_case"` and returning `axum::Json<&[FileDiff]>` removes ~35
lines. Owned by `typed-wire-contracts.md`; called out here so that
plan's review-target list includes it.

**`Phase` enum is load-bearing but coincides with `CommitKind` of
the latest reviewable commit.** Audit confirmed 6 call-sites in each
of `mcp_response.rs` and `ui_response.rs` that branch on `Phase`. The
underlying truth — "is this plan currently waiting on a plan-only
revision, an implementation commit, or done?" — is already encoded
in the latest reviewable commit's `CommitKind` (PlanOnly | Mixed →
"planning posture"; CodeOnly → "implementation posture"). `Phase` is
a parallel encoding of the same information that
`build_commit_gates` already walks past. Folding `Phase` into a
`fn current_posture(&self) -> Posture` derived from latest reviewable
commit kind would let `phase_for` and the `Phase` enum delete, with
the wire field becoming a function of the same projection. This is
adjacent to `wfw-alias §12` (deleting `PlanState::Done`) but
distinct.

## Goals

- Strong-typed IDs that actually validate at construction and at
  deserialize. `CommitSha::new("hello")` no longer compiles.
- One canonical `CommitGate` shape consumed everywhere; delete the
  `ReviewGateDecision` bridge.
- Shared `test_helpers` module (plain `#[cfg(test)]`, no feature
  flag, no builder type); eliminate the duplicated `gate` /
  `agents` / `feedback_with` / path-format helpers.
- `self_writes` ring deleted. Other `runtime.rs` extractions only
  if a concrete reason emerges.
- Four `latest_*` delegators in `projection.rs` inlined at callers;
  `commit_kind_for` accessible via a single-arg method on `Plan`;
  existing "Hot path" micro-optimization in `commit_kind_for`
  collapsed to one clear expression.
- `runtime_snapshot.rs` deleted; response builders read live
  `&RepoState` directly or take narrow typed query DTOs (no
  `to_repo_state()` compat bridge).
- Unified `PlanSelector` resolver path consumed by MCP, HTTP, and
  tests — no per-surface bespoke plan-id resolution.
- Fake `Option<T>` fields (variant-tag Options on `WaitResponse`,
  timeline events, live events) replaced with typed enum variants.
  Every remaining `Option` answers a real "absence is meaningful"
  question.
- `wait_for_work` timeout cap (`MAX_TIMEOUT_SECS = 300`) removed
  from daemon and shim; default raised from 60s to 1800s
  (30 minutes). Trinity stops imposing a semantic timeout ceiling
  on its blocking coordination primitive.
- `Phase` and `TimelinePhase` enums deleted (folded into
  `CommitKind`-derived posture); coordinated with `wfw-alias §12`
  for `PlanState::Done`.
- Frontend feedback shape unified — pick `CommitFeedback`, delete
  `FeedbackEntry` and the bridge; remove blanket `#[allow(dead_code)]`
  on wire structs.
- HTTP handlers stop bypassing `Runtime` for state-mutating IO.
  (The `api_move_to_done` endpoint goes away with `wfw-alias §12`;
  any other state-mutating endpoints route through `Runtime`.)
- Test harness injects `$HOME` instead of mutating it via a global
  mutex (mandatory final phase, not optional).

## Premature Optimization: Explicitly Rejected

This plan deliberately does NOT propose the following, even though
the first draft did. Future plans / reviews should also reject these
unless a *measured* problem appears:

- **Memoizing `commit_kind_for` into a cache on `RepoState`.** The
  function is microseconds; Trinity holds small N. Recomputing is
  fine. The clarity problem (four-arg call) is solved by an
  accessor method, not a cache.
- **`CommitGateBuilder` type.** Six-field struct with sane
  defaults solves with `..CommitGate::default()` struct-update
  syntax. A separate builder type is overkill.
- **`from_validated` internal escape hatch on newtype
  constructors.** Validating-on-construction is cheap; the
  "internal-only unchecked path" exists for hypothetical perf wins
  that don't exist. Just `parse(...).expect("invariant")`.
- **Splitting `runtime.rs` into three modules.** 890 LOC is big
  but cohesive; no caller or test demands the extraction. Defer
  until a concrete reason emerges.
- **`latest_commit_matching<F>` higher-order primitive.** Three
  callers don't justify the abstraction. Just inline `.last()`.
- **`test-helpers` cargo feature flag.** Nothing outside tests
  consumes them. Plain `#[cfg(test)]` is sufficient.
- **Debug-asserts / wrapper struct around `Trinity`'s `repos` +
  `repo_basenames` invariant.** The invariant is local to two
  mutation entrypoints; no need for a structural enforcer.

## Non-Goals

- Typed wire contracts (covered by `typed-wire-contracts.md`). This
  plan calls out the duplication as evidence, but the structural fix
  belongs in that plan.
- Deleting `PlanState::Done` (covered by
  `wfw-alias-and-post-commit-driver.md §12`).
- Frontend redesign / Leptos refactor. This is daemon-side cleanup
  plus a small frontend shape unification.
- New tests for already-tested code. Only test-tooling shape changes.

## Phases

**Execution policy: one sweep, no per-phase pauses.** This plan
is meant to be implemented end-to-end in a single pass. Do not
stop for review or approval between phases. Do not request feedback
after each phase, do not move the plan to `done/` early, and do
not break the sweep into separately-merged PRs unless a phase
literally cannot land without an out-of-band dependency. The
phase numbering is a *recommended ordering* (so dependencies are
honoured), not a checkpoint cadence. Reviewers grade the whole
sweep against the Acceptance Criteria at the end; intermediate
state will look incomplete by design.

Phases form a recommended order with explicit dependencies. The
ordering is: **Phase 1 (typed IDs) is independent and should land
first**; **Phase 2 (retire `ReviewGateDecision`) is independent of
Phase 1** and can run in parallel; **Phase 3 (test helpers) depends
on Phase 1's validated parsers**; **Phase 4 (runtime dead-code
removal) is independent of all others**; **Phase 5 (walker collapse
+ accessor + strip hot-path branch) is independent**; **Phase 6
(delete `runtime_snapshot.rs`) is independent of 1-5 but touches
response builders**; **Phase 7 (`PlanSelector` resolver) is
independent**; **Phase 8 (purge fake `Option<T>`) is independent
but overlaps with Phase 9 on TimelineEvent shape — sequence 8 → 9
to avoid touching TimelineEvent twice**; **Phase 9 (`Phase` enum
deletion) interacts with `wfw-alias §12` and the ordering is
specified below**; **Phase 10 (frontend feedback unification)
depends on Phase 9**; **Phase 11 (`wait_for_work` timeout cap +
default) is independent**; **Phase 12 (test-file split,
mandatory) lands last**.

### Phase 1 — Strong-typed ID validation (three sub-steps)

This is bigger than a single commit because the unchecked `new` /
`From<&str>` / `From<String>` constructors are used at 50+ call
sites; the call-site sweep is the bulk of the work. Split:

**1a — Add validated parsers.**
- For each type in `lifecycle.rs`, add
  `pub fn parse(s: &str) -> Result<Self, IdError>` with the
  appropriate validation:
  - `CommitSha`: 4–40 lowercase hex chars.
  - `PlanKey`: non-empty, no `/`, no `..`, no leading dot.
  - `RepoBasename`: non-empty, no `/`.
  - `AgentLabel`: non-empty, no `/`, no leading dot.
  - `ContentHash`: existing format (verify in code).
- Keep `new` / `From<&str>` / `From<String>` working temporarily so
  call-sites compile during the migration. Callers that know the
  value is valid use `parse(...).expect("invariant")`. No
  `from_validated` escape hatch.

**1b — Migrate call-sites.**
- Every call-site that currently uses `new` / `.into()` / `From`
  on these types: either replace with `parse(...).expect("known
  valid")` (where the input is genuinely known-valid, e.g.
  internal disk-snapshot post-parse paths), or with `parse(...)?`
  (where the input is external).
- The highest-traffic targets: `server/http.rs:247` (URL path
  segments), `server/http.rs:304` (URL path segments),
  `server/mcp.rs` MCP tool argument parsing, `tests/end_to_end.rs`.
- After this step, `cargo build` should still pass with the old
  unchecked constructors present but unused.

**1c — Remove unchecked constructors; switch `serde::Deserialize`
to validate.**
- Delete `string_newtype!`'s `new` and `From<&str>` / `From<String>`
  impls. The macro now produces only `parse`.
- Implement `serde::Deserialize` via `try_from = "&str"` so JSON
  inputs fail at deserialize time, not 20 lines into a handler.
- Update `WaitArgs` (`server/wait.rs:33-54`) and any other
  request struct that declares `String` for an identifier field
  to use the typed variant.
- Tests: invalid inputs reject at deserialize; existing tests
  pass unchanged (helpers already use valid SHAs etc.).

**Security note**: phase 1 closes the path-traversal latency in
`PlanKey::new("../../etc/passwd")`. Not a known active exploit,
but the input flows from URL path segments into filesystem
operations, so it's worth landing first.

### Phase 2 — Retire `ReviewGateDecision`

- Change `projection::waiting_on` to take
  `gate: Option<&CommitGate>` instead of `Option<&ReviewGateDecision>`.
  Internal adjustments: `participants` field maps directly;
  `approvals`/`request_changes` decompose into
  `approvers`/`requesters`; `missing_approvals` → `missing`.
- Delete `commit_gate_to_review_decision`
  (`projection.rs:498-512`) and `ReviewGateDecision`
  (`review_state.rs:69-91`).
- Update `plan_gate_for` / `impl_gate_for` /
  `latest_reviewable_commit_gate_for` callers to return
  `Option<&CommitGate>` (zero-copy reference, since `Plan.commits`
  owns).
- Touchpoints: `mcp_response.rs:285, 443-472`, `ui_response.rs:272,
  448-474` — both `gate_value` functions adapt to the new field
  names. (This phase interacts with typed-wire-contracts; if that
  plan lands first, `gate_value` is already moved to a shared
  module. Either ordering works.)
- Tests: helpers in `projection.rs`/`wait.rs` adjust to construct
  `CommitGate` directly.

### Phase 3 — Shared test helpers module (depends on 1a)

- Create `src/test_helpers.rs` behind plain `#[cfg(test)]`. No
  feature flag — nothing outside tests needs these helpers.
- Contents:
  - `pub fn al(s: &str) -> AgentLabel` / `pk(s) -> PlanKey` /
    `cs(s) -> CommitSha` short-form constructors using the
    validated parsers from Phase 1a (panic on invalid — tests
    pass literals).
  - `pub fn gate(state, participants, approvers, requesters) -> CommitGate`
    that constructs via `CommitGate { state, participants,
    approvers, requesters, ..CommitGate::default() }`. No
    separate `CommitGateBuilder` type — struct-update syntax
    already gives us the "fill empty defaults" affordance.
    Requires `impl Default for CommitGate`.
  - `pub fn feedback_with(author, verdict, target_sha) -> Feedback`.
  - `pub fn touches_one(plan, kind) -> Vec<(PlanKey, PlanTouchKind)>`.
- Add `FeedbackPath::canonical_string(&self) -> String` in
  `disk_format.rs` (inverse of `parse_feedback_path`) and a
  `FeedbackPath::test_fixture(plan_key, target_sha, author) -> Self`
  constructor that goes through validation.
- Migrate `projection.rs`, `server/wait.rs`, `runtime.rs`,
  `disk_snapshot.rs`, and `tests/end_to_end.rs` test code to use
  the shared helpers. Delete the local duplicates.

### Phase 4 — `runtime.rs` dead-code removal + light cleanup

The "split runtime.rs into three modules" framing in the first draft
was premature optimization disguised as cleanliness — 890 lines is
big but cohesive, and there's no second caller demanding the
extraction. Scope this phase down to mechanical wins only:

- **Delete the `self_writes` ring and its two methods**
  (`runtime.rs:27, 243-264`). Dead code. Won't be revived without an
  explicit need.
- **Inline anything else dead-allow'd** that grep surfaces in
  `runtime.rs`.
- **If `cargo test`'s coverage on `runtime.rs` shows the same fn
  being tested in two different ways** (signal-routing tested via
  full Runtime + feedback-ingest tested via direct method call),
  that's a concrete reason to extract — and only then. Otherwise
  leave `runtime.rs` as one module.

Module extraction (`event_bus.rs`, `signal_router.rs`,
`feedback_ingest.rs`) is **explicitly deferred** until a concrete
reason emerges (a unit test that can't reach the seam, a second
caller, or a measured testability problem). 890 lines isn't
sufficient justification on its own.

### Phase 5 — Collapse parallel walkers + strip existing micro-opt

- Replace the four `latest_*` delegators
  (`projection.rs:351-372`) with `.last()` at the two or three
  callers. Just inline. **Don't introduce a
  `latest_commit_matching<F>` primitive** — three callers don't
  justify a higher-order function, and it would make the call-sites
  less direct.
- Add an accessor: `Plan::commit_kind_of(&CommitSha) ->
  Option<CommitKind>` (or a free fn that takes `(&Plan, &CommitSha,
  &Attribution)`), so callers stop having to assemble the four-arg
  call to `commit_kind_for`. The accessor still computes on the fly.
- **Strip the existing "Hot path" micro-optimization in
  `commit_kind_for`** (`projection.rs:617-621`). The
  short-circuit-before-building-a-set branch saves microseconds
  against a classifier that runs O(commits) per response. Collapse
  to one expression:
  ```rust
  let distinct_plans_touched = touches
      .map(|ts| ts.iter().map(|(k, _)| k)
                  .collect::<BTreeSet<_>>().len())
      .unwrap_or(0);
  ```
- **Explicitly NOT proposed**: memoizing `commit_kind_for` results
  into a `BTreeMap` on `RepoState`. The recomputation is cheap; the
  cache would be premature optimization.

### Phase 6 — Delete `runtime_snapshot.rs`

`RepoSnapshot` / `PlanSnapshot` / `PlanSnapshotBundle` are parallel
domain shapes that clone the whole live state under the mutex, then
synthesize fake `RepoState`s for response builders via
`to_repo_state()`. The right shape is: response builders take the
live `&RepoState` directly while holding the runtime read-lock, or
take narrow query DTOs computed on the fly.

- Move every consumer of `RepoSnapshot::from_state` /
  `PlanSnapshotBundle::from_state_for` to take `&RepoState` (or a
  borrowed `&Plan`) under the runtime read-lock for the duration
  of the response build. If holding the lock across a response
  is unacceptable, define a narrow `PlanQueryResult` typed DTO
  (only the fields needed by that builder) and copy *just those*.
- Delete `RepoSnapshot::to_repo_state`,
  `PlanSnapshotBundle::to_repo_state`, and any other
  compat-bridge methods. No long-term `to_repo_state()` shim.
- Delete `runtime_snapshot.rs` once all callers migrate.

### Phase 7 — Unified `PlanSelector` resolver

Today the "which plan?" question resolves differently in MCP, HTTP,
and tests:
- MCP `wait_for_work` infers from cwd (`server/mcp.rs:48-88`).
- HTTP `/api/wait_for_work` and core `server/wait.rs:115-121`
  reject empty `plan_id`.
- Tests construct their own.

Work:
- Define a typed `PlanSelector` enum in a shared module (probably
  `lifecycle.rs` or a new `plan_selector.rs`):
  ```rust
  pub enum PlanSelector {
      Explicit(PlanId),
      InferFromCwd { cwd: PathBuf },
      InferFromRepoFilter(RepoBasename),
  }
  ```
- Define one resolver:
  `pub fn resolve(selector: &PlanSelector, trinity: &Trinity)
   -> Result<PlanId, PlanResolutionError>` where the error variants
  cover `NoActivePlans`, `Ambiguous { candidates }`, and
  `RepoNotWatched`.
- Replace every entry point's bespoke resolution logic with one
  call to the resolver:
  - `server/mcp.rs` `resolve_plan_id`
  - `server/wait.rs` plan_id check
  - HTTP `/api/wait_for_work` handler
  - `server/mcp.rs` `get_context`
- If HTTP truly cannot resolve from cwd (the request has no notion
  of caller cwd), encode that in the type: HTTP handlers construct
  `PlanSelector::Explicit(...)` only, and the type-checker
  prevents accidentally passing a cwd-inference variant. The split
  becomes intentional rather than accidental.

### Phase 8 — Purge fake `Option<T>`

Audit every `Option<T>` in `src/` and `frontend/src/` and ask:
*what real absence does this model?* If the answer is "the caller
knows by checking another field" or "this variant doesn't have
this", that's a fake `Option`, and the model should use a typed
variant instead.

Concrete starting points:
- `server/wait.rs:56-90` + `175-188`: split `WaitResponse::Work`
  into per-work-kind variants. Each variant carries exactly the
  fields it needs; no `target_sha: Option<String>` carrying
  variant invariants implicitly. `WorkItem` similarly becomes a
  typed enum or is deleted in favour of constructing the response
  variant directly at each call-site.
- `frontend/src/api.rs:69-79` and timeline event wire structs:
  `phase: Option<String>` / `plan_touch: Option<String>` become
  per-variant required fields on a tagged enum. Overlaps with
  Phase 9 (`TimelinePhase` deletion); coordinate.
- `frontend/src/store.rs:38-46`: live event struct splits into a
  tagged enum (`LiveEvent::Plan { plan_id: PlanId, slug: PlanKey,
  state: PlanState, .. } | LiveEvent::Repo { .. }`). Daemon-side
  emission (`runtime.rs` / `event_bus`) updates to match.
- Audit pass: grep for `Option<CommitSha>`, `Option<PlanKey>`,
  `Option<PlanId>`, `Option<String>` across `src/` and
  `frontend/src/`. Every remaining `Option<...>` after this phase
  should answer "the absent case is a genuinely different
  semantic state" — `head` / `parent` / `previous_sha` style.
  Variant-tag Options get deleted in favour of typed enums.

### Phase 9 — Delete `Phase` and `TimelinePhase`

**Ordering with `wfw-alias §12`**: `wfw-alias §12` deletes
`PlanState::Done`. This phase deletes `Phase` and `TimelinePhase`.
The two interact at the wire layer (both contribute to the
"is this done?" question on the response). Recommended ordering:

1. `wfw-alias §12` lands first, deleting `PlanState::Done` and
   reshaping the wire so `state: "active" | "done"` becomes a
   derived predicate.
2. This phase lands second, deleting `Phase` and `TimelinePhase`.

If this phase lands first, leave `Phase::Done` in place until §12
can take it; the wire string for `phase` remains
`"planning"`/`"implementing"`/`"done"` until both phases are
complete.

Work:
- Introduce `fn current_posture(&self) -> Posture` on `Plan` (or a
  free fn in `projection`) that returns one of
  `{Planning, Implementing}` (plus `Done` if §12 hasn't landed)
  based on the latest reviewable commit's `CommitKind`. PlanOnly |
  Mixed → Planning; CodeOnly → Implementing.
- Update `TimelineEvent::Review.phase` field: either fold the
  same `Posture` value in, or (preferred) delete the field
  entirely and let the consumer derive it from the linked commit's
  kind. The frontend timeline rendering needs to follow whichever
  choice this phase makes — coordinate with Phase 10.
- Delete `Phase`, `phase_for`, `phase` from `repo_state.rs` /
  `projection.rs`; delete `TimelinePhase` from `repo_state.rs:292`.
- Update the 12 call-sites in `mcp_response.rs`/`ui_response.rs`
  to read `posture` instead.

### Phase 10 — Frontend feedback shape unification (depends on Phase 9)

The dependency: `TimelineEvent` row rendering touches the same
component code as `FeedbackEntry`. Picking `CommitFeedback` as
canonical means `target_sha` (which `FeedbackEntry` has and
`CommitFeedback` doesn't) needs to be sourced from the commit's
context. Doing this *after* Phase 9 means the timeline rendering
has already been touched once.

- Pick `CommitFeedback` (`frontend/src/api.rs:101-109`) as canonical.
- Delete `FeedbackEntry` (`api.rs:129-137`) and
  `commit_fb_to_feedback_entry` bridge in
  `frontend/src/components/session_detail.rs`.
- Update consumers (`feedback_card.rs`, `timeline.rs`) to take
  `CommitFeedback` directly. Add `target_sha` to `CommitFeedback`
  if any consumer truly needs it on a flat list (timeline rows
  almost certainly do); otherwise pass it as a sibling arg.
- Remove the blanket `#[allow(dead_code)]` on
  `frontend/src/api.rs:100,112,122,128`; replace with
  `#[serde(deny_unknown_fields)]` per struct and fix any genuinely
  unused fields surfaced by `cargo check`.

### Phase 11 — `wait_for_work`: remove timeout cap, raise default to 30m

`server/wait.rs:29-30` defines
`const DEFAULT_TIMEOUT_SECS: u64 = 60;` and
`const MAX_TIMEOUT_SECS: u64 = 300;`. The request handler
`.clamp(1, MAX_TIMEOUT_SECS)`'s the caller's `timeout_secs`. The
MCP shim repeats the same 300s limit at
`src/mcp_shim/mod.rs:317-322`, and the tool schema text at
`src/tools.rs:119` still documents `timeout_secs (optional,
1–300, default 60)`. `wait_for_work` is the blocking
coordination primitive — capping it at 5 minutes forces agents
into pointless polling loops, and a 60-second default means
agents fall out and re-call far more often than necessary.

Work:
- Change `DEFAULT_TIMEOUT_SECS` from `60` to `1800` (30 minutes).
- Delete `MAX_TIMEOUT_SECS` from `server/wait.rs`.
- Replace `.clamp(1, MAX_TIMEOUT_SECS)` with `.max(1)`.
- Update the MCP shim's per-request transport-timeout derivation
  (`mcp_shim/mod.rs:317-322`) to use the requested timeout
  directly (with the same +30s transport grace) and update its
  default-fill from `unwrap_or(60)` to `unwrap_or(1800)`.
- Update the tool description in `src/tools.rs` to drop the
  `1–300` text and say "positive seconds, default 1800
  (30 minutes)".
- Update the daemon-side schema in
  `src/server/wait.rs` (`maximum: 300` if present in the
  generated JSON Schema; bump default).
- Add at least one test that passes `timeout_secs` above 300 and
  asserts the daemon waits for the full duration rather than
  rewriting it to 300.

Note: client-side transport stacks (reqwest, hyper, etc.) may
have their own timeouts. Those are outside this plan; Trinity
itself should not impose a semantic cap.

### Phase 12 — `tests/end_to_end.rs` split + injectable test home

**Mandatory**, not optional. A 1707-line e2e file plus a global
`$HOME` mutex *is* architecture debt — exactly what this plan is
paying down. Lands last because it's the largest churn footprint,
but it is part of the acceptance criteria.

- Refactor the test harness so `$HOME` is injected via a struct
  field rather than mutated via `std::env::set_var`. The
  `HOME_LOCK` global mutex goes away with this.
- Split by surface: `tests/wait_for_work.rs`, `tests/get_context.rs`,
  `tests/feedback_ingest.rs`, `tests/multi_repo.rs`.
- Move shared setup (`init_repo`, `write_file`, `commit`) to
  `tests/common/mod.rs`.

## Risks

- **Phase 1 will catch real bugs / fail the build mid-migration.**
  The unchecked constructors (`new`, `From<&str>`, `From<String>`)
  are used at 50+ call-sites. A single-commit migration would
  break `cargo build` at the deletion step. That's why Phase 1 is
  split into 1a (add validated parsers, keep unchecked), 1b
  (migrate call-sites), 1c (delete unchecked, switch
  `Deserialize`). 1a and 1c are atomic; 1b is the largest patch in
  the sweep and benefits from focused review.
- **Phase 2 interacts with typed-wire-contracts.** If that plan
  lands first, much of Phase 2's `gate_value` shape adjustment is
  already done — Phase 2 then reduces to deleting
  `ReviewGateDecision` and changing `waiting_on`'s signature. If
  this plan lands first, typed-wire-contracts inherits
  `CommitGate`-typed contracts directly, which is the better
  shape anyway. Either ordering is safe.
- **Phase 4 is now small.** Dead-code removal only, no module
  split. Run the test suite once; no per-step coverage needed.
- **Phase 6 (`runtime_snapshot.rs` deletion) widens the runtime
  read-lock.** Today response builders take a snapshot under the
  lock, release it, then build JSON. Switching to "build JSON
  while holding the read-lock" lengthens the critical section.
  If profiling later shows lock contention, revisit with a typed
  query DTO that copies only the fields needed (not the world).
  Don't reintroduce `RepoSnapshot`-style mirrors.
- **Phase 8 (fake `Option<T>` purge) widens the wire-shape change
  surface.** Splitting `WaitResponse::Work` into per-kind variants
  is a breaking JSON change. Coordinate with downstream tooling
  (the MCP shim, the frontend, any external script consuming
  `wait_for_work`) — bump the response schema together with the
  variant split.
- **Phase 9 ordering vs `wfw-alias §12`.** `Phase::Done` and
  `PlanState::Done` are two encodings of the same truth.
  Recommended sequence: §12 deletes `PlanState::Done` first, then
  this phase deletes `Phase` and `TimelinePhase`. If they land in
  the other order, leave `Phase::Done` until §12 takes it.
- **Phase 9 wire field stability.** The internal type goes away
  but the wire field stays through the transition; reviewers
  should verify the call-site migration is exhaustive and that
  the wire string output is byte-identical pre- and post-deletion.

## Acceptance Criteria

The plan is not landed until all of the following hold against the
post-sweep tree. Reviewers should grep for each.

1. **No new derived caches on `RepoState`.** Specifically:
   - No `commit_kinds: BTreeMap<...>` field or equivalent.
   - No "kind cache", "phase cache", or memoized-projection field
     added during the sweep.
2. **Dead bridges deleted.**
   - `ReviewGateDecision` (type + `commit_gate_to_review_decision`)
     removed.
   - `RepoSnapshot::to_repo_state`,
     `PlanSnapshotBundle::to_repo_state`, and `runtime_snapshot.rs`
     entirely removed.
   - `TimelinePhase` enum removed.
   - `Phase` enum removed (modulo §12 coordination — see Phase 9).
3. **No `#[allow(dead_code)]` added to preserve old shapes.** The
   sweep removes dead code; it doesn't tag it as kept-for-now.
4. **No new compatibility shims** unless this plan names the
   deletion phase that retires them.
5. **No new feature flags** introduced for the test-helpers module
   or any other component touched.
6. **Grep checks pass** in every touched module: searches for
   `Hot path`, `legacy`, `bridge`, and `phase` either return no
   hits, or every remaining hit has a one-line comment justifying
   why it stays.
7. **No fake `Option<T>`.** Grep for `Option<` in every touched
   module. Every remaining occurrence must answer "what real
   absence does this model?" — `head` / `parent` / `previous_sha`
   semantics are fine; variant-tag Options
   (`WaitResponse::Work { target_sha: Option<...> }`,
   `LiveEvent { plan_id: Option<...> }`, etc.) are not.
   `WaitResponse` has been split into per-work-kind variants;
   live-event struct has been split into a tagged enum.
8. **Strong-typed constructors validate.** `cargo build` rejects
   `CommitSha::new("hello")`, `PlanKey::new("../etc/passwd")`,
   `AgentLabel::new("")`. JSON requests with malformed IDs fail at
   deserialize.
9. **One resolver path.** `PlanSelector` is the only type that
   answers "which plan?"; `server/mcp.rs`, `server/wait.rs`, and
   HTTP wait-for-work all consume the same resolver.
10. **`runtime.rs` `self_writes` ring deleted** along with its two
    methods.
11. **`wait_for_work` timeout cap removed.** `MAX_TIMEOUT_SECS`
    deleted from `server/wait.rs`; no `.clamp(1, 300)` or
    `.clamp(1, MAX_TIMEOUT_SECS)` remains for `wait_for_work` in
    daemon or shim. Tool description says "positive seconds,
    default 1800 (30 minutes)". Default raised to 1800. A test
    asserts a timeout above 300 is accepted without rewriting.
12. **`tests/end_to_end.rs` split into themed files** under
    `tests/`, with `HOME_LOCK` mutex removed in favour of an
    injected test-home struct field.

## Out of Scope (Explicit)

- Storage migrations (no on-disk format changes).
- Multi-repo concurrency model changes.
- Replacing `notify` with another watcher.
- MCP protocol-level changes.
- Anything touching `git_io.rs` plumbing (it's IO-heavy but stable).
