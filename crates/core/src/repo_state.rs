//! Sans-io fold state for one repo.
//!
//! `RepoState` carries active workflow facts only — finished plans
//! are summarized in `finished_plans`, deleted plans are
//! hard-forgotten. Ad-hoc commits live in `ad_hoc` until the next
//! plan intro clears them. A commit is attributed to a plan by its
//! `[plan]` tag or by touching the plan file — nothing is inherited.
//! `warnings` holds the few facts the fold notices but doesn't act on
//! (currently `DanglingPlanRef`).
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
    /// SHA Y." Body at freeze / continuer count / full historical
    /// commit timeline are recoverable via `git_io`. `PlanKey` may
    /// repeat (a plan can be re-introduced and re-finalized).
    pub finished_plans: Vec<FinishedPlan>,

    /// Out-of-plan commits in fold order. Cleared on the next plan
    /// intro.
    pub ad_hoc: Vec<AdHocEvent>,

    /// Warnings the fold notices but doesn't act on (currently just
    /// `DanglingPlanRef`, surfaced by projection). The classifier
    /// itself no longer emits warnings — prefix ambiguity is caught
    /// live at HEAD, not recorded against history.
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

    /// First commit that made [`adopted`](Self::adopted) true. This is
    /// durable even when every plan is later deleted, and gives history
    /// readers an exact floor: commits before it are plain Git history and
    /// never need to enter the clank fold.
    #[serde(default)]
    pub adopted_at: Option<CommitSha>,
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
    /// wait, feedback write, preview builders) should call this
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
    /// True iff this commit had non-plan, non-clank code changes
    /// attributed to this plan via its `[plan]` tag.
    pub touched_code: bool,
}

/// Entry in `RepoState.finished_plans`. Stores plan identity plus
/// the two boundary SHAs (intro = first commit of this instance,
/// finalized_at = commit whose tree fired the freeze predicate).
/// Body / continuer count / full historical timeline are recoverable
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
    /// The plan a plan-scoped warning is about (`DanglingPlanRef` is
    /// always plan-scoped). `None` is reserved for any future
    /// repo-scoped warning.
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
    /// Emitted by projection helpers when a `plan_attribution`
    /// (or `touches` key) names a plan that no longer exists in
    /// `state.plans`. Not a fold-time warning per se — the
    /// classifier doesn't know about future deletions; projection
    /// surfaces it on each affected commit.
    ///
    /// (Prefix-ambiguity warnings — unknown/missing/mismatched tags —
    /// were removed with the classifier collapse: history is tolerated
    /// silently and mistakes are caught live at HEAD by wait, not
    /// retroactively. adhoc-commits-and-plan-tag-validation.)
    DanglingPlanRef { plan: PlanKey },
}

// ============================================================
// LogEvent — fold output (side-channel for `clank log`)
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogEvent {
    PlanIntro {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
        /// Commit subject, carried from the fold input so renderers
        /// (clank log --oneline, the status TUI log pane) never
        /// shell `git log -1` per event (status-tui-live-log).
        subject: String,
    },
    PlanCommit {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
        touched_plan: bool,
        touched_code: bool,
        /// See `PlanIntro::subject`.
        subject: String,
    },
    PlanFinalized {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
        /// The finish commit's real subject — a whole-plan summary now that
        /// finish messages are mandatory (not `"finish"`). `#[serde(default)]`
        /// for back-compat with pre-subject cache checkpoints; the
        /// `CACHE_FORMAT_VERSION` bump forces a one-time re-fold so existing
        /// finishes populate it (renderers fall back to a synthesized label
        /// when it's still empty).
        #[serde(default)]
        subject: String,
    },
    PlanDeleted {
        plan: PlanKey,
        sha: CommitSha,
        ts: i64,
    },
    AdHoc {
        sha: CommitSha,
        ts: i64,
        /// See `PlanIntro::subject`.
        subject: String,
    },
}

impl LogEvent {
    /// The plan this event belongs to, or `None` for ad-hoc.
    pub fn plan(&self) -> Option<&PlanKey> {
        match self {
            LogEvent::PlanIntro { plan, .. }
            | LogEvent::PlanCommit { plan, .. }
            | LogEvent::PlanFinalized { plan, .. }
            | LogEvent::PlanDeleted { plan, .. } => Some(plan),
            LogEvent::AdHoc { .. } => None,
        }
    }
}

