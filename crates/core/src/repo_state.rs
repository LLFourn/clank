//! Sans-io fold state for one repo.
//!
//! `RepoState` carries active workflow facts only — finished plans
//! are summarized in `finished_plans`, deleted plans are
//! hard-forgotten. Ad-hoc commits live in `ad_hoc` until the next
//! plan intro clears them. Attribution warnings discovered by the
//! fold are recorded in `warnings` at the moment the classifier
//! sees them.
//!
//! `apply_commit` is the single fold step: take a `CommitEvent`
//! (built by the daemon from git_io), classify it, mutate state.
//! No git, no filesystem, no async — everything callers need lives
//! on the inputs.
//!
//! Cache stores `RepoState` directly under the `cache-encoding`
//! feature. Projection types (`CommitNode`, `CommitReview`,
//! `ReviewPolicy`, `ReviewReadiness`) live in this module too but
//! are NEVER stored in `RepoState` — they are synthesized on
//! demand by the daemon's projection layer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::vocab::{CommitGateState, Verdict};

// ============================================================
// Canonical fold state
// ============================================================

/// Top-level fold output. The cache stores this directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct RepoState {
    /// Active plans only. A plan exits the map on finalize (moved
    /// to `finished_plans`) or on `TouchKind::Delete` (gone from
    /// state entirely; see hard-forget semantics in `apply_commit`).
    pub plans: BTreeMap<PlanKey, PlanState>,

    /// Ordered log of finalize events: "plan X was finalized at
    /// SHA Y." Body at freeze / approver count / full historical
    /// commit timeline are recoverable via `git_io`. `PlanKey` may
    /// repeat (a plan can be re-introduced and re-finalized).
    pub finished_plans: Vec<FinishedPlan>,

    /// Out-of-plan commits in fold order. Cleared on the next plan
    /// intro.
    pub ad_hoc: Vec<AdHocEvent>,

    /// The plan the master is assumed to be working on right now.
    /// Set by `apply_commit` from the classifier's
    /// `next_active_plan_hint`. Sanitized after each commit so
    /// `active_plan_hint.is_none_or(|p| plans.contains_key(p))`
    /// always holds post-apply.
    pub active_plan_hint: Option<PlanKey>,

    /// Attribution warnings discovered by the fold. The classifier
    /// sees the correct historical inputs (active-plan hint +
    /// known active plans BEFORE this commit), so warnings are
    /// recorded here at the moment of classification. Projection
    /// filters by `(sha, plan)` for display; it never re-runs the
    /// classifier on historical commits.
    pub warnings: Vec<RepoWarning>,

    /// Has this repo ever participated in clank? Set true on the
    /// first commit that touches any plan (intro / revise /
    /// finalize / delete) and never cleared, even when
    /// `PlanDeleted` removes the plan from `plans` without
    /// promoting it into `finished_plans`. Gate for `LogEvent::AdHoc`
    /// emission — pre-adoption code commits don't surface as
    /// ad-hoc events.
    #[serde(default)]
    pub adopted: bool,
}

/// One active plan. Just its per-commit timeline; every other
/// per-plan fact (body, intro SHA, last activity, stage,
/// participants, latest revision/implementation) is a projection
/// computed on demand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct PlanState {
    pub commits: Vec<PlanTimelineEvent>,
}

impl PlanState {
    /// SHAs of commits in this plan's timeline that count as
    /// "reviewable" — anything that touched the plan file or
    /// touched code attributed to the plan. This IS the
    /// definition; every site that needs the list (status,
    /// wfw, feedback write, preview builders) should call this
    /// rather than hand-rolling the filter so the rule stays
    /// in one place.
    pub fn reviewable_shas(&self) -> Vec<CommitSha> {
        self.commits
            .iter()
            .filter(|e| e.touched_plan || e.touched_code)
            .map(|e| e.sha.clone())
            .collect()
    }
}

/// One entry in a plan's timeline: how a single commit affected
/// this plan. Multi-plan commits emit one event per touched plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct PlanTimelineEvent {
    pub sha: CommitSha,
    pub ts: i64,
    /// True iff this commit modified `.clank/plans/<plan>.md`.
    pub touched_plan: bool,
    /// True iff this commit had non-plan, non-clank code
    /// changes attributed to this plan (via explicit prefix or
    /// active-plan-hint inheritance).
    pub touched_code: bool,
}

/// Entry in `RepoState.finished_plans`. Stores plan identity plus
/// the two boundary SHAs (intro = first commit of this instance,
/// finalized_at = commit whose tree fired the freeze predicate).
/// Body / approver count / full historical timeline are recoverable
/// via git_io.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct FinishedPlan {
    pub plan: PlanKey,
    pub intro: CommitSha,
    pub finalized_at: CommitSha,
}

