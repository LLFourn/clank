# core-state-rewrite

## Summary

The trinity-core fold state has accumulated structural decay
that the previous plan (`core-model-invalid-states-unrepresentable`)
tried to fix in flight. Mid-stream it kept widening — bucket
enums → flat facts → active plans only → drop the commit
registry → drop CommitNode from state. None of that is a
"making invalid states unrepresentable" change anymore; it's
a rewrite of the fold's data model.

This plan does the rewrite cleanly, from scratch, with one
declarative center. After it lands:

- Fold + state types live in `crates/trinity-core/src/repo_state.rs`.
  Sans-io.
- `RepoState` carries only active workflow facts. Finished
  plans, the global commit registry, the global commit order
  list, and `plan_conflicts` are all gone — `git log` is the
  system of record for "what happened in this repo."
- The fold has no separate `FoldCarry`. `apply_commit` is a
  single-arg method.
- The cache stores the entire `RepoState`. Warm rebuilds load
  the cached state at an ancestor SHA and apply just the new
  commits — no re-walk from the root commit.
- A plan's stored data is its per-commit timeline of small
  per-(plan, commit) booleans. Body, intro SHA, last
  activity, lifecycle stage, participants — all derived from
  the timeline + `git_io` + feedback files.

The previous plan (`core-model-invalid-states-unrepresentable`)
is superseded. Its Phase 1 + Phase 2 commits stay (they
landed useful cleanup — deleted bucket enums, introduced the
classifier + Warning enum). What didn't land
(Phase 3, atomic fold swap) is what this plan now executes,
with the smaller and more honest scope.

## Hard Direction

- `RepoState` is `{ plans, finished_plans, ad_hoc,
  active_plan_hint, warnings }`. Five fields. The fold
  tracks NO per-plan approver state — the daemon (via
  `git_io`) computes whether a commit's tree freezes a
  plan when it builds the `CommitEvent`, and tells the
  fold via `event.newly_finished: BTreeSet<PlanKey>`.
- Attribution warnings are folded facts (`RepoWarning {
  sha, plan, warning }`), appended to `state.warnings`
  during `apply_commit` while the historical classifier
  context is correct. Projection filters by `(sha,
  plan)`; it never re-runs the classifier on old
  commits.
- `state.plans` holds only active plans. Non-frozen Delete
  REMOVES the plan from the map. Finalize MOVES the plan
  to `finished_plans` (recording `{ plan, finalized_at:
  CommitSha }`) and removes it from `plans`.
- `state.finished_plans: Vec<FinishedPlan>` is an ordered
  log of "plan X was finalized at SHA Y." That's all. The
  body at freeze, approver count, etc. are
  `git_io::show <sha>:...` queries when the UI needs them.
  PlanKey can repeat (a plan can be re-introduced and
  re-finalized).
- `PlanState` is `{ commits: Vec<PlanTimelineEvent> }`. No
  body, no body_hash, no plan_intro, no plan_intro_parent,
  no last_activity_ts, no archived_cycles, no
  latest_revision / latest_implementation, no
  participants_cumulative, no stage. All derived at
  projection time.
- `PlanTimelineEvent` is `{ sha, ts, touched_plan,
  touched_code }`. Four fields. The finalize fact lives
  in `state.finished_plans`, not on the event.
- `state.ad_hoc: Vec<AdHocEvent>`. Cleared on the next plan
  intro.
- `apply_commit(&mut self, event: &CommitEvent)`. One arg.
- The fold module is sans-io. No git, no filesystem, no
  async.
- `CommitNode` (the rich projection view) is built on
  demand by the projection layer from `PlanTimelineEvent`
  + `git_io` + feedback file scans. Never stored.
- Cache supports incremental fold via a cache file whose
  header records the SHA the state was saved at.
- No `FoldCarry`, no `BaseRepoState` / `LiveRepoState`
  wrapper distinction (live overlays are applied on top of
  the loaded `RepoState` by daemon code, not by a separate
  type).

## Core State

The complete inventory of state types lives in
`crates/trinity-core/src/repo_state.rs`. Every struct + enum
below has exactly one declaration; the rest of this plan
refers back here.

