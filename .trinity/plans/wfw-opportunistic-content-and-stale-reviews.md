# wfw-opportunistic-content-and-stale-reviews

Two `wait_for_work`-only additions to remove a recurring agent
drift class plus prevent lost reviews:

1. **Opportunistic body inlines** on the variants that point an
   agent at a file the agent didn't author. Bodies arrive on
   first encounter (per agent, by content hash) and are omitted
   on subsequent polls.
2. **Stale-review sidecar** (master-only) that one-shot-delivers
   reviews against superseded commits so no review is silently
   lost when the master rebases past it.

The variants reshape on both surfaces (they share
`WorkPayload`), but inline `content` populating and the
`stale_reviews` sidecar are WFW-only. `work_context` returns
the same metadata shape with `content: None` everywhere and no
sidecar.

## Why

### The agent-drift problem

Today's reviewer flow:

1. WFW returns `{kind: "write_feedback", path, target_sha}`.
2. Reviewer agent runs `Read` on the plan file to know what
   they're reviewing.
3. Reviewer agent paraphrases the plan in its head before writing
   the verdict.
4. Verdict drifts from what the plan actually said.

Same pattern hit master in `AddressChanges`: WFW hands a path,
agent `Read`s the RC body, agent paraphrases. Path/body drift
inserts verdict errors. The user has flagged this multiple times
("I have to `cat` the file to verify").

Fix: when WFW hands the agent a path, also hand them the body in
the same response. No separate I/O step to drift on. After the
first delivery, the agent has the content and the body is omitted
from subsequent polls (with a hash check so file mutations
naturally re-send).

### The lost-review problem

A reviewer writes RC against commit `aaa`. Master responds by
rebasing to `bbb` (which may or may not address the concern).
The original RC on `aaa` is no longer "current" — the current
target is `bbb`. AddressChanges fires only for `bbb`'s gate.
The RC on `aaa` is no longer surfaced anywhere by WFW.

Result: master never reads codex's concern. Codex's RC was
silently dropped from the agent loop.

Fix: a master-only sidecar that delivers stale reviews exactly
once. Once master has been shown the stale review, it's marked
seen and never re-surfaces.

## What

### Wire shape (the only place this plan changes the wire)

```rust
pub enum ExpectedAction {
    WriteFeedback {
        path: String,            // where reviewer writes
        target_sha: String,
        plan_file: PlanFile,     // what reviewer reads
    },
    AddressChanges {
        target_sha: String,
        plan_path: Option<String>,
        reviews: Vec<CurrentReview>,  // share variant's target_sha
    },
    CommitPlanRevision { plan_path: String },
    StartImplementation { previous_commit: String, plan_path: String },
    SessionFinished,
}

pub struct PlanFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

pub struct CurrentReview {
    pub path: String,
    pub author: AgentLabel,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

pub struct StaleReview {
    pub path: String,
    pub author: AgentLabel,
    pub verdict: Verdict,
    pub target_sha: String,      // the superseded commit
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>, // None iff oversized; entry still
                                 // delivered (path + verdict)
}

pub struct WaitWorkPayload {
    #[serde(flatten)]
    pub work: WorkPayload,
    /// Master-only one-shot review delivery for superseded
    /// targets. Empty on reviewer variants, on subsequent polls
    /// after each entry's been emitted, and after daemon restart
    /// clears state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_reviews: Vec<StaleReview>,
}

#[serde(untagged)]
pub enum WaitForWorkResponse {
    Work(WaitWorkPayload),
    Timeout(WaitTimeout),
}
```

### What this changes from today

The current `WriteFeedback` and `AddressChanges` variants (just
landed in `share-work-payload-across-surfaces`):

```rust
// before
WriteFeedback { path, target_sha }
AddressChanges { target_sha, rc_paths: Vec<String>, plan_path: Option<String> }
```

become:

```rust
// after
WriteFeedback { path, target_sha, plan_file: PlanFile }
AddressChanges { target_sha, plan_path: Option<String>, reviews: Vec<CurrentReview> }
```