/// One entry in `state.ad_hoc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct AdHocEvent {
    pub sha: CommitSha,
    pub ts: i64,
    pub touched_code: bool,
}

/// One attribution warning seen by the fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct RepoWarning {
    pub sha: CommitSha,
    /// `None` for repo-scoped warnings (e.g. `UnknownPlanPrefix`,
    /// `AttributionMismatch` — commit-level facts not tied to a
    /// single plan). `Some(plan)` when the warning is plan-scoped
    /// (e.g. single-plan `MissingPrefix`, `DanglingPlanRef`).
    pub plan: Option<PlanKey>,
    pub warning: Warning,
}

/// Typed warning vocabulary. Closed set of facts the fold notices
/// but doesn't act on. The fold writes a `RepoWarning` for each;
/// projection filters by `(sha, plan)` to display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Warning {
    /// Title prefix names plan(s) that aren't in `known_plans` at
    /// classification time. The classifier sets `plan_attribution
    /// = ∅` and emits this.
    UnknownPlanPrefix { unknown_names: Vec<String> },
    /// No prefix; classifier inferred attribution from touches or
    /// the active-plan hint. `suggested_prefix` is what amending
    /// the title would look like (`[plan-a]` or `[plan-a,plan-b]`).
    MissingPrefix { suggested_prefix: String },
    /// Explicit prefix names plan(s) that disagree with the
    /// commit's `touches`. The prefix wins for `plan_attribution`;
    /// reviewers see both sides.
    AttributionMismatch {
        attributed: Vec<PlanKey>,
        touched: Vec<PlanKey>,
    },
    /// Emitted by projection helpers when a `plan_attribution`
    /// (or `touches` key) names a plan that no longer exists in
    /// `state.plans`. Not a fold-time warning per se — the
    /// classifier doesn't know about future deletions; projection
    /// surfaces it on each affected commit.
    DanglingPlanRef { plan: PlanKey },
}

// ============================================================
// LogEvent — fold output (side-channel for `clank log`)
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    PlanIntro {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
    },
    PlanCommit {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
        touched_plan: bool,
        touched_code: bool,
    },
    PlanFinalized {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
    },
    PlanDeleted {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
    },
    AdHoc {
        sha: CommitSha,
        ts: i64,
    },
}

// ============================================================
// CommitEvent — fold input
// ============================================================

/// All facts the fold needs about one commit. The daemon (or any
/// other caller) builds this from git_io before calling
/// `state.apply_commit(&event)`. Sans-io: no path types, no
/// trees, just the closed set of facts the classifier and fold
/// consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEvent {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub plan_touches: Vec<PlanTouchInput>,
    pub has_code_changes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTouchInput {
    pub plan: PlanKey,
    pub kind: TouchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(rename_all = "snake_case")]
pub enum TouchKind {
    Intro,
    Revise,
    Delete,
    Finish,
}

// ============================================================
// Classifier
// ============================================================

/// Classifier inputs: built from a `CommitEvent` + the fold's
/// current state (active-plan hint + known active plans BEFORE
/// the commit is applied).
#[derive(Debug, Clone)]
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    pub touches: &'a BTreeMap<PlanKey, TouchKind>,
    pub has_code_changes: bool,
    pub active_plan_hint: Option<&'a PlanKey>,
    pub known_plans: &'a BTreeSet<PlanKey>,
}

/// Classifier output: attribution decision + warnings + the new
/// active-plan hint after this commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierOutput {
    /// Plans the master attributed this commit to. Multiple entries
    /// for `[plan-a,plan-b]`; one for `[plan-x]` or hint-inherit;
    /// empty for `[misc]` / unknown prefix / no attribution.
    pub plan_attribution: BTreeSet<PlanKey>,
    pub warnings: Vec<Warning>,
    /// New active-plan hint after this commit. Single-plan
    /// attribution sets it to that plan; multi-plan / `[misc]` /
    /// unknown / no-attribution preserve the incoming hint.
    pub next_active_plan_hint: Option<PlanKey>,
}

/// Title-prefix vocabulary. `[misc]` is the explicit ad-hoc
/// opt-out; `[plan-a]` / `[plan-a,plan-b]` lists plan names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitlePrefix {
    Misc,
    Plans(Vec<String>),
}

/// Pure prefix parser. Returns `None` for "no recognized prefix";
/// the classifier then falls back to touch / hint inference.
pub fn parse_title_prefix(subject: &str) -> Option<TitlePrefix> {
    let trimmed = subject.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let inner = &rest[..close];
    if inner.is_empty() {
        return None;
    }
    let names: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    if names.len() == 1 && names[0].eq_ignore_ascii_case("misc") {
        return Some(TitlePrefix::Misc);
    }
    Some(TitlePrefix::Plans(names))
}