```rust
/// Top-level fold output. The cache stores this directly.
pub struct RepoState {
    /// Active plans only. A plan exits the map on finalize
    /// (moved to `finished_plans`) or on a non-frozen Delete
    /// (gone from state entirely).
    pub plans: BTreeMap<PlanKey, PlanState>,

    /// Ordered log of finalize events: "plan X was finalized
    /// at SHA Y, then plan Z at SHA W, ..." That's all. Body
    /// at freeze, approver count, etc. are recoverable via
    /// `git_io::show <sha>:.trinity/finished/<plan>/...` or
    /// `git show <sha>:.trinity/plans/<plan>.md`. PlanKey
    /// can repeat (re-intro + re-finalize).
    pub finished_plans: Vec<FinishedPlan>,

    /// Out-of-plan commits in fold order. Cleared on the
    /// next plan intro.
    pub ad_hoc: Vec<AdHocEvent>,

    /// The plan we're assumed to be working on right now —
    /// "active plan hint" — determined by the last commit
    /// that touched exclusively one plan (or had an
    /// explicit `[plan-x]` prefix). The classifier uses
    /// this as the fallback `plan_attribution` for the
    /// next commit when the title is bare and the diff
    /// doesn't disambiguate. Not authoritative: master can
    /// always override with an explicit `[plan-x]` or
    /// `[misc]` prefix.
    pub active_plan_hint: Option<PlanKey>,

    /// Attribution warnings discovered by the fold. The
    /// classifier sees the correct historical inputs
    /// (active-plan hint + known active plans BEFORE this
    /// commit), so warnings are recorded here at the
    /// moment of classification. Projection filters by
    /// `(sha, plan)` to render them; it never re-runs the
    /// classifier on historical commits.
    pub warnings: Vec<RepoWarning>,
}

pub struct RepoWarning {
    pub sha: CommitSha,
    /// `None` for repo-scoped warnings (e.g.
    /// `UnknownPlanPrefix`, `AttributionMismatch` —
    /// commit-level facts not tied to a single plan).
    /// `Some(plan)` when the warning is plan-scoped
    /// (e.g. `MissingPrefix` whose suggestion names a
    /// single plan, or `DanglingPlanRef` for the missing
    /// key).
    pub plan: Option<PlanKey>,
    pub warning: Warning,
}

/// One active plan. Just its timeline. All derived facts
/// (body, intro SHA, last activity, stage,
/// participants_cumulative, latest_revision,
/// latest_implementation) are projections.
pub struct PlanState {
    pub commits: Vec<PlanTimelineEvent>,
}

/// Entry in `RepoState.finished_plans`. Stores the bare
/// identity of a finalized plan: its key plus the two
/// boundary SHAs (intro = first commit of this instance,
/// finalized_at = the commit whose tree fired the freeze
/// predicate). Body text, approver count, and the full
/// historical timeline are recoverable via `git_io`:
///   `git show <finalized_at>:.trinity/plans/<plan>.md`
///   gets the body at freeze;
///   walking `intro..=finalized_at` reconstructs which
///   commits belonged to the plan if a future feature
///   needs full history. The fold itself does NOT retain
///   per-commit history for finished plans — the
///   `PlanState.commits` Vec is dropped when the plan
///   exits `state.plans`.
pub struct FinishedPlan {
    pub plan: PlanKey,
    pub intro: CommitSha,
    pub finalized_at: CommitSha,
}

/// One entry in a plan's timeline: how a single commit
/// affected this plan. Multi-plan commits emit one event
/// per touched plan.
pub struct PlanTimelineEvent {
    pub sha: CommitSha,
    pub ts: i64,
    /// True iff this commit modified
    /// `.trinity/plans/<plan>.md`.
    pub touched_plan: bool,
    /// True iff this commit had non-plan, non-trinity code
    /// changes attributed to this plan (explicit `[plan]`
    /// prefix or walk-back).
    pub touched_code: bool,
}

/// One entry in `state.ad_hoc`.
pub struct AdHocEvent {
    pub sha: CommitSha,
    pub ts: i64,
    /// True iff this commit had non-plan code changes.
    pub touched_code: bool,
}

/// All facts the fold needs about one commit. Built by
/// the daemon (or any other caller) before calling
/// `state.apply_commit`. Sans-io.
pub struct CommitEvent {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub plan_touches: Vec<PlanTouchInput>,
    /// Plans whose freeze rule fires at this commit. The
    /// daemon computes this by inspecting the commit's
    /// tree: a plan freezes when its `.trinity/finished/<plan>/`
    /// directory has ≥1 file and all files are APPROVE
    /// verdicts AND the plan is currently active
    /// (in `state.plans` at the moment the daemon builds
    /// the event). The fold never inspects file contents
    /// or tracks per-plan approver counts.
    pub newly_finished: BTreeSet<PlanKey>,
    pub has_code_changes: bool,
}

pub struct PlanTouchInput {
    pub plan: PlanKey,
    pub kind: TouchKind,
}

pub enum TouchKind {
    Intro,
    Revise,
    Delete,
}

/// Classifier inputs: built from a CommitEvent + the fold's
/// current state (active-plan hint + known active plans).
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    pub touches: &'a BTreeMap<PlanKey, TouchKind>,
    pub finalizes: &'a BTreeSet<PlanKey>,
    pub has_code_changes: bool,
    pub active_plan_hint: Option<&'a PlanKey>,
    pub known_plans: &'a BTreeSet<PlanKey>,
}

/// Classifier output: per-commit attribution decision +
/// warnings + the hint update.
pub struct ClassifierOutput {
    /// Plans the master attributed this commit to. Multiple
    /// entries for `[plan-a,plan-b]`; one for `[plan-x]` or
    /// walk-back; empty for `[misc]` / unknown prefix / no
    /// attribution at all.
    pub plan_attribution: BTreeSet<PlanKey>,
    pub warnings: Vec<Warning>,
    /// The new active-plan hint after this commit. Single-plan
    /// attribution sets it to that plan; multi-plan attribution
    /// is "transparent" — the previous hint is preserved (a
    /// `[plan-a,plan-b]` commit doesn't change which plan we're
    /// dominantly working on); `[misc]` / unknown also preserve
    /// the previous hint.
    pub next_active_plan_hint: Option<PlanKey>,
}

/// Typed warning. Closed vocabulary of facts the fold
/// notices but doesn't act on. The variants here are all
/// attribution-related (the only category we surface
/// today); future warning categories (e.g. malformed
/// finalize file, hook violation, etc.) get their own
/// variants without changing the wrapper. Emitted by the
/// classifier during `apply_commit` and persisted on
/// `RepoState.warnings` (wrapped in `RepoWarning`).
pub enum Warning {
    UnknownPlanPrefix { unknown_names: Vec<String> },
    MissingPrefix { suggested_prefix: String },
    AttributionMismatch {
        attributed: Vec<PlanKey>,
        touched: Vec<PlanKey>,
    },
    DanglingPlanRef { plan: PlanKey },
}
```

