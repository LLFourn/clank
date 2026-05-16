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
each time a response is built. The kind is a deterministic function of
data that already lives in `RepoState` (`plan_touches`, `attribution`)
and never changes for a given (plan, sha) pair after disk-snapshot
ingest. `RepoState` should memoize `commit_kinds:
BTreeMap<CommitSha, BTreeMap<PlanKey, CommitKind>>` once at ingest
time. Q1 (invariant available, not leveraged) and Q2 (repeated
computation).

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

**`Trinity` god-object dual-index is fine but undocumented.**
`repo_state.rs:16-23` holds `repos: BTreeMap<RepoRoot, RepoState>`
plus a `repo_basenames: BTreeMap<RepoBasename, RepoRoot>` reverse
index for basename-collision detection. This isn't a layering
violation — it's the canonical store — but the invariant ("for every
key in `repos` there's exactly one entry in `repo_basenames` mapping
back to it") lives only in commit history. Worth a debug-assert on
mutation entrypoints, or a constructor that wraps the pair.

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

Reasonable extraction targets: an `event_bus` module (out of #3), a
`signal_router` module (out of #2), a `feedback_ingest` module (out
of #5). Splitting #1 from `Runtime` is harder — the mutex is the
backbone — but the others can move with mechanical refactors.

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
- Shared `test_helpers` module; eliminate the duplicated `gate` /
  `agents` / `feedback_with` / path-format helpers.
- `runtime.rs` under 500 lines, with `event_bus` / `signal_router` /
  `feedback_ingest` extracted as their own modules.
- `commit_kinds` memoized on `RepoState`, computed once at
  disk-snapshot ingest, read everywhere else.
- `Phase` and `TimelinePhase` enums deleted (folded into
  `CommitKind`-derived posture); coordinated with `wfw-alias §12`
  for `PlanState::Done`.
- `self_writes` ring deleted or revived intentionally.
- Frontend feedback shape unified — pick `CommitFeedback`, delete
  `FeedbackEntry` and the bridge; remove blanket `#[allow(dead_code)]`
  on wire structs.
- HTTP handlers stop bypassing `Runtime` for state-mutating IO.
  (The `api_move_to_done` endpoint goes away with `wfw-alias §12`;
  any other state-mutating endpoints route through `Runtime`.)
- Test harness injects `$HOME` instead of mutating it via a global
  mutex.

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

Phases form a recommended order with explicit dependencies. The
ordering is: **Phase 1 (typed IDs) is independent and should land
first**; **Phase 2 (retire `ReviewGateDecision`) is independent of
Phase 1** and can run in parallel; **Phase 3 (test helpers) depends
on Phase 1's validated parsers**; **Phase 4 (runtime extraction)
depends on Phase 2** (the extracted `feedback_ingest` module reads
gate types directly, easier with `ReviewGateDecision` already
deleted); **Phase 5 (memoization + walker collapse) is independent
of Phase 1-2**; **Phase 6 (`Phase` enum deletion) interacts with
`wfw-alias §12` and the ordering is specified below**; **Phase 7
(frontend feedback unification) depends on Phase 6**; **Phase 8
(test-file split) is last, optional**.

### Phase 1 — Strong-typed ID validation (three sub-steps)

This is bigger than a single commit because the unchecked `new` /
`From<&str>` / `From<String>` constructors are used at 50+ call
sites; the call-site sweep is the bulk of the work. Split:

**1a — Add validated parsers, keep escape hatch.**
- For each type in `lifecycle.rs`, add
  `pub fn parse(s: &str) -> Result<Self, IdError>` with the
  appropriate validation:
  - `CommitSha`: 4–40 lowercase hex chars.
  - `PlanKey`: non-empty, no `/`, no `..`, no leading dot.
  - `RepoBasename`: non-empty, no `/`.
  - `AgentLabel`: non-empty, no `/`, no leading dot.
  - `ContentHash`: existing format (verify in code).
- Add `pub(crate) fn from_validated(s: String) -> Self` as the
  internal-use unchecked path, documented "only call after
  upstream validation (e.g. disk-snapshot path parsing has
  already accepted this)".
- Keep `new` / `From<&str>` / `From<String>` working temporarily.

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
  impls. The macro now produces only `parse` and the internal
  `from_validated`.
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

- Create `src/test_helpers.rs` behind `#[cfg(any(test, feature = "test-helpers"))]`.
  Contents:
  - `pub fn al(s: &str) -> AgentLabel` / `pk(s) -> PlanKey` /
    `cs(s) -> CommitSha` short-form constructors using the
    validated parsers from Phase 1a (panic on invalid — tests
    pass literals).
  - `pub fn gate(state, participants, approvers, requesters) -> CommitGate`
    with sensible empty defaults via a builder
    (`CommitGateBuilder::default().approvers(...).build()`).
  - `pub fn feedback_with(author, verdict, target_sha) -> Feedback`.
  - `pub fn touches_one(plan, kind) -> Vec<(PlanKey, PlanTouchKind)>`.
- Add `FeedbackPath::canonical_string(&self) -> String` in
  `disk_format.rs` (inverse of `parse_feedback_path`) and a
  `FeedbackPath::test_fixture(plan_key, target_sha, author) -> Self`
  constructor that goes through validation.
- Migrate `projection.rs`, `server/wait.rs`, `runtime.rs`,
  `disk_snapshot.rs`, and `tests/end_to_end.rs` test code to use
  the shared helpers. Delete the local duplicates.

### Phase 4 — `runtime.rs` extraction (depends on Phase 2)

The dependency: extracted `feedback_ingest` and `signal_router`
modules will touch `CommitGate` and gate-projection types directly.
Doing this *after* Phase 2 means the new modules don't need to
import or adapt to the dead `ReviewGateDecision` bridge.

- Extract `src/event_bus.rs` owning the broadcast channel,
  `subscribe_events`, `push_event`, `LiveEvent` ring buffer.
  `Runtime` holds `event_bus: EventBus`.
- Extract `src/feedback_ingest.rs` owning `upsert_feedback`,
  `remove_feedback`, `extract_feedback`, `refresh_commits_for`.
  Takes `&mut RepoState` and writes through it; no lock awareness.
- Extract `src/signal_router.rs` owning `handle_signal` and the
  signal-kind dispatch. Takes `(Arc<Mutex<Trinity>>, &EventBus,
  &FeedbackIngest)` or similar.
- Delete the `self_writes` ring and its two methods. If it's needed
  later, reintroduce intentionally.
- `Runtime` ends up under ~400 lines and owns: mutex + wiring of
  the three extracted services.
- Risk: this is the largest behaviour-preserving refactor in the
  sweep. Run `cargo test -p trinity --tests` and
  `tests/end_to_end.rs` after each module extraction, not at the
  end.

### Phase 5 — Memoize `commit_kinds`; collapse parallel walkers

- Add `commit_kinds: BTreeMap<CommitSha, BTreeMap<PlanKey, CommitKind>>`
  to `RepoState`; populate it once in `disk_snapshot::derive_state`
  using the existing pure `commit_kind_for`. Delete the per-call
  recomputation in `mcp_response.rs:310`, `ui_response.rs:302`,
  `projection.rs:141/393/415/700`.
- Replace the four `latest_*` delegators
  (`projection.rs:351-372`) with `.last()` calls at the two or
  three callers. Equivalently, introduce
  `fn latest_commit_matching<F: Fn(CommitKind) -> bool>(&self, pred: F)`
  as the single primitive.
- Delete `commit_kind_for` from the response builders' import list
  once `RepoState.commit_kinds` is the canonical source.

### Phase 6 — Delete `Phase` and `TimelinePhase`

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
  choice this phase makes — coordinate with Phase 7.
- Delete `Phase`, `phase_for`, `phase` from `repo_state.rs` /
  `projection.rs`; delete `TimelinePhase` from `repo_state.rs:292`.
- Update the 12 call-sites in `mcp_response.rs`/`ui_response.rs`
  to read `posture` instead.

### Phase 7 — Frontend feedback shape unification (depends on Phase 6)

The dependency: `TimelineEvent` row rendering touches the same
component code as `FeedbackEntry`. Picking `CommitFeedback` as
canonical means `target_sha` (which `FeedbackEntry` has and
`CommitFeedback` doesn't) needs to be sourced from the commit's
context. Doing this *after* Phase 6 means the timeline rendering
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

### Phase 8 — `tests/end_to_end.rs` split + injectable test home (optional)

- Refactor the test harness so `$HOME` is injected via a struct
  field rather than mutated via `std::env::set_var`. The
  `HOME_LOCK` global mutex goes away with this.
- Split by surface: `tests/wait_for_work.rs`, `tests/get_context.rs`,
  `tests/feedback_ingest.rs`, `tests/multi_repo.rs`.
- Move shared setup (`init_repo`, `write_file`, `commit`) to
  `tests/common/mod.rs`.
- Keep this phase last; it's pure code hygiene with no functional
  win and a large churn footprint. The `HOME_LOCK` design smell is
  the only structural win.

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
- **Phase 4 (runtime extraction) is large.** `runtime.rs` is the
  most-tested module; the extraction is mechanical (no behaviour
  change) but the test surface is large. Run `cargo test -p
  trinity --tests` plus `tests/end_to_end.rs` after each module
  split, not at the end. Plan for a focused review pass.
- **Phase 6 ordering vs `wfw-alias §12`.** `Phase::Done` and
  `PlanState::Done` are two encodings of the same truth.
  Recommended sequence: §12 deletes `PlanState::Done` first, then
  this phase deletes `Phase` and `TimelinePhase`. If they land in
  the other order, leave `Phase::Done` until §12 takes it.
- **Phase 6 wire field stability.** The internal type goes away
  but the wire field stays through the transition; reviewers
  should verify the call-site migration is exhaustive and that
  the wire string output is byte-identical pre- and post-deletion.

## Out of Scope (Explicit)

- Storage migrations (no on-disk format changes).
- Multi-repo concurrency model changes.
- Replacing `notify` with another watcher.
- MCP protocol-level changes.
- Anything touching `git_io.rs` plumbing (it's IO-heavy but stable).