/// Pure classifier. Computes `plan_attribution` (a set, possibly
/// empty) from the title prefix and active-plan hint; emits
/// warnings for unknown / mismatched / missing prefixes;
/// computes the next active-plan hint.
///
/// Attribution priority:
/// 1. `[misc]` → `∅`, hint preserved.
/// 2. `[plan-x]` known → `{plan-x}`, hint = `Some(plan-x)`.
/// 3. `[plan-x]` unknown → `∅`, hint preserved, `UnknownPlanPrefix`.
/// 4. `[a,b,...]` multi-plan known → `{a, b, ...}`, hint
///    preserved (multi-plan is transparent to the hint).
/// 5. No prefix, exactly one entry in `touches` → `{touched}`,
///    hint = `Some(touched)`, `MissingPrefix` suggestion (unless
///    the inferred plan already matches the hint).
/// 6. No prefix, multiple entries in `touches` → `∅`, hint
///    preserved, `MissingPrefix` listing all touched plans.
/// 7. No prefix, no touches, code changes, hint present →
///    inherit hint, no warning (the implementation chain is the
///    expected shape).
/// 8. Otherwise → `∅`, hint preserved.
pub fn classify(inputs: ClassifierInputs<'_>) -> ClassifierOutput {
    let prefix = parse_title_prefix(inputs.subject);
    let mut warnings: Vec<Warning> = Vec::new();

    let touched_plans: BTreeSet<&PlanKey> = inputs.touches.keys().collect();
    let touched_owned: Vec<PlanKey> = touched_plans.iter().map(|k| (*k).clone()).collect();

    let (plan_attribution, next_hint): (BTreeSet<PlanKey>, Option<PlanKey>) = match prefix {
        Some(TitlePrefix::Misc) => (BTreeSet::new(), inputs.active_plan_hint.cloned()),
        Some(TitlePrefix::Plans(names)) => {
            let parsed: Vec<Result<PlanKey, String>> = names
                .iter()
                .map(|n| PlanKey::parse(n).map_err(|_| n.clone()))
                .collect();
            let unknown: Vec<String> = parsed
                .iter()
                .filter_map(|r| match r {
                    Ok(k) if inputs.known_plans.contains(k) => None,
                    Ok(k) => Some(k.as_str().to_string()),
                    Err(s) => Some(s.clone()),
                })
                .collect();
            if !unknown.is_empty() {
                warnings.push(Warning::UnknownPlanPrefix {
                    unknown_names: unknown,
                });
                (BTreeSet::new(), inputs.active_plan_hint.cloned())
            } else {
                let valid: BTreeSet<PlanKey> = parsed.into_iter().flatten().collect();
                if valid.len() == 1 {
                    let plan = valid.iter().next().cloned().expect("len==1");
                    let touched_set: BTreeSet<PlanKey> = touched_owned.iter().cloned().collect();
                    let expected: BTreeSet<PlanKey> = std::iter::once(plan.clone()).collect();
                    if !touched_set.is_empty() && touched_set != expected {
                        warnings.push(Warning::AttributionMismatch {
                            attributed: vec![plan.clone()],
                            touched: touched_owned.clone(),
                        });
                    }
                    let attribution: BTreeSet<PlanKey> = std::iter::once(plan.clone()).collect();
                    (attribution, Some(plan))
                } else {
                    let attributed: Vec<PlanKey> = valid.iter().cloned().collect();
                    let touched_set_refs: BTreeSet<&PlanKey> = touched_plans.clone();
                    let any_missing = attributed.iter().any(|p| !touched_set_refs.contains(p));
                    let extra_touched = touched_plans.iter().any(|p| !valid.contains(*p));
                    if any_missing || extra_touched {
                        warnings.push(Warning::AttributionMismatch {
                            attributed: attributed.clone(),
                            touched: touched_owned.clone(),
                        });
                    }
                    (valid, inputs.active_plan_hint.cloned())
                }
            }
        }
        None => {
            if touched_plans.len() == 1 {
                let plan = (*touched_plans.iter().next().expect("len==1")).clone();
                if inputs.active_plan_hint != Some(&plan) {
                    warnings.push(Warning::MissingPrefix {
                        suggested_prefix: format!("[{}]", plan.as_str()),
                    });
                }
                let attribution: BTreeSet<PlanKey> = std::iter::once(plan.clone()).collect();
                (attribution, Some(plan))
            } else if touched_plans.len() >= 2 {
                let suggested = format!(
                    "[{}]",
                    touched_plans
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                warnings.push(Warning::MissingPrefix {
                    suggested_prefix: suggested,
                });
                (BTreeSet::new(), inputs.active_plan_hint.cloned())
            } else if inputs.has_code_changes
                && let Some(hint) = inputs.active_plan_hint
            {
                let owned = hint.clone();
                let attribution: BTreeSet<PlanKey> = std::iter::once(owned.clone()).collect();
                (attribution, Some(owned))
            } else {
                (BTreeSet::new(), inputs.active_plan_hint.cloned())
            }
        }
    };

    ClassifierOutput {
        plan_attribution,
        warnings,
        next_active_plan_hint: next_hint,
    }
}

// ============================================================
// Fold step
// ============================================================

impl RepoState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `CommitEvent` to this state.
    pub fn apply_commit(&mut self, event: &CommitEvent) -> Vec<LogEvent> {
        let mut log_events = Vec::new();
        let touches: BTreeMap<PlanKey, TouchKind> = event
            .plan_touches
            .iter()
            .map(|t| (t.plan.clone(), t.kind))
            .collect();
        // Known plans = existing active plans ∪ plans this commit
        // introduces. Without the union, an `[foo] intro` commit sees
        // `[foo]` as unknown and emits a spurious warning.
        let mut known_plans: BTreeSet<PlanKey> = self.plans.keys().cloned().collect();
        for touch in &event.plan_touches {
            if matches!(touch.kind, TouchKind::Intro) {
                known_plans.insert(touch.plan.clone());
            }
        }

        let classified = classify(ClassifierInputs {
            subject: &event.subject,
            touches: &touches,
            has_code_changes: event.has_code_changes,
            active_plan_hint: self.active_plan_hint.as_ref(),
            known_plans: &known_plans,
        });

        for w in classified.warnings {
            self.warnings.push(RepoWarning {
                sha: event.sha.clone(),
                plan: warning_plan_tag(&w),
                warning: w,
            });
        }

        let mut intros_this_commit: BTreeSet<PlanKey> = BTreeSet::new();
        let mut finished_this_commit: BTreeSet<PlanKey> = BTreeSet::new();

        for touch in &event.plan_touches {
            match touch.kind {
                TouchKind::Intro => {
                    self.plans
                        .entry(touch.plan.clone())
                        .or_insert_with(PlanState::default);
                    intros_this_commit.insert(touch.plan.clone());
                    self.adopted = true;
                    log_events.push(LogEvent::PlanIntro {
                        plan: touch.plan.clone(),
                        sha: event.sha.clone(),
                        ts: event.author_ts,
                    });
                }
                TouchKind::Revise => {
                    self.adopted = true;
                }
                TouchKind::Delete => {
                    self.adopted = true;
                    log_events.push(LogEvent::PlanDeleted {
                        plan: touch.plan.clone(),
                        sha: event.sha.clone(),
                        ts: event.author_ts,
                    });
                    self.plans.remove(&touch.plan);
                    self.finished_plans.retain(|f| f.plan != touch.plan);
                    self.warnings.retain(|w| {
                        w.plan.as_ref() != Some(&touch.plan)
                            && !warning_mentions_plan(&w.warning, &touch.plan)
                    });
                    if self.active_plan_hint.as_ref() == Some(&touch.plan) {
                        self.active_plan_hint = None;
                    }
                }
                TouchKind::Finish => {
                    finished_this_commit.insert(touch.plan.clone());
                }
            }
        }

        let mut affected: BTreeSet<PlanKey> = BTreeSet::new();
        affected.extend(touches.keys().cloned());
        affected.extend(classified.plan_attribution.iter().cloned());
        affected.extend(finished_this_commit.iter().cloned());

        for plan in &affected {
            let Some(ps) = self.plans.get_mut(plan) else {
                continue;
            };
            let tp = touches.contains_key(plan);
            let tc = event.has_code_changes && classified.plan_attribution.contains(plan);
            ps.commits.push(PlanTimelineEvent {
                sha: event.sha.clone(),
                ts: event.author_ts,
                touched_plan: tp,
                touched_code: tc,
            });
            if !intros_this_commit.contains(plan) && !finished_this_commit.contains(plan) {
                self.adopted = true;
                log_events.push(LogEvent::PlanCommit {
                    plan: plan.clone(),
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                    touched_plan: tp,
                    touched_code: tc,
                });
            }
        }

        for plan in &finished_this_commit {
            let intro = self
                .plans
                .remove(plan)
                .and_then(|ps| ps.commits.first().map(|e| e.sha.clone()))
                .unwrap_or_else(|| event.sha.clone());
            self.finished_plans.push(FinishedPlan {
                plan: plan.clone(),
                intro,
                finalized_at: event.sha.clone(),
            });
            self.adopted = true;
            log_events.push(LogEvent::PlanFinalized {
                plan: plan.clone(),
                sha: event.sha.clone(),
                ts: event.author_ts,
            });
            if self.active_plan_hint.as_ref() == Some(plan) {
                self.active_plan_hint = None;
            }
        }

        // Ad-hoc bucket: bare code-only commit with no touches and
        // no attribution. Gated on `self.adopted` so pre-adoption
        // history (everything before the first plan touch in this
        // repo) doesn't flood the log / bucket with AdHoc events.
        // `adopted` is durable, so a later PlanDeleted that drops
        // `plans` doesn't reverse the gate.
        if !touches.is_empty() || !classified.plan_attribution.is_empty() {
            self.ad_hoc.clear();
        } else if self.adopted {
            if event.has_code_changes {
                self.ad_hoc.push(AdHocEvent {
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                    touched_code: true,
                });
            }
            log_events.push(LogEvent::AdHoc {
                sha: event.sha.clone(),
                ts: event.author_ts,
            });
        }

        // Update the active-plan hint, then sanitize: clear it if
        // the classifier set a hint that lifecycle just removed
        // (Delete or Finalize).
        self.active_plan_hint = classified.next_active_plan_hint;
        if let Some(hint) = &self.active_plan_hint
            && !self.plans.contains_key(hint)
        {
            self.active_plan_hint = None;
        }
        log_events
    }
}