### Projection types (NOT stored)

These types are used by WFW + response builders to
synthesize "everything about this commit" views from
`PlanTimelineEvent` + `git_io` + feedback scans. They live
in `trinity-core` for sharing across consumers but they
never appear as fields on `RepoState`.

```rust
/// Rich per-commit view, synthesized on demand. One
/// view per SHA (repo-scoped); `feedback` carries every
/// review for the SHA across every scope, each item
/// tagged with its scope. WFW filters by scope when
/// deciding readiness; repo/home timelines can show all
/// reviews together.
pub struct CommitNode {
    pub sha: CommitSha,
    pub ts: i64,
    pub subject: String,
    pub touches: BTreeMap<PlanKey, TouchKind>,
    pub finalizes: BTreeSet<PlanKey>,
    pub has_code_changes: bool,
    /// Plans the master attributed this commit to, via the
    /// title prefix (`[plan-x]` → one plan, `[plan-a,plan-b]`
    /// → multiple plans) or walk-back (one plan). Empty set
    /// means ad-hoc / `[misc]`.
    pub plan_attribution: BTreeSet<PlanKey>,
    pub warnings: Vec<Warning>,
    pub feedback: Vec<CommitReview>,
}

/// One review for a commit, carrying its scope. Filesystem
/// identity is `(scope, sha, author)`; this struct mirrors
/// that — the projection assembles every review for the
/// SHA into the same `CommitNode.feedback` Vec, tagged
/// with its own scope, so the same author writing for plan
/// A and plan B is two `CommitReview` entries.
pub struct CommitReview {
    pub author: AgentLabel,
    /// `None` = ad-hoc / repo-scoped feedback (the file
    /// lives under `.trinity/feedback/_/<sha>/<author>.md`).
    /// `Some(plan)` = plan-scoped feedback under
    /// `.trinity/feedback/<plan>/<sha>/<author>.md`.
    pub plan: Option<PlanKey>,
    pub body: FeedbackBody,
}

pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}

pub enum ReviewPolicy {
    Blocking { participants: NonEmptyVec<AgentLabel> },
    NonBlocking { reason: NonBlockingReason },
}

pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    StructurallyNonReviewable,
}

pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}
```

### Notes on the shape

- **Plan metadata is implicit.** `PlanKey` is the
  `BTreeMap<PlanKey, PlanState>` key. The plan path is
  `.trinity/plans/<key>.md`. The body comes from
  `git show <intro_sha>:.trinity/plans/<key>.md` (or the
  tree at the latest commit with `touched_plan == true`).
- **`participants_cumulative` is a projection.** Scan
  `.trinity/feedback/<key>/*/` filtered to SHAs that
  appear in the active plan's `commits`. (Delete's
  hard-forget rule means re-introduced plan keys never
  conflate cycles — old-cycle feedback files may exist on
  disk but the new active plan's `commits` SHAs are
  disjoint from prior history.)
- **`last_activity_ts` is a projection.** `max(commits.last().ts,
  max feedback file mtime for SHAs in `commits`)`.