`rc_paths: Vec<String>` → `reviews: Vec<CurrentReview>`. Each
review now carries `author` and `verdict` along with the path,
plus opportunistic `content`. The variant's `target_sha` is
inherited by every review (current reviews don't repeat it).

`WaitWorkPayload` wraps `WorkPayload` for the WFW happy path and
adds the master-only `stale_reviews` sidecar.

### Authorship rule (which content fields populate)

> Don't send back what the calling agent wrote.

- Reviewer in `WriteFeedback`: master wrote the plan →
  `plan_file.content` populated opportunistically.
- Master in `AddressChanges`: reviewers wrote the reviews →
  `reviews[*].content` populated opportunistically. Plan body
  NOT inlined (master wrote it).
- Master in `CommitPlanRevision`: plan is dirty in master's
  worktree, master will commit. Nothing inlined.
- Master in `StartImplementation`: plan body is on disk for
  master to re-read on demand. Nothing inlined.
- Stale reviews: master reads, reviewers wrote. Content
  inlined one-shot when the body fits the cap; otherwise the
  entry still delivers (`content: None`) and gets marked seen
  on the path + metadata reaching the agent.

### Caches (in-memory on Runtime)

```rust
// In Trinity:
opportunistic_bodies: BTreeMap<(RepoBasename, AgentLabel, String), ContentHash>,
seen_stale_reviews: BTreeSet<(RepoBasename, AgentLabel, String)>,
```

- `opportunistic_bodies`: keyed by `(repo basename, agent,
  repo-relative path)`, value is the content hash last sent to
  that agent. Used for `plan_file.content` and
  `current_review.content`. Hash-keyed: reviewer rewrites and
  plan revisions naturally re-send.
- `seen_stale_reviews`: keyed by `(repo basename, agent,
  repo-relative path)`. Strictly one-shot — once a stale review
  has been delivered to an agent, it never re-surfaces in
  `stale_reviews`.

Repo basename is part of the key because Trinity watches
multiple repos and the same repo-relative path
(`.trinity/plans/foo.md`, `.trinity/feedback/foo/<sha>/codex.md`)
exists in many of them. Without repo identity, `foo` plans
across repos would share cache state.

Both cleared on daemon restart. No persistence.

### Population (WFW handler, post-projection)

After `responses::build_work_payload` returns, the WFW handler
in `src/server/wait.rs` post-processes:

1. **Opportunistic content fill**:
   - If the action is `WriteFeedback`: read `plan_file.path`,
     hash it, check `opportunistic_bodies[(agent, path)]`. If
     absent or hash mismatch → fill `plan_file.content`, update
     cache.
   - If the action is `AddressChanges`: per review entry, same
     check on its `path`. Fill `content` on miss.

2. **Stale reviews collection** (master role only, regardless
   of which variant fired):
   - Walk the plan's timeline for reviews against
     non-current-target commits.
   - For each, check `seen_stale_reviews[(repo, agent, path)]`.
     If absent → read content, append as `StaleReview`, mark
     seen.

### Wakeup rule for stale reviews

A naïve master-role WFW only returns when
`waiting_on.role == Master` (or `SessionFinished`). That means
in the common pattern "master commits → reviewer reviews → if
RC, master addresses; if approve, master implements next", the
master is asleep most of the time when stale reviews exist.
The sidecar would arrive only on the next master-side wakeup,
which is fine for "didn't lose data" but bad for "master sees
it before doing more work."

Rule: **master WFW wakes on any state change that produces a
new unseen stale review for this `(repo, agent)`.** Concretely:

- The existing role-gate exception for `SessionFinished` keeps
  working.
- Add a second gate exception: if the master-role caller has
  any unseen stale reviews for this `(repo, agent)` AND the
  current `waiting_on.role` is `Reviewers` (master would
  normally stay asleep), still emit a response. The action
  variant emitted in that case is whatever the projection
  produces (likely the previous "your work is X" — there's no
  new work; the master is being delivered the sidecar). This
  is the only case where master WFW returns when
  `waiting_on.role == Reviewers`.

This guarantees: any review filed against ANY commit is
delivered to the master in their next WFW response, regardless
of whether the current gate is waiting on them. After
delivery, the review is marked seen and the master goes back
to sleep on the normal wakeup rule.

The wakeup itself triggers off the same broadcast that wakes
the WFW loop today (commit lands → repo rebuilt → broadcast
event). The check happens at `compute_match` time.

`work_context_response_from_snapshot` skips both steps. Content
fields stay `None`; no sidecar.

### Body cap

Inline body is capped at **64 KB** per file. Beyond the cap, the
handler emits the entry with `content: None` and the agent falls
back to `Read`. The cap exists for protocol hygiene; verdict
files are usually hundreds of bytes and plan files are typically
a few KB.

Same cap applies to `stale_reviews` content. To make this work
without silently dropping oversized entries, `StaleReview.content`
is also `Option<String>` (not `String`-always-present): an
oversized stale review emits `{path, author, verdict,
target_sha, content: None}` and gets marked seen anyway —
master has the path + verdict so they can `Read` if they care.
"Marked seen" means "the metadata reached the agent," not
"the body was delivered." This avoids re-emitting the same
oversized entry on every subsequent poll.

## Files touched (sketch)

- `crates/trinity-core/src/api.rs` — new structs (`PlanFile`,
  `CurrentReview`, `StaleReview`, `WaitWorkPayload`). Reshape
  `WriteFeedback` and `AddressChanges` variants. Rewire
  `WaitForWorkResponse::Work` to use `WaitWorkPayload`.
- `src/responses.rs` — `build_work_payload` constructs the new
  variant shapes (paths-and-metadata only; content stays
  `None`). `WorkPayloadInputs` updates if needed.
- `src/server/wait.rs` — `compute_match` calls `build_work_payload`
  as today, then a new post-processing step
  (`enrich_with_bodies` + `collect_stale_reviews`) populates
  content and the sidecar. Caches live on the runtime; methods
  there return the necessary lookup / mark-seen handles.
- `src/runtime.rs` — add the two cache maps to `Trinity` and
  accessor methods (`mark_body_sent`, `mark_stale_seen`,
  `query_body_cache`, `query_stale_seen`).
- `tests/end_to_end.rs` — new integration tests:
  reviewer-first-poll-has-plan-content, reviewer-subsequent-
  omits-it, master-addresschanges-includes-rc-content-first-
  poll, master-omits-on-subsequent, master-with-superseded-rc
  gets-stale-review-sidecar-once, hash-mismatch-re-sends.
- `src/server/wait.rs::integration_tests` — cross-surface test
  still asserts `wfw_response.work == work_context.work` (inner
  WorkPayload comparison); the `WaitWorkPayload` wrapper's
  `stale_reviews` sidecar is wait-only.
- `crates/trinity-core/tests/wire_snapshots.rs` — fixtures for
  the new shapes; existing
  `wait_for_work_response_work` regenerated with the wrapper
  + extension fields.

## Rules

- Bodies appear ONLY on `wait_for_work`. `work_context` ignores
  the content fields (always None / skipped on the wire).
- The authorship rule is bedrock: never re-send a file content
  to the agent that authored it.
- Two distinct caches with distinct semantics: hash-keyed for
  opportunistic content (naturally re-sends on change); path-
  keyed for stale reviews (strictly one-shot).
- `stale_reviews` is a master concern. Reviewer variants always
  see `stale_reviews: []`.
- Lock-boundary discipline: file reads and content hashing run
  OUTSIDE the runtime mutex. Only the cache-query and cache-mark
  operations take the lock, and each takes it briefly. Pattern
  mirrors the existing `resolve_plan_id` lock-then-disk-read
  shape from `mcp-context-surface-cleanup`: snapshot candidate
  metadata + cache decisions under the lock, drop the lock,
  read + hash files outside, briefly re-acquire to record
  what was sent. The hot WFW path must not block on disk I/O
  with the global mutex held.

## Testing

- Wire snapshot suite regenerated for the new variant shapes
  and the `WaitWorkPayload` wrapper.
- Round-trip tests for `PlanFile`, `CurrentReview`, `StaleReview`,
  and a `WaitWorkPayload` carrying populated and empty
  `stale_reviews`.
- Integration tests for both surfaces:
  - First-poll body delivery (reviewer + master).
  - Subsequent-poll body omission.
  - Hash-mismatch re-send.
  - Stale review delivered once, then suppressed.
  - `work_context` never carries content or sidecar.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo fmt -- --check`.
- `cd frontend && trunk build`.

## Acceptance criteria

- `WaitForWorkResponse::Work` is `WaitWorkPayload`, not
  `WorkPayload`. The wrapper carries `stale_reviews`.
- `WriteFeedback` carries `plan_file: PlanFile`.
- `AddressChanges` carries `reviews: Vec<CurrentReview>`
  (replaces `rc_paths`).
- `work_context` returns the same action metadata shape as
  WFW (the variants change on both surfaces) but never
  populates inline `content` and never carries the
  `stale_reviews` sidecar.
- WFW first-poll wire shows the relevant `content` field
  populated (reviewer's `plan_file.content`, master's
  `reviews[*].content`).
- WFW subsequent-poll wire omits `content` for already-sent
  bodies.
- WFW for master with a never-seen stale review emits the
  `stale_reviews` sidecar; the next poll omits it.
- Hash-mismatch (e.g. reviewer rewrites RC) re-sends the
  current content. Stale-review hash mismatch does NOT
  re-surface the entry (one-shot).
- Inline bodies above 64 KB are skipped (`content: None`); the
  agent reads via the `Read` tool. For stale reviews this still
  counts as "delivered" — path + verdict + target_sha reached
  the agent, only the body was omitted.

## Non-goals

- Touching `work_context` semantics. The variants change on
  both surfaces (shared `WorkPayload`), but the inline-content
  and `stale_reviews` behaviors are WFW-only.
- Persisting cache state across daemon restarts. Restart is the
  "force re-send everything" reset by design.
- New work-action variants for "address stale review."
  `stale_reviews` is informational delivery, not a separate
  routed work item.

## Trade-off honest record

The big call: WFW becomes stateful. Today it's a pure
projection of the fold state at a moment. After this plan,
WFW responses depend on prior calls (the cache remembers what
each agent has been told). Test fixtures get slightly more
involved; the response is no longer idempotent. Agent-loop
correctness wins — paraphrasing drift is a recurring failure
and per-agent first-send caching is the cheapest fix.

The 64 KB body cap: plan files in some repos may exceed it.
The fallback (`content: None`, agent `Read`s) preserves
correctness; the only loss is the drift-prevention benefit on
oversized files.