/// Returns `Some(plan)` when the warning is plan-scoped:
/// `DanglingPlanRef.plan`, single-plan `MissingPrefix` whose
/// suggestion names one plan. `None` otherwise.
pub fn warning_plan_tag(w: &Warning) -> Option<PlanKey> {
    match w {
        Warning::DanglingPlanRef { plan } => Some(plan.clone()),
        Warning::MissingPrefix { suggested_prefix } => {
            let inner = suggested_prefix
                .trim_start_matches('[')
                .trim_end_matches(']');
            if !inner.contains(',') {
                return PlanKey::parse(inner.trim()).ok();
            }
            None
        }
        _ => None,
    }
}

/// True iff `warning` directly references `key` in its payload —
/// used by the `Delete` hard-forget path so that wiping a key
/// removes every warning that even mentions it, regardless of the
/// tag (some variants are tagged `None` but still reference
/// individual plans, e.g. `AttributionMismatch`).
pub fn warning_mentions_plan(warning: &Warning, key: &PlanKey) -> bool {
    match warning {
        Warning::UnknownPlanPrefix { unknown_names } => {
            unknown_names.iter().any(|n| n == key.as_str())
        }
        Warning::MissingPrefix { suggested_prefix } => {
            let inner = suggested_prefix
                .trim_start_matches('[')
                .trim_end_matches(']');
            inner.split(',').any(|n| n.trim() == key.as_str())
        }
        Warning::AttributionMismatch {
            attributed,
            touched,
        } => attributed.contains(key) || touched.contains(key),
        Warning::DanglingPlanRef { plan } => plan == key,
    }
}