- **`PlanStage` is a projection.** `Drafting` if every event
  has `touched_code == false`; `Implementing` once any
  event has `touched_code == true`. Finalized plans aren't
  in `state.plans` (they're in `finished_plans`); deleted
  plans are gone entirely.
- **`intro` is `commits[0].sha`.** No separate field.
- **`latest_revision` / `latest_implementation` are
  projections** off the timeline.
- **Attribution warnings ARE stored** in
  `state.warnings: Vec<RepoWarning>` — they're folded
  facts captured while the classifier sees the correct
  historical context. Projection filters by `(sha, plan)`
  to render them; it never re-runs the classifier on
  historical commits.
- **Cache shape.** The cache file's header records the HEAD
  SHA the state was saved at. `RepoState` itself doesn't
  carry that field.

## Fold Algorithm

```rust
impl RepoState {
    pub fn apply_commit(&mut self, event: &CommitEvent) {
        // 1. Classify (computes plan_attribution + warnings
        //    + next active-plan hint). The fold doesn't
        //    compute newly_finished — it comes in on
        //    `event.newly_finished`, already filtered by
        //    the daemon to active plans.
        let touches: BTreeMap<PlanKey, TouchKind> = event
            .plan_touches
            .iter()
            .map(|t| (t.plan.clone(), t.kind))
            .collect();
        let known_plans: BTreeSet<PlanKey> =
            self.plans.keys().cloned().collect();
        let classified = classify(ClassifierInputs {
            subject: &event.subject,
            touches: &touches,
            finalizes: &event.newly_finished,
            has_code_changes: event.has_code_changes,
            active_plan_hint: self.active_plan_hint.as_ref(),
            known_plans: &known_plans,
        });

        // 2. Persist warnings while the historical
        //    classifier context is fresh. RepoWarning's
        //    `plan` field tags plan-scoped warnings (e.g.
        //    DanglingPlanRef) so projection can filter
        //    them per-plan view.
        for w in classified.warnings {
            self.warnings.push(RepoWarning {
                sha: event.sha.clone(),
                plan: warning_plan_tag(&w),
                warning: w,
            });
        }

        // 3. Apply lifecycle mutations:
        //    - Intro inserts a new PlanState AND clears
        //      self.ad_hoc.
        //    - Revise is a no-op (the per-plan event in
        //      step 4 carries the touched_plan flag).
        //    - Delete is HARD-FORGET: drop plan from
        //      `plans`, drop every `finished_plans` entry
        //      for the same key (prior cycles), drop
        //      warnings tagged with the key, and clear
        //      `active_plan_hint` if it points at the
        //      deleted key. A subsequent Intro of the same
        //      key is a fresh plan with no prior memory.
        for touch in &event.plan_touches {
            match touch.kind {
                TouchKind::Intro => {
                    self.ad_hoc.clear();
                    self.plans.insert(
                        touch.plan.clone(),
                        PlanState { commits: Vec::new() },
                    );
                }
                TouchKind::Revise => {}
                TouchKind::Delete => {
                    self.plans.remove(&touch.plan);
                    self.finished_plans
                        .retain(|f| f.plan != touch.plan);
                    // Drop warnings tagged with the key AND
                    // warnings whose payload mentions the
                    // key (e.g. AttributionMismatch's
                    // `attributed`/`touched` lists). Tag
                    // alone isn't enough — the variant data
                    // can still reference the deleted plan.
                    self.warnings.retain(|w| {
                        w.plan.as_ref() != Some(&touch.plan)
                            && !warning_mentions_plan(&w.warning, &touch.plan)
                    });
                    if self.active_plan_hint.as_ref() == Some(&touch.plan) {
                        self.active_plan_hint = None;
                    }
                }
            }
        }

        // 4. Emit ONE PlanTimelineEvent per affected plan.
        //    Affected = touches ∪ attribution ∪
        //    newly_finished. The booleans are computed
        //    from the per-plan facts; exactly one push
        //    site.
        let mut affected: BTreeSet<PlanKey> = BTreeSet::new();
        affected.extend(touches.keys().cloned());
        affected.extend(classified.plan_attribution.iter().cloned());
        affected.extend(event.newly_finished.iter().cloned());

        for plan in &affected {
            let Some(ps) = self.plans.get_mut(plan) else {
                continue; // plan not active (e.g. attribution
                          // names a finalized plan)
            };
            ps.commits.push(PlanTimelineEvent {
                sha: event.sha.clone(),
                ts: event.author_ts,
                touched_plan: touches.contains_key(plan),
                touched_code: event.has_code_changes
                    && classified.plan_attribution.contains(plan),
            });
        }

        // 5. Move finalized plans to finished_plans and
        //    drop them from `plans`. Finalize keeps the
        //    finished_plans record (unlike Delete which
        //    hard-forgets); warnings tagged with the
        //    finalized plan stay too — they're historical
        //    facts about commits the plan saw.
        for plan in &event.newly_finished {
            if let Some(ps) = self.plans.remove(plan) {
                let intro = ps
                    .commits
                    .first()
                    .map(|e| e.sha.clone())
                    .unwrap_or_else(|| event.sha.clone());
                self.finished_plans.push(FinishedPlan {
                    plan: plan.clone(),
                    intro,
                    finalized_at: event.sha.clone(),
                });
            }
        }

        // 6. Ad-hoc bucket: no touches, no attribution,
        //    code changes present.
        if touches.is_empty()
            && classified.plan_attribution.is_empty()
            && event.has_code_changes
        {
            self.ad_hoc.push(AdHocEvent {
                sha: event.sha.clone(),
                ts: event.author_ts,
                touched_code: true,
            });
        }

        // 7. Update the active-plan hint, then sanitize:
        //    if the hint points at a plan that lifecycle
        //    just removed (Delete or Finalize), clear it.
        //    `active_plan_hint.is_none_or(|p|
        //    self.plans.contains_key(p))` is an invariant
        //    of post-apply state.
        self.active_plan_hint = classified.next_active_plan_hint;
        if let Some(hint) = &self.active_plan_hint
            && !self.plans.contains_key(hint)
        {
            self.active_plan_hint = None;
        }
    }
}
```

Helper invariants:

- `event.newly_finished` is a **stateless parent/child
  tree comparison**. The daemon (with git_io) computes it
  per commit:

  ```text
  newly_finished(plan, commit) =
      plan file exists in the commit tree
      AND finish_predicate(plan, parent_tree) == false
      AND finish_predicate(plan, commit_tree) == true

  finish_predicate(plan, tree) =
      .trinity/finished/<plan>/ has ≥1 file
      AND every file parses as APPROVE
  ```

  This rule is a pure function of the two trees; it does
  NOT depend on `state.plans`. The fold accepts
  `newly_finished` as ground truth and does not verify,
  track approver counts, or inspect file contents. A side
  benefit: stale approver files from a prior cycle don't
  re-finalize a re-introduced plan unless the directory
  itself changes after re-intro (the parent tree's
  predicate is true → no transition).
- A commit with `touches = {A: Intro}` clears
  `self.ad_hoc` before anything else. This is the "new
  plan absorbs/discards prior ad-hoc work" rule.
- A `TouchKind::Delete` is **hard-forget**: it wipes
  `plans[key]`, every `finished_plans` entry for the same
  key, every `warning` whose tag is the key OR whose
  payload mentions the key (see `warning_mentions_plan`
  below), and clears `active_plan_hint` if it pointed at
  the key. After
  delete, Trinity has no folded memory of the plan; a
  later `Intro` of the same `PlanKey` is a fresh plan.
- A `TouchKind::Finalize` (via `event.newly_finished`) is
  **soft-archive**: the plan moves to `finished_plans`,
  its `PlanState.commits` is dropped, but warnings
  tagged with the key stay (historical facts).
- Multi-plan commits (`touches.len() ≥ 2`) result in one
  event per touched plan, each with `touched_plan: true`.
- `warning_plan_tag(&Warning)` returns
  `Some(plan)` for plan-scoped variants (e.g.
  `DanglingPlanRef.plan`, the inferred plan in
  `MissingPrefix` when the suggestion names exactly one
  plan) and `None` for repo-scoped variants
  (`UnknownPlanPrefix`, `AttributionMismatch`,
  multi-plan `MissingPrefix`).
- `warning_mentions_plan(&Warning, &PlanKey)` returns
  true iff the warning's payload directly references the
  given plan: `DanglingPlanRef.plan == key`,
  `AttributionMismatch.attributed.contains(key)` or
  `.touched.contains(key)`, etc. Used by the Delete
  hard-forget path so that wiping a key removes every
  warning that even mentions it.
- Post-apply invariant:
  `active_plan_hint.is_none_or(|p|
  self.plans.contains_key(p))`. The sanitization step at
  the end of `apply_commit` enforces this.

## Cache & Incremental Fold

The cache stores `RepoState` directly. The file format:

```
[header: magic + format_version + saved_at_head_sha]
[payload: wincode-encoded RepoState]
```

Rebuild flow:

1. Resolve current HEAD via `git_io::rev_parse_head`.
2. If a cached file exists for HEAD → load + return; no
   fold.
3. Else, attempt to load a cache file whose
   `saved_at_head_sha` is an ancestor of HEAD. (The daemon
   keeps the most recent cached HEAD; if it's not an
   ancestor of the new HEAD, fall back to cold start.)
4. If a usable cache is found:
   - Walk `git_io::commits_between(saved, head)` building
     `CommitEvent`s in fold order.
   - For each event: `state.apply_commit(&event)`.
   - Write the cache at the new HEAD; delete the previous
     cache file in the same write.
5. Otherwise (cold start, or no usable ancestor): walk
   history from the root commit, building `CommitEvent`s
   per commit, applying each.

Cache error handling:

- Any `try_load` error (bad magic, format-version
  mismatch, head mismatch, decode failure) MUST delete
  the offending file before falling back. Today the
  fallback is silent and stale files leak. The new loader
  bakes delete-on-error in.

Validating wrappers (`NonEmptyVec`) need cache encoding
that goes through their validation. wincode-derive
bypasses constructors. Either:

- Gate cache encoding behind a serde-with-TryFrom
  boundary, or
- Hand-write `SchemaRead` impls that call the validating
  constructor.

The new code must not ship with a wincode bypass on
validated types. Phase 1 picks the approach.

## Projection Layer

WFW and response builders synthesize the "everything about
this commit" view on demand. The projection layer lives in
the daemon (`src/responses.rs`, `src/server/wait.rs`,
`src/server/http.rs`, `src/preview.rs`) and consumes both
`RepoState` + live IO.

Projection inputs:

- `&RepoState` — folded state.
- `git_io` reads — commit subjects, diffs, trees (for body
  text).
- Feedback scan — `.trinity/feedback/<scope>/<sha>/<author>.md`
  files.
- Snapshotted `Config` — review-policy settings.

Common queries:

- Plan timeline → `state.plans[plan].commits.iter()`.
- Plan body → `git show <commits[0].sha>:.trinity/plans/<plan>.md`
  (or the tree at the most recent `touched_plan` event).
- Latest reviewable for plan → `state.plans[plan].commits.last()`
  (the event's `touched_plan || touched_code` determines
  what kind of review).
- Plan stage → derived from `commits`
  (Drafting/Implementing).
- Last activity → `max(commits.last().ts, max feedback
  mtime)`.
- Participants cumulative → distinct authors across
  feedback files under `.trinity/feedback/<plan>/`.
- Per-commit warnings → filter `state.warnings` by SHA
  (and optionally by plan). The classifier emitted them
  during the fold; projection only displays.
- Finished plan view → `state.finished_plans` gives
  `{ plan, intro, finalized_at }`. The UI can show plan
  identity, intro/finalize SHAs, and the body at finalize
  via `git show <finalized_at>:.trinity/plans/<plan>.md`.
  The full historical commit timeline is NOT in
  `RepoState` for finished plans — the fold drops
  `PlanState.commits` on finalize. A future feature can
  walk `intro..=finalized_at` to reconstruct it on
  demand; this plan keeps `RepoState` small and accepts
  the tradeoff.

`review_policy` / `readiness` are free functions. The
caller picks the relevant scope (a `Some(plan)` for plan
review, `None` for ad-hoc) and the function filters
`feedback` to only items matching that scope:

```rust
pub fn review_policy(
    event: &PlanTimelineEvent,
    scope: Option<&PlanKey>,
    plan_state: Option<&PlanState>,
    ad_hoc_participants: &[AgentLabel],
    feedback: &[CommitReview],
    config: &Config,
) -> ReviewPolicy;