/// What groups a run of timeline events into one umbrella: the
/// plan they belong to, or the ad-hoc bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmbrellaKey {
    Plan(PlanKey),
    AdHoc,
}

pub fn umbrella_key(event: &LogEvent) -> UmbrellaKey {
    match event.plan() {
        Some(p) => UmbrellaKey::Plan(p.clone()),
        None => UmbrellaKey::AdHoc,
    }
}

/// THE umbrella rule, shared by every timeline renderer (html,
/// `clank log --oneline`, the status TUI log pane): group
/// CONTIGUOUS events into one section per plan. A plan interrupted
/// by ANOTHER plan's commit opens a NEW umbrella — chronology is
/// never reordered, so interleaved `A1 B1 A2` yields THREE sections
/// (A, B, A), not a merged A.
///
/// Ad-hoc commits (no `[plan]` tag) have NO umbrella of their own:
/// they FOLD into the CHRONOLOGICALLY-PRECEDING plan's run (the plan
/// active when the ad-hoc landed), and the renderer marks them per-row
/// (the `~` ad-hoc marker) so they read inline rather than as a
/// segregated "adhoc" block (adhoc-commit-marker). An ad-hoc older
/// than every plan in the window has nothing to fold into and opens
/// its own (header-less) section.
///
/// `newest_first` declares the caller's EVENT ORDER explicitly — the
/// fold uses stream position, NOT author timestamps (which are
/// second-resolution and can tie or go backwards), so it is correct
/// even when timestamps are equal. `clank log` / `status` / `html`
/// pass `true` (git-log convention); pass `false` for an oldest-first
/// stream. Sections (and events within them) come out in the caller's
/// order.
pub fn umbrella_sections<'a>(
    events: &[&'a LogEvent],
    newest_first: bool,
) -> Vec<(UmbrellaKey, Vec<&'a LogEvent>)> {
    let mut out: Vec<(UmbrellaKey, Vec<&'a LogEvent>)> = Vec::new();
    if newest_first {
        // The chronologically-preceding plan appears AFTER an ad-hoc in
        // a newest-first stream, so buffer ad-hocs and flush them into
        // the NEXT plan run (newest within that run). Trailing ad-hocs
        // (older than every plan) have nothing to fold into → own run.
        let mut pending: Vec<&'a LogEvent> = Vec::new();
        for e in events {
            let key = umbrella_key(e);
            if key == UmbrellaKey::AdHoc {
                pending.push(e);
                continue;
            }
            // A finish commit CLOSES its plan. In newest-first order it is the
            // plan's chronologically-newest event, so any pending ad-hoc
            // (which appears BEFORE it in the stream) is NEWER than the finish
            // — it landed AFTER the plan finished and must NOT fold into it.
            // Flush the pending ad-hocs as their own header-less section first.
            if matches!(e, LogEvent::PlanFinalized { .. }) && !pending.is_empty() {
                out.push((UmbrellaKey::AdHoc, std::mem::take(&mut pending)));
            }
            let same = matches!(out.last(), Some((k, _)) if *k == key);
            if !same {
                out.push((key, Vec::new()));
            }
            let run = &mut out.last_mut().unwrap().1;
            run.append(&mut pending);
            run.push(e);
        }
        if !pending.is_empty() {
            out.push((UmbrellaKey::AdHoc, pending));
        }
    } else {
        // Oldest-first: the preceding plan is the current run, so an
        // ad-hoc simply joins it.
        for e in events {
            let key = umbrella_key(e);
            match out.last_mut() {
                Some((_, run)) if key == UmbrellaKey::AdHoc => run.push(e),
                Some((k, run)) if *k == key => run.push(e),
                _ => out.push((key, vec![e])),
            }
        }
    }
    out
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

/// Classifier inputs: a commit's subject + the plans known BEFORE the
/// commit is applied. The tag is the only signal — no touches, no
/// active-plan hint (adhoc-commits-and-plan-tag-validation).
#[derive(Debug, Clone)]
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    pub known_plans: &'a BTreeSet<PlanKey>,
}

/// Classifier output: which plans the commit's `[tag]` attributes it
/// to (the known subset; empty when untagged or the tag names no
/// known plan). `apply_commit` unions this with the commit's touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierOutput {
    pub plan_attribution: BTreeSet<PlanKey>,
}