// ============================================================
// Projection-only types (NEVER stored on RepoState)
// ============================================================

/// Rich per-commit view, synthesized on demand by the projection
/// layer. One view per SHA (repo-scoped); `feedback` carries
/// every review for the SHA across every scope, each item tagged
/// with its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitNode {
    pub sha: CommitSha,
    pub ts: i64,
    pub subject: String,
    pub touches: BTreeMap<PlanKey, TouchKind>,
    pub has_code_changes: bool,
    /// Plans the master attributed this commit to. Empty set ==
    /// ad-hoc / `[misc]`.
    pub plan_attribution: BTreeSet<PlanKey>,
    pub warnings: Vec<Warning>,
    pub feedback: Vec<CommitReview>,
}

/// One review for a commit, carrying its scope. The same author
/// writing for plan A and plan B is two `CommitReview` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReview {
    pub author: AgentLabel,
    /// `None` = ad-hoc / repo-scoped feedback (file lives under
    /// `.clank/agents/<author>/feedback/_/<ref>.md`).
    /// `Some(plan)` = plan-scoped feedback under
    /// `.clank/agents/<author>/feedback/<plan>/<ref>.md`.
    pub plan: Option<PlanKey>,
    pub body: FeedbackBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    StructurallyNonReviewable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewPolicy {
    Blocking {
        participants: NonEmptyVec<AgentLabel>,
    },
    NonBlocking {
        reason: NonBlockingReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}

/// A `Vec` that cannot be empty. Used by `ReviewPolicy::Blocking`
/// where the type's purpose demands at least one element.
///
/// Serde decoding goes through `TryFrom<Vec<T>>` so the invariant
/// holds across the wire. Wincode cache encoding is intentionally
/// not derived — `ReviewPolicy` is a projection, never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    bound(
        serialize = "T: Clone + Serialize",
        deserialize = "T: Clone + Deserialize<'de>"
    ),
    try_from = "Vec<T>",
    into = "Vec<T>"
)]
pub struct NonEmptyVec<T>
where
    T: Clone,
{
    inner: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyVecError;

impl std::fmt::Display for NonEmptyVecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NonEmptyVec cannot be constructed from an empty Vec")
    }
}