pub fn readiness(/* same inputs */) -> ReviewReadiness;
```

The projection layer is daemon-side because it needs IO.
The core fold is sans-io.

## Implementation Phases

Phase 0 was the prior plan's Phase 1 + Phase 2 (deleted
bucket types + added flat-facts scaffolding). That work
landed; this plan reuses what's salvageable
(`Warning` typed enum, the classifier's logic
shape) and replaces the rest.

### Phase 1 — Move + restructure (atomic)

ONE atomic commit (or PR landing as one squashed commit).

- Create `crates/trinity-core/src/repo_state.rs` with the
  Core State types from above.
- Move the classifier (`classify`, `ClassifierInputs`,
  `parse_title_prefix`) from `model::facts` into
  `repo_state`. Reshape `ClassifierOutput` to `{
  plan_attribution: BTreeSet<PlanKey>, warnings,
  next_active_plan_hint: Option<PlanKey> }`
  (drop the bundled `CommitFacts`; the fold passes the
  facts directly into the events it emits).
- Implement `RepoState::apply_commit(&mut self,
  event: &CommitEvent)` per the Fold Algorithm above.
- Delete from `trinity-core::model::facts`:
  - `CommitNode` (legacy facts shape) — replaced by the
    projection-only `CommitNode` in `repo_state`.
  - `CommitFacts` — folded into apply_commit's local
    variables.
  - `PlanState` (Phase 2's shape with timeline, intro,
    latest_revision, etc.) — replaced by the minimal
    `PlanState`.
  - `AdHocState` — replaced by `Vec<AdHocEvent>`.
  - `PlanStage` enum — projection only; defined in
    `repo_state` if a UI consumer needs it.
- Delete `ReviewScope` enum and `CommitReviews` struct
  entirely. Replace with `CommitReview { author, plan:
  Option<PlanKey>, body }` and `CommitNode.feedback:
  Vec<CommitReview>` (one CommitNode per SHA, repo-scoped;
  filesystem `(scope, sha, author)` identity preserved by
  the `plan: Option<PlanKey>` on each review).
- Delete from `trinity-core::model`:
  - Legacy `CommitNode` (`kind`, `attribution`, `plans`,
    `gate`, `attribution_warning`).
  - Legacy `Plan` (`timeline: Vec<PlanTimelineEvent>`,
    `last_activity_ts`, `plan_intro_parent`,
    `archived_cycles`, `body_hash`, `body`, `lifecycle`,
    `plan_intro`).
  - `model::PlanTimelineEvent` (the legacy enum-of-rows;
    the new `PlanTimelineEvent` struct lives in
    `repo_state`).
  - `Feedback`, `CommitGate` (legacy review storage).
  - `plan_conflicts` field + `PlanConflict` API type.
- Daemon-side `src/repo_state.rs` becomes a re-export
  shim or is deleted outright (consumers import from
  `trinity_core::repo_state`).
- Daemon-side `src/disk_snapshot.rs` shrinks to its IO
  role: walk git history → `Vec<CommitEvent>`, call
  `state.apply_commit(&event)` per event. The post-fold
  feedback-attach pass (current `attach_live_feedback`)
  becomes a projection-layer concern, not part of fold
  state.
- Rewrite WFW (`src/server/wait.rs`) and response
  projections (`src/responses.rs`, `src/server/http.rs`,
  `src/preview.rs`) to read `state.plans[plan].commits`
  directly and call projection functions for body /
  participants / readiness.
- Preserve existing API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're tagged enums built by
  projection.
- Bump `state_cache::CACHE_FORMAT_VERSION` (v3 → v4).
  - v4 stores the new `RepoState` shape. Old caches
    invalidate.
  - Bake delete-on-error into the loader.
- The cache continues to work in cold-start mode at the
  end of this phase. Incremental rebuild lands in Phase 2.

### Phase 2 — Incremental cache loading (additive)

- Change the cache file format to include a header with
  `saved_at_head_sha`.
- Rebuild logic: try to load the cache; if its
  `saved_at_head_sha` is an ancestor of current HEAD,
  walk `git_io::commits_between(saved, head)`, build
  `CommitEvent`s, call `apply_commit` for each, write the
  new cache.
- Cold start (no usable cache) still walks from the root
  commit.
- Tests:
  - Apply N commits via `apply_commit`, snapshot
    `RepoState`. Apply one more via `apply_commit`. The
    result equals a from-root re-fold over all N+1
    commits.
  - Cache load at ancestor SHA + fold-forward equals the
    cold-start equivalent.

### Phase 3 — Cleanup

- Remove `CommitAttribution` and any other stale helpers
  left behind from the legacy shape.
- Delete the now-superseded prior plan file
  `.trinity/plans/core-model-invalid-states-unrepresentable.md`
  (if it hasn't been finalized already by Trinity).
- Retire `.trinity/plans/tagged-enums-for-non-orthogonal-fields.md`
  — its intent is preserved for API/projection types in
  this plan's Projection Layer section; the core-model
  intent is fully absorbed.

## Tests

### Unit (trinity-core)

- Classifier: every attribution path covered (already
  landed in Phase 0). Tests stay green after the move to
  `repo_state`.
- Fold invariants:
  - `apply_commit` with `touches = {A: Intro}` inserts
    `state.plans[A]` with `commits = [event]`
    (touched_plan true, touched_code false). Also clears
    `state.ad_hoc`.
  - `touches = {A: Revise}` after intro appends an event
    with `touched_plan: true`.
  - `touches = {A: Delete}` (hard-forget): removes A from
    `state.plans`, removes every `finished_plans` entry
    for A, removes every `warnings` entry tagged with A,
    clears `active_plan_hint` if it pointed at A.
  - `event.newly_finished = {A}` (soft-archive): emits an
    event into A's `commits` (touched_plan/touched_code
    per the commit's facts), moves A to `finished_plans`,
    drops A from `state.plans`. `warnings` tagged with A
    stay.
  - `newly_finished = {A}` + `touches = {B: Revise}`: A
    exits state into `finished_plans`; B's `commits`
    gains a touched_plan event.
  - Multi-plan touch: `touches = {A: Revise, B: Revise}`
    appends one event to each.
  - Multi-plan attribution: `[plan-a,plan-b]` →
    `plan_attribution = {A, B}`; both A's and B's
    `commits` get a touched_code event when
    `has_code_changes`. `active_plan_hint` is preserved
    (multi-plan attribution is transparent to the hint).
  - Code-only attribution: `plan_attribution = {A}`,
    `has_code_changes = true`, `touches = ∅` appends a
    `touched_code: true` event to A's commits.
  - Ad-hoc eligible commit appends to `state.ad_hoc`.
  - Intro after ad-hoc commits clears `state.ad_hoc`.
  - Warnings: classifier-emitted warnings land in
    `state.warnings` tagged with `(sha, plan_tag)`.
    `UnknownPlanPrefix` / `AttributionMismatch` /
    multi-plan `MissingPrefix` are tagged with
    `plan: None`. `DanglingPlanRef` is tagged with
    `Some(the dangling key)`. Single-plan
    `MissingPrefix` is tagged with `Some(the inferred
    plan)`.
  - `active_plan_hint` post-condition: after every
    `apply_commit` call,
    `state.active_plan_hint.is_none_or(|p|
    state.plans.contains_key(p))`. Tested across
    Delete-of-hint, Finalize-of-hint, and
    classifier-set-then-deleted scenarios.
- Incremental-fold equivalence: snapshot `RepoState`
  after N commits; apply commit N+1 via `apply_commit`.
  Result equals a from-root re-fold over all N+1
  commits.

### Acceptance grep (CI)

- `grep -rn "pub kind: " crates/trinity-core/src/` returns
  no canonical-struct hits.
- `grep -rn "pub gate: " crates/trinity-core/src/` returns
  no hits.
- `grep -rn "FoldCarry" crates/ src/` returns no hits.
- `grep -rn "commit_order\|plan_conflicts" crates/trinity-core/src/repo_state.rs`
  returns no hits.
- `grep -rn "model::Plan\b\|model::CommitNode\b\|model::Feedback\b\|model::CommitGate\b\|model::PlanTimelineEvent"
  crates/ src/` returns no hits.

### Regression

- Plan-scoped WFW finds the right candidate for plans
  going through revise → implement → revise → implement.
- Multi-plan commits surface review work for each touched
  plan.
- Finalized plans surface no review work (absent from
  `state.plans`).
- Late feedback on a finalized plan does not produce WFW
  work (the plan isn't in state).
- Repo-scoped WFW still surfaces ad-hoc-eligible commits
  per config.
- Rendered plan-page timeline matches today's output on
  golden-file fixtures.

### CI

- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend` if frontend types
  change.