/// Title-prefix vocabulary: `[plan-a]` / `[plan-a,plan-b]` names the
/// plan(s) a commit is attributed to. There is no special `[misc]` —
/// the ad-hoc opt-in is simply NOT tagging (adhoc-commits-and-plan-tag-
/// validation); `[misc]` parses as a one-element plan list that won't
/// match a real plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitlePrefix {
    Plans(Vec<String>),
}

/// A commit subject parsed into its `[prefix] body` parts.
/// `body` is the text after a recognized prefix (with the
/// single separating space consumed); when no prefix matched,
/// `body` is the raw subject verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSubject<'a> {
    pub prefix: Option<TitlePrefix>,
    pub body: &'a str,
}

/// Pure subject parser. Splits a commit subject into its
/// `[plan,...] | [misc]` prefix and the remaining body. Both
/// renderers and the classifier go through this so prefix
/// vocabulary lives in one place.
pub fn parse_subject(subject: &str) -> ParsedSubject<'_> {
    let trimmed_offset = subject.len() - subject.trim_start().len();
    let trimmed = &subject[trimmed_offset..];
    let Some(rest) = trimmed.strip_prefix('[') else {
        return ParsedSubject {
            prefix: None,
            body: subject,
        };
    };
    let Some(close) = rest.find(']') else {
        return ParsedSubject {
            prefix: None,
            body: subject,
        };
    };
    let inner = &rest[..close];
    if inner.is_empty() {
        return ParsedSubject {
            prefix: None,
            body: subject,
        };
    }
    let names: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return ParsedSubject {
            prefix: None,
            body: subject,
        };
    }
    let prefix = TitlePrefix::Plans(names);
    // body starts after `[<inner>]`. Consume one separating
    // space if present so callers don't have to.
    let after = &rest[close + 1..];
    let body = after.strip_prefix(' ').unwrap_or(after);
    ParsedSubject {
        prefix: Some(prefix),
        body,
    }
}

/// Pure prefix parser. Thin wrapper over [`parse_subject`]
/// kept for callers that only need the prefix.
pub fn parse_title_prefix(subject: &str) -> Option<TitlePrefix> {
    parse_subject(subject).prefix
}

/// Pure classifier. Computes `plan_attribution` (a set, possibly
/// empty) from the title prefix and active-plan hint; emits
/// warnings for unknown / mismatched / missing prefixes;
/// emits no warnings (history is tolerated; mistakes are caught live
/// at HEAD by wait — adhoc-commits-and-plan-tag-validation).
///
/// The tag is the ONLY attribution signal: `[a,b,…]` → the subset of
/// the named plans that are known; an untagged commit or a tag naming
/// no known plan → `∅`. `apply_commit` then unions this with the
/// commit's touches (plan-file edits attribute on their own).
pub fn classify(inputs: ClassifierInputs<'_>) -> ClassifierOutput {
    let plan_attribution: BTreeSet<PlanKey> = match parse_title_prefix(inputs.subject) {
        Some(TitlePrefix::Plans(names)) => names
            .iter()
            .filter_map(|n| PlanKey::parse(n).ok())
            .filter(|k| inputs.known_plans.contains(k))
            .collect(),
        None => BTreeSet::new(),
    };
    ClassifierOutput { plan_attribution }
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
        let was_adopted = self.adopted;
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
            known_plans: &known_plans,
        });

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
                        subject: event.subject.clone(),
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
                }
                TouchKind::Finish => {
                    finished_this_commit.insert(touch.plan.clone());
                }
            }
        }

        // Attribution sources: `touches` (the diff edited
        // `.clank/plans/<x>.md` — objective) and `plan_attribution`
        // (the `[..]` tag — author claim). These are ORTHOGONAL facts,
        // not a contradictory dual source: `touched_plan` records the
        // file edit, `touched_code` records tag-attributed code. The
        // HEAD commit-tag invariant
        // (`commit-tag-fixup-is-first-class-state`) now forces the two
        // to AGREE at HEAD — a tag that names a different plan than the
        // diff touched is a dominating `MasterToFixCommitTag`
        // correction in `derive_status`, caught before reviewability
        // matters. The fold itself is over ALL history, which is
        // deliberately NOT policed (HEAD-only semantics), so a
        // historical `[b]`-tagged commit that touched `a.md` must still
        // land on BOTH timelines as it did before — collapsing the
        // union here would silently rewrite tolerated history. The
        // collapse is therefore enforced at the HEAD gate, not in the
        // fold (see the plan's "leave a clear note" escape hatch).
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
                    subject: event.subject.clone(),
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
                subject: event.subject.clone(),
            });
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
                subject: event.subject.clone(),
            });
        }

        if !was_adopted && self.adopted && self.adopted_at.is_none() {
            self.adopted_at = Some(event.sha.clone());
        }

        log_events
    }
}