impl std::error::Error for NonEmptyVecError {}

impl<T> NonEmptyVec<T>
where
    T: Clone,
{
    pub fn new(items: Vec<T>) -> Result<Self, NonEmptyVecError> {
        if items.is_empty() {
            Err(NonEmptyVecError)
        } else {
            Ok(Self { inner: items })
        }
    }
    pub fn first(&self) -> &T {
        &self.inner[0]
    }
    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T> TryFrom<Vec<T>> for NonEmptyVec<T>
where
    T: Clone,
{
    type Error = NonEmptyVecError;
    fn try_from(v: Vec<T>) -> Result<Self, Self::Error> {
        NonEmptyVec::new(v)
    }
}

impl<T> From<NonEmptyVec<T>> for Vec<T>
where
    T: Clone,
{
    fn from(v: NonEmptyVec<T>) -> Vec<T> {
        v.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }
    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }
    fn touches(items: &[(&str, TouchKind)]) -> Vec<PlanTouchInput> {
        items
            .iter()
            .map(|(p, k)| PlanTouchInput {
                plan: plan(p),
                kind: *k,
            })
            .collect()
    }

    fn ev(sha_hex: &str, ts: i64, subject: &str, plan_touches: Vec<PlanTouchInput>) -> CommitEvent {
        CommitEvent {
            sha: sha(sha_hex),
            author_ts: ts,
            subject: subject.into(),
            plan_touches,
            has_code_changes: false,
        }
    }

    // ============ Classifier ============

    #[test]
    fn explicit_known_prefix_sets_attribution() {
        let known: BTreeSet<PlanKey> = std::iter::once(plan("foo")).collect();
        let touches = std::iter::once((plan("foo"), TouchKind::Revise)).collect();
        let out = classify(ClassifierInputs {
            subject: "[foo] revise",
            touches: &touches,

            has_code_changes: false,
            active_plan_hint: None,
            known_plans: &known,
        });
        assert_eq!(out.plan_attribution, [plan("foo")].into_iter().collect());
        assert_eq!(out.next_active_plan_hint, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn unknown_prefix_warns_and_clears_attribution() {
        let known: BTreeSet<PlanKey> = std::iter::once(plan("other")).collect();
        let out = classify(ClassifierInputs {
            subject: "[ghost] random",
            touches: &BTreeMap::new(),

            has_code_changes: true,
            active_plan_hint: None,
            known_plans: &known,
        });
        assert!(out.plan_attribution.is_empty());
        assert!(matches!(
            out.warnings.as_slice(),
            [Warning::UnknownPlanPrefix { unknown_names }]
                if unknown_names == &vec!["ghost".to_string()]
        ));
    }

    #[test]
    fn misc_prefix_preserves_hint() {
        let known: BTreeSet<PlanKey> = std::iter::once(plan("foo")).collect();
        let hint = plan("foo");
        let out = classify(ClassifierInputs {
            subject: "[misc] one-off",
            touches: &BTreeMap::new(),

            has_code_changes: true,
            active_plan_hint: Some(&hint),
            known_plans: &known,
        });
        assert!(out.plan_attribution.is_empty());
        assert_eq!(out.next_active_plan_hint, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn multi_plan_prefix_sets_attribution_set_preserves_hint() {
        let known: BTreeSet<PlanKey> = [plan("foo"), plan("bar")].into_iter().collect();
        let hint = plan("baz");
        let touches = [
            (plan("foo"), TouchKind::Revise),
            (plan("bar"), TouchKind::Revise),
        ]
        .into_iter()
        .collect();
        let out = classify(ClassifierInputs {
            subject: "[foo,bar] cross-cut",
            touches: &touches,

            has_code_changes: false,
            active_plan_hint: Some(&hint),
            known_plans: &known,
        });
        assert_eq!(
            out.plan_attribution,
            [plan("foo"), plan("bar")].into_iter().collect()
        );
        // Multi-plan is transparent to the hint.
        assert_eq!(out.next_active_plan_hint, Some(plan("baz")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn no_prefix_single_touch_infers_attribution() {
        let known: BTreeSet<PlanKey> = std::iter::once(plan("foo")).collect();
        let touches = std::iter::once((plan("foo"), TouchKind::Intro)).collect();
        let out = classify(ClassifierInputs {
            subject: "introduce foo",
            touches: &touches,

            has_code_changes: false,
            active_plan_hint: None,
            known_plans: &known,
        });
        assert_eq!(out.plan_attribution, [plan("foo")].into_iter().collect());
        assert_eq!(out.next_active_plan_hint, Some(plan("foo")));
        assert!(matches!(
            out.warnings.as_slice(),
            [Warning::MissingPrefix { suggested_prefix }]
                if suggested_prefix == "[foo]"
        ));
    }

    #[test]
    fn no_prefix_inherits_hint_for_code() {
        let known: BTreeSet<PlanKey> = std::iter::once(plan("foo")).collect();
        let hint = plan("foo");
        let out = classify(ClassifierInputs {
            subject: "implement",
            touches: &BTreeMap::new(),

            has_code_changes: true,
            active_plan_hint: Some(&hint),
            known_plans: &known,
        });
        assert_eq!(out.plan_attribution, [plan("foo")].into_iter().collect());
        assert_eq!(out.next_active_plan_hint, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    // ============ apply_commit ============

    #[test]
    fn intro_inserts_plan_and_clears_ad_hoc() {
        let mut s = RepoState::new();
        s.ad_hoc.push(AdHocEvent {
            sha: sha("aaaa"),
            ts: 1,
            touched_code: true,
        });
        s.apply_commit(&ev(
            "bbbb",
            2,
            "[foo] introduce",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        assert!(s.ad_hoc.is_empty());
        assert!(s.plans.contains_key(&plan("foo")));
        assert_eq!(s.plans[&plan("foo")].commits.len(), 1);
        assert!(s.plans[&plan("foo")].commits[0].touched_plan);
        assert!(!s.plans[&plan("foo")].commits[0].touched_code);
        assert_eq!(s.active_plan_hint, Some(plan("foo")));
    }

    #[test]
    fn revise_appends_event() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "[foo] revise",
            touches(&[("foo", TouchKind::Revise)]),
        ));
        let p = &s.plans[&plan("foo")];
        assert_eq!(p.commits.len(), 2);
        assert!(p.commits[1].touched_plan);
    }

    #[test]
    fn delete_hard_forgets() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        // Plant a finished_plans entry for foo (simulating prior cycle).
        s.finished_plans.push(FinishedPlan {
            plan: plan("foo"),
            intro: sha("0000"),
            finalized_at: sha("0001"),
        });
        // Plant a warning tagged with foo.
        s.warnings.push(RepoWarning {
            sha: sha("0002"),
            plan: Some(plan("foo")),
            warning: Warning::DanglingPlanRef { plan: plan("foo") },
        });
        // Delete commit.
        s.apply_commit(&ev(
            "3333",
            3,
            "[foo] delete",
            touches(&[("foo", TouchKind::Delete)]),
        ));
        assert!(!s.plans.contains_key(&plan("foo")));
        assert!(s.finished_plans.is_empty());
        assert!(s.warnings.is_empty());
        assert!(s.active_plan_hint.is_none());
    }

    #[test]
    fn finish_moves_to_finished_plans() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "Finish foo",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        assert!(!s.plans.contains_key(&plan("foo")));
        assert_eq!(s.finished_plans.len(), 1);
        assert_eq!(s.finished_plans[0].plan, plan("foo"));
        assert_eq!(s.finished_plans[0].intro.as_str(), sha("1111").as_str());
        assert_eq!(
            s.finished_plans[0].finalized_at.as_str(),
            sha("2222").as_str()
        );
    }

    #[test]
    fn finish_without_prior_intro_still_archives() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "squashed plan",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        assert!(s.plans.is_empty());
        assert_eq!(s.finished_plans.len(), 1);
        assert_eq!(s.finished_plans[0].plan, plan("foo"));
        assert_eq!(s.finished_plans[0].intro.as_str(), sha("1111").as_str());
    }

    #[test]
    fn active_plan_hint_sanitized_after_finish() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        assert_eq!(s.active_plan_hint, Some(plan("foo")));
        s.apply_commit(&ev(
            "2222",
            2,
            "Finish foo",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        assert!(s.active_plan_hint.is_none());
    }

    #[test]
    fn ad_hoc_eligible_commit_lands_in_ad_hoc() {
        // Adoption required before AdHocs surface.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "0001",
            0,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "0002",
            0,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        let mut e = ev("1111", 1, "[misc] fix typo", Vec::new());
        e.has_code_changes = true;
        s.apply_commit(&e);
        assert_eq!(s.ad_hoc.len(), 1);
    }

    #[test]
    fn plan_commit_supersedes_adhoc() {
        // Adoption first; then finalize so the active-plan hint
        // is cleared (otherwise a no-prefix code commit would
        // inherit it and classify as PlanCommit, not AdHoc).
        // Then a `[misc]` commit lands in ad_hoc, and a fresh
        // plan touch supersedes it.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        let mut e = ev("3333", 3, "[misc] fix typo", Vec::new());
        e.has_code_changes = true;
        s.apply_commit(&e);
        assert_eq!(s.ad_hoc.len(), 1);

        s.apply_commit(&ev(
            "4444",
            4,
            "[bar] intro",
            touches(&[("bar", TouchKind::Intro)]),
        ));
        assert!(s.ad_hoc.is_empty(), "plan commit should clear ad-hoc");
    }

    #[test]
    fn adhoc_suppressed_before_first_plan_intro() {
        // Fold three plain code commits then a plan intro.
        // The first three must NOT emit AdHoc events; only the
        // intro lands in the log.
        let mut s = RepoState::new();
        let mut events = Vec::new();
        for (i, sha) in ["1111", "2222", "3333"].iter().enumerate() {
            let mut e = ev(sha, (i + 1) as i64, "fix something", Vec::new());
            e.has_code_changes = true;
            events.extend(s.apply_commit(&e));
        }
        events.extend(s.apply_commit(&ev(
            "4444",
            4,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        )));
        assert!(
            !events.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "no AdHoc events before adoption; got {events:?}"
        );
        assert!(
            s.ad_hoc.is_empty(),
            "ad_hoc bucket stays empty pre-adoption"
        );
        assert!(matches!(events.last(), Some(LogEvent::PlanIntro { .. })));
        assert!(s.adopted);
    }

    #[test]
    fn adhoc_emitted_after_first_plan_intro() {
        // After adoption + finish (to clear the active-plan
        // hint), a `[misc]` code commit emits AdHoc. A
        // no-prefix code commit would inherit the active hint
        // and classify as PlanCommit instead.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        let mut e = ev("3333", 3, "[misc] drive-by fix", Vec::new());
        e.has_code_changes = true;
        let events = s.apply_commit(&e);
        assert!(
            events.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "post-adoption [misc] commit should emit AdHoc; got {events:?}"
        );
        assert_eq!(s.ad_hoc.len(), 1);
    }

    #[test]
    fn adhoc_emitted_again_after_plan_finalized() {
        // intro -> finish -> `[misc]` code commit. Repo is
        // still adopted (finished_plans has the entry).
        // Finish also clears the active-plan hint so the
        // misc commit doesn't inherit foo.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        let mut e = ev("3333", 3, "[misc] drive-by fix", Vec::new());
        e.has_code_changes = true;
        let events = s.apply_commit(&e);
        assert!(
            events.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "post-finalize [misc] commit should emit AdHoc; got {events:?}"
        );
    }

    #[test]
    fn adhoc_emitted_after_plan_deleted_then_plain_commit() {
        // Codex's flag: intro -> delete -> `[misc]` commit.
        // After PlanDeleted, `plans` is empty AND
        // `finished_plans` is empty for this plan — but
        // `adopted` is durable, so the misc commit still
        // emits AdHoc. PlanDeleted also clears the
        // active-plan hint so we don't inherit foo.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        s.apply_commit(&ev(
            "2222",
            2,
            "[foo] delete",
            touches(&[("foo", TouchKind::Delete)]),
        ));
        assert!(s.plans.is_empty());
        assert!(s.finished_plans.is_empty());
        assert!(s.adopted, "adoption must persist across PlanDeleted");
        let mut e = ev("3333", 3, "[misc] drive-by fix", Vec::new());
        e.has_code_changes = true;
        let events = s.apply_commit(&e);
        assert!(
            events.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "[misc] commit after PlanDeleted must still emit AdHoc; got {events:?}"
        );
    }

    #[test]
    fn plan_revision_supersedes_adhoc() {
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        let mut e = ev("2222", 2, "[misc] fix", Vec::new());
        e.has_code_changes = true;
        s.apply_commit(&e);
        assert_eq!(s.ad_hoc.len(), 1, "misc commit should be ad-hoc");

        s.apply_commit(&ev(
            "3333",
            3,
            "[foo] revise",
            touches(&[("foo", TouchKind::Revise)]),
        ));
        assert!(s.ad_hoc.is_empty(), "plan revision should clear ad-hoc");
    }

    #[test]
    fn warnings_persisted_with_correct_tag() {
        let mut s = RepoState::new();
        // Bare code-only commit with a non-existent prefix.
        let mut e = ev("1111", 1, "[ghost] random", Vec::new());
        e.has_code_changes = true;
        s.apply_commit(&e);
        assert_eq!(s.warnings.len(), 1);
        assert!(matches!(
            s.warnings[0].warning,
            Warning::UnknownPlanPrefix { .. }
        ));
        assert!(s.warnings[0].plan.is_none());
    }

    #[test]
    fn warning_mentions_plan_payload_aware() {
        let w = Warning::AttributionMismatch {
            attributed: vec![plan("foo")],
            touched: vec![plan("bar")],
        };
        assert!(warning_mentions_plan(&w, &plan("foo")));
        assert!(warning_mentions_plan(&w, &plan("bar")));
        assert!(!warning_mentions_plan(&w, &plan("baz")));
    }
}