- `cargo fmt -- --check`.

## Acceptance

- `RepoState` is `{ plans, finished_plans, ad_hoc,
  active_plan_hint, warnings }`. Lives in
  `crates/trinity-core/src/repo_state.rs`.
- `FinishedPlan` is `{ plan: PlanKey, intro: CommitSha,
  finalized_at: CommitSha }`.
  `state.finished_plans: Vec<FinishedPlan>` is the ordered
  log of finalize events. Body / approver count / full
  historical timeline are recoverable via `git_io`
  (e.g. `git show <finalized_at>:.trinity/plans/<plan>.md`,
  or walking `intro..=finalized_at` if a future plan
  needs full history reconstruction).
- `PlanState` is `{ commits: Vec<PlanTimelineEvent> }`.
  No body, body_hash, intro, last_activity, archived_cycles,
  latest_revision, latest_implementation,
  participants_cumulative, stage fields.
- `PlanTimelineEvent` is `{ sha, ts, touched_plan,
  touched_code }`.
- `state.ad_hoc` is `Vec<AdHocEvent>`. Cleared on plan
  intro.
- `apply_commit` is `RepoState::apply_commit(&mut self,
  event: &CommitEvent)`. No `FoldCarry` exists in the
  codebase.
- The fold module is sans-io: no `use ... git_io`, no
  `tokio::fs`, no IO of any kind.
- The cache stores the entire `RepoState`. Cache file
  header records the saved-at HEAD SHA. Warm rebuilds
  load + apply new commits; cold starts walk from root.
- Cache `try_load` errors delete the offending file.
- WFW and response projection read from `state.plans` /
  `state.ad_hoc` and call projection functions for body /
  participants / readiness / warnings.
- The grep-mechanized invariants pass.
- `core-model-invalid-states-unrepresentable.md` is
  deleted from `.trinity/plans/`.
- `tagged-enums-for-non-orthogonal-fields.md` is deleted
  from `.trinity/plans/`.

## Out of Scope

- Changing user-facing approval / RC / force-finish
  semantics.
- Live config reload — config snapshotted at startup.
- `.trinity/config` policies for ad-hoc commits (deny /
  require-review / ignore). The new design tracks ad-hoc
  events; the policy layer is a follow-up plan.
- Adding any per-plan derived-index cache. PlanState IS
  the per-plan cache; cross-plan queries iterate
  `state.plans` and `state.ad_hoc`.
- Multi-SHA checkpoint caching. This plan ships per-HEAD
  cache with single-ancestor fold-forward. If branch
  divergence or long history becomes a problem, a future
  plan adds checkpoints.
- Removing the web UI.