/// Returns `Some(plan)` when the warning is plan-scoped.
pub fn warning_plan_tag(w: &Warning) -> Option<PlanKey> {
    match w {
        Warning::DanglingPlanRef { plan } => Some(plan.clone()),
    }
}

/// True iff `warning` directly references `key` in its payload —
/// used by the `Delete` hard-forget path so that wiping a key
/// removes every warning that mentions it.
pub fn warning_mentions_plan(warning: &Warning, key: &PlanKey) -> bool {
    match warning {
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
    //
    // The tag is the ONLY signal: `[X]` → the known subset of X; an
    // untagged commit or a tag naming no known plan → ∅. No warnings,
    // no hint, no `[misc]` special case (adhoc-commits-and-plan-tag-
    // validation).

    fn classify_with(subject: &str, known: &[&str]) -> BTreeSet<PlanKey> {
        let known: BTreeSet<PlanKey> = known.iter().map(|k| plan(k)).collect();
        classify(ClassifierInputs {
            subject,
            known_plans: &known,
        })
        .plan_attribution
    }

    #[test]
    fn known_prefix_attributes_to_that_plan() {
        assert_eq!(
            classify_with("[foo] revise", &["foo"]),
            [plan("foo")].into_iter().collect()
        );
    }

    #[test]
    fn unknown_prefix_is_unattributed_no_warning() {
        // `[ghost]` names no known plan → ∅ (ad-hoc). History is
        // tolerated silently; the live HEAD nag catches it.
        assert!(classify_with("[ghost] random", &["other"]).is_empty());
    }

    #[test]
    fn misc_is_not_special_just_an_unknown_tag() {
        // No `[misc]` carve-out — `misc` isn't a plan, so it's
        // unattributed like any other unknown tag.
        assert!(classify_with("[misc] one-off", &["foo"]).is_empty());
    }

    #[test]
    fn multi_plan_prefix_attributes_the_known_subset() {
        assert_eq!(
            classify_with("[foo,bar] cross-cut", &["foo", "bar"]),
            [plan("foo"), plan("bar")].into_iter().collect()
        );
        // Unknown members are dropped, not attributed.
        assert_eq!(
            classify_with("[foo,ghost] cross-cut", &["foo"]),
            [plan("foo")].into_iter().collect()
        );
    }

    #[test]
    fn no_prefix_is_unattributed() {
        // No tag → ad-hoc. No touches inference, no hint inheritance.
        assert!(classify_with("implement a thing", &["foo"]).is_empty());
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
        assert_eq!(s.adopted_at, Some(sha("4444")));
    }

    #[test]
    fn untagged_code_commit_is_adhoc_not_inherited() {
        // adhoc-commits-and-plan-tag-validation: the tag is the ONLY
        // plan association — an untagged code commit after a plan intro
        // is AD-HOC, never inherited via an active-plan hint.
        // Reproduce-first: today this inherits `foo` and folds to
        // PlanCommit (no AdHoc), so this test fails until the
        // classifier collapse lands.
        let mut s = RepoState::new();
        let _ = s.apply_commit(&ev(
            "1111",
            1,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        let mut code = ev("2222", 2, "fix a thing", Vec::new());
        code.has_code_changes = true;
        let events = s.apply_commit(&code);
        assert!(
            events.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "untagged code commit must be ad-hoc; got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LogEvent::PlanCommit { .. })),
            "untagged code commit must NOT inherit the active plan; got {events:?}"
        );
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
        let adopted_at = s.adopted_at.clone();
        let mut e = ev("3333", 3, "[misc] drive-by fix", Vec::new());
        e.has_code_changes = true;
        let events = s.apply_commit(&e);
        assert_eq!(s.adopted_at, adopted_at, "adoption boundary is durable");
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
    fn unknown_prefix_emits_no_fold_warning() {
        // The classifier no longer warns on prefix ambiguity — a
        // `[ghost]` code commit just folds to ad-hoc, silently
        // (adhoc-commits-and-plan-tag-validation; the live HEAD nag
        // catches it). Adopt first so the ad-hoc would surface.
        let mut s = RepoState::new();
        s.apply_commit(&ev(
            "0001",
            0,
            "[foo] intro",
            touches(&[("foo", TouchKind::Intro)]),
        ));
        let mut e = ev("1111", 1, "[ghost] random", Vec::new());
        e.has_code_changes = true;
        s.apply_commit(&e);
        assert!(s.warnings.is_empty(), "no fold warning on an unknown tag");
        assert_eq!(s.ad_hoc.len(), 1, "unknown-tag code commit is ad-hoc");
    }

    #[test]
    fn warning_mentions_plan_payload_aware() {
        let w = Warning::DanglingPlanRef { plan: plan("foo") };
        assert!(warning_mentions_plan(&w, &plan("foo")));
        assert!(!warning_mentions_plan(&w, &plan("bar")));
    }

    // ============ parse_subject ============

    #[test]
    fn parse_subject_single_plan() {
        let p = parse_subject("[foo] intro");
        assert_eq!(p.prefix, Some(TitlePrefix::Plans(vec!["foo".into()])));
        assert_eq!(p.body, "intro");
    }

    #[test]
    fn parse_subject_multi_plan() {
        let p = parse_subject("[foo,bar] shared work");
        assert_eq!(
            p.prefix,
            Some(TitlePrefix::Plans(vec!["foo".into(), "bar".into()]))
        );
        assert_eq!(p.body, "shared work");
    }

    #[test]
    fn parse_subject_misc_is_just_a_plan_name() {
        // No special `[misc]` — it parses as a one-element plan list
        // (that won't match a real plan, so it's ad-hoc).
        let p = parse_subject("[misc] drive-by");
        assert_eq!(p.prefix, Some(TitlePrefix::Plans(vec!["misc".into()])));
        assert_eq!(p.body, "drive-by");
    }

    #[test]
    fn parse_subject_no_prefix() {
        let p = parse_subject("plain commit");
        assert!(p.prefix.is_none());
        assert_eq!(p.body, "plain commit");
    }

    #[test]
    fn parse_subject_empty_brackets() {
        let p = parse_subject("[] body");
        assert!(p.prefix.is_none());
        assert_eq!(p.body, "[] body");
    }

    #[test]
    fn parse_subject_no_close_bracket() {
        let p = parse_subject("[unterminated body");
        assert!(p.prefix.is_none());
        assert_eq!(p.body, "[unterminated body");
    }

    #[test]
    fn parse_subject_leading_whitespace_preserves_body_offset() {
        // Leading whitespace stays in body when no prefix.
        let p = parse_subject("  plain");
        assert!(p.prefix.is_none());
        assert_eq!(p.body, "  plain");
    }

    #[test]
    fn parse_subject_consumes_one_separating_space() {
        // Only one space is consumed between `]` and body.
        let p = parse_subject("[foo]  double-space");
        assert_eq!(p.prefix, Some(TitlePrefix::Plans(vec!["foo".into()])));
        assert_eq!(p.body, " double-space");
    }

    #[test]
    fn parse_subject_no_body() {
        let p = parse_subject("[foo]");
        assert_eq!(p.prefix, Some(TitlePrefix::Plans(vec!["foo".into()])));
        assert_eq!(p.body, "");
    }

    #[test]
    fn parse_title_prefix_still_works() {
        // Backwards-compat wrapper.
        assert_eq!(
            parse_title_prefix("[foo] x"),
            Some(TitlePrefix::Plans(vec!["foo".into()]))
        );
        assert_eq!(
            parse_title_prefix("[misc] x"),
            Some(TitlePrefix::Plans(vec!["misc".into()]))
        );
        assert_eq!(parse_title_prefix("plain"), None);
    }

    // ── umbrella_sections (log-plan-umbrellas) ──

    fn log_ev(plan: Option<&str>, n: u8) -> LogEvent {
        let sha = CommitSha::parse(&format!("{n:0<40x}")).unwrap();
        match plan {
            Some(p) => LogEvent::PlanCommit {
                plan: PlanKey::parse(p).unwrap(),
                sha,
                ts: n as i64,
                touched_plan: false,
                touched_code: true,
                subject: format!("[{p}] c{n}"),
            },
            None => LogEvent::AdHoc {
                sha,
                ts: n as i64,
                subject: format!("adhoc c{n}"),
            },
        }
    }

    fn log_finish(plan: &str, n: u8) -> LogEvent {
        LogEvent::PlanFinalized {
            plan: PlanKey::parse(plan).unwrap(),
            sha: CommitSha::parse(&format!("{n:0<40x}")).unwrap(),
            ts: n as i64,
            subject: format!("[{plan}] wrap up"),
        }
    }

    #[test]
    fn umbrella_sections_adhoc_after_finish_is_its_own_section() {
        // Chronological: intro(a), finish(a), then ad-hoc X. X landed AFTER
        // a finished, so it must NOT fold under a's umbrella — it opens its
        // own header-less section. Newest-first stream (log/status feed):
        // X, finish(a), intro(a).
        let intro = log_ev(Some("a"), 1);
        let finish = log_finish("a", 2);
        let x = log_ev(None, 3);
        let s = umbrella_sections(&[&x, &finish, &intro], true);
        assert_eq!(s.len(), 2, "post-finish adhoc is its own section: {s:?}");
        assert_eq!(s[0].0, UmbrellaKey::AdHoc);
        assert!(matches!(&s[0].1[..], [e] if matches!(e, LogEvent::AdHoc { .. })));
        assert_eq!(s[1].0, UmbrellaKey::Plan(PlanKey::parse("a").unwrap()));
        assert!(
            !s[1].1.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "no adhoc folded under the finished plan"
        );
    }

    #[test]
    fn umbrella_sections_adhoc_before_finish_still_folds_in() {
        // An ad-hoc BEFORE the finish (while the plan was active) still folds
        // into the plan: chronological intro(a), X, finish(a); newest-first
        // stream finish(a), X, intro(a). Only the absorbing FINISH is special.
        let intro = log_ev(Some("a"), 1);
        let x = log_ev(None, 2);
        let finish = log_finish("a", 3);
        let s = umbrella_sections(&[&finish, &x, &intro], true);
        assert_eq!(s.len(), 1, "adhoc during an active plan folds in: {s:?}");
        assert_eq!(s[0].0, UmbrellaKey::Plan(PlanKey::parse("a").unwrap()));
        assert!(
            s[0].1.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "adhoc folded into the active plan"
        );
    }

    #[test]
    fn umbrella_sections_split_on_interleave_never_reorder() {
        // THE load-bearing rule (ruthless 3ea8580 concern 2):
        // A1 B1 A2 yields THREE umbrellas (A, B, A) — merging A1+A2
        // into one A-umbrella would reorder chronology.
        let a1 = log_ev(Some("a"), 1);
        let b1 = log_ev(Some("b"), 2);
        let a2 = log_ev(Some("a"), 3);
        let events = [&a1, &b1, &a2];
        let sections = umbrella_sections(&events, false);
        let keys: Vec<&UmbrellaKey> = sections.iter().map(|(k, _)| k).collect();
        assert_eq!(sections.len(), 3, "A1 B1 A2 → three umbrellas");
        assert_eq!(*keys[0], UmbrellaKey::Plan(PlanKey::parse("a").unwrap()));
        assert_eq!(*keys[1], UmbrellaKey::Plan(PlanKey::parse("b").unwrap()));
        assert_eq!(*keys[2], UmbrellaKey::Plan(PlanKey::parse("a").unwrap()));
    }

    #[test]
    fn umbrella_sections_adhoc_folds_into_surrounding_plan() {
        // adhoc-commit-marker: an ad-hoc commit has no umbrella of its
        // own — it folds into the surrounding plan's run (marked per-row
        // by the renderer), NOT a segregated AdHoc section.
        let a1 = log_ev(Some("a"), 1);
        let a2 = log_ev(Some("a"), 2);
        let x = log_ev(None, 3);
        let events = [&a1, &a2, &x];
        let sections = umbrella_sections(&events, false);
        assert_eq!(
            sections.len(),
            1,
            "ad-hoc folds into plan a, not its own section"
        );
        assert_eq!(
            sections[0].0,
            UmbrellaKey::Plan(PlanKey::parse("a").unwrap())
        );
        assert_eq!(sections[0].1.len(), 3);
    }

    #[test]
    fn umbrella_sections_leading_adhoc_opens_headerless_section() {
        // Ad-hoc with NO preceding plan has nothing to fold into, so it
        // opens its own (header-less at render) section.
        let x = log_ev(None, 1);
        let a1 = log_ev(Some("a"), 2);
        let events = [&x, &a1];
        let sections = umbrella_sections(&events, false);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, UmbrellaKey::AdHoc);
        assert_eq!(
            sections[1].0,
            UmbrellaKey::Plan(PlanKey::parse("a").unwrap())
        );
    }

    #[test]
    fn umbrella_sections_adhoc_folds_under_preceding_plan_both_directions() {
        // For chronological A -> adhoc X -> B, X folds under the
        // PRECEDING plan A (the plan active when X landed), and the
        // result is the same fed newest- or oldest-first. Direction is
        // EXPLICIT (no ts inference).
        let a = log_ev(Some("a"), 1);
        let x = log_ev(None, 2);
        let b = log_ev(Some("b"), 3);
        let a_key = UmbrellaKey::Plan(PlanKey::parse("a").unwrap());
        let has_adhoc = |run: &[&LogEvent]| run.iter().any(|e| matches!(e, LogEvent::AdHoc { .. }));

        // Newest-first (as log / status / html feed): stream is B, X, A.
        let newest = [&b, &x, &a];
        let s = umbrella_sections(&newest, true);
        assert_eq!(s.len(), 2, "B run + A run (X folded into A): {s:?}");
        let a_run = s.iter().find(|(k, _)| *k == a_key).expect("A run");
        assert!(
            has_adhoc(&a_run.1),
            "X folds under preceding plan A: {:?}",
            a_run.1
        );
        let b_run = s
            .iter()
            .find(|(k, _)| matches!(k, UmbrellaKey::Plan(p) if p.as_str() == "b"))
            .unwrap();
        assert_eq!(b_run.1.len(), 1, "B must NOT absorb the ad-hoc");

        // Oldest-first stream A, X, B → identical attribution.
        let oldest = [&a, &x, &b];
        let s2 = umbrella_sections(&oldest, false);
        let a_run2 = s2.iter().find(|(k, _)| *k == a_key).unwrap();
        assert!(has_adhoc(&a_run2.1), "oldest-first also folds X under A");
    }

    #[test]
    fn umbrella_sections_fold_uses_order_not_equal_timestamps() {
        // codex 75a84c8: author timestamps are second-resolution and can
        // tie. With ALL ts equal, a real newest-first stream B, X, A must
        // STILL fold X under the preceding plan A — the fold uses event
        // ORDER (explicit `newest_first`), never ts.
        let a = log_ev(Some("a"), 1);
        let mut x = log_ev(None, 1);
        let mut b = log_ev(Some("b"), 1);
        // Distinct shas, identical ts (log_ev keys sha off `n`).
        if let LogEvent::AdHoc { sha, .. } = &mut x {
            *sha = CommitSha::parse(&format!("{:0<40x}", 9u8)).unwrap();
        }
        if let LogEvent::PlanCommit { sha, .. } = &mut b {
            *sha = CommitSha::parse(&format!("{:0<40x}", 8u8)).unwrap();
        }
        let newest = [&b, &x, &a];
        let s = umbrella_sections(&newest, true);
        let a_run = s
            .iter()
            .find(|(k, _)| *k == UmbrellaKey::Plan(PlanKey::parse("a").unwrap()))
            .expect("A run");
        assert!(
            a_run.1.iter().any(|e| matches!(e, LogEvent::AdHoc { .. })),
            "equal-ts newest-first still folds X under A, not B: {s:?}"
        );
    }
}
