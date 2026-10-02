//! Sans-io fold state for one repo.
//!
//! `RepoState` carries active workflow facts only — finished plans
//! are summarized in `finished_plans`, deleted plans are
//! hard-forgotten. Ad-hoc commits live in `ad_hoc` until the next
//! plan intro clears them. A commit is attributed to a plan by its
//! `[plan]` tag or by touching the plan file — nothing is inherited.
//! `warnings` holds the few facts the fold notices but doesn't act on
//! including rejected introductions and their ignored-run provenance.
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

use std::collections::BTreeMap;

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
    /// The one accepted introduction in flight. Finalize or hard-forget
    /// empties this slot; an overlapping introduction cannot enter it.
    pub active_plan: Option<ActivePlan>,

    /// Ordered log of finalize events: "plan X was finalized at
    /// SHA Y." Body at freeze / continuer count / full historical
    /// commit timeline are recoverable via `git_io`. `PlanKey` may
    /// repeat (a plan can be re-introduced and re-finalized).
    pub finished_plans: Vec<FinishedPlan>,

    /// Out-of-plan commits in fold order. Cleared on the next plan
    /// intro.
    pub ad_hoc: Vec<AdHocEvent>,

    /// Projection warnings and rejected-run provenance. A rejected intro stays
    /// here until a genuine accepted intro of that name supersedes it. It must
    /// outlive the accepting plan so later finish/delete touches cannot mutate
    /// older finished history with the rejected name. No second active map exists.
    pub warnings: Vec<RepoWarning>,

    /// Has this repo ever participated in clank? Set true on the
    /// first accepted plan touch and never cleared, even when
    /// `PlanDeleted` empties the active slot without
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

/// Identity and timeline of the single accepted plan run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct ActivePlan {
    pub key: PlanKey,
    pub state: PlanState,
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
/// the accepted plan. A commit can affect only this one active timeline.
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
    /// Author-time interval from this accepted introduction to its finalize.
    /// Computed once when the fold closes the run; never an event's age.
    pub duration_secs: u64,
}

/// The one definition of a plan's duration. Git author clocks may move
/// backwards: clamp that interval to zero. i128 covers the entire difference
/// between two i64 timestamps, whose nonnegative range fits u64 exactly.
fn plan_duration_secs(introduced_at: i64, finalized_at: i64) -> u64 {
    (i128::from(finalized_at) - i128::from(introduced_at)).max(0) as u64
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
    /// The plan this warning belongs to. Both current variants are
    /// plan-scoped; `None` is reserved for any future
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
    /// `state.active_plan`. Not a fold-time warning per se — the
    /// classifier doesn't know about future deletions; projection
    /// surfaces it on each affected commit.
    ///
    /// (Prefix-ambiguity warnings — unknown/missing/mismatched tags —
    /// were removed with the classifier collapse: history is tolerated
    /// silently and mistakes are caught live at HEAD by wait, not
    /// retroactively. adhoc-commits-and-plan-tag-validation.)
    DanglingPlanRef { plan: PlanKey },
    /// This introduction was ignored. Its follow-up touches and tags
    /// remain ignored until a genuine introduction into an empty slot.
    /// This warning is scoped to the rejected run, not to the accepting
    /// plan: finishing/deleting the latter must not revive the former.
    RejectedPlanIntro {
        plan: PlanKey,
        reason: IntroRejection,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntroRejection {
    PlanAlreadyActive {
        active: PlanKey,
    },
    /// An atomic commit cannot select between several introductions.
    MultipleIntroductions,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DanglingPlanRef { plan } => {
                write!(f, "plan reference `{plan}` no longer resolves")
            }
            Self::RejectedPlanIntro { plan, reason } => {
                write!(f, "ignored introduction of `{plan}`: ")?;
                match reason {
                    IntroRejection::PlanAlreadyActive { active } => {
                        write!(f, "`{active}` was already active")
                    }
                    IntroRejection::MultipleIntroductions => {
                        write!(f, "several plans introduced in one commit")
                    }
                }
            }
        }
    }
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
        duration_secs: u64,
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
    /// When this event happened: the commit's author time.
    pub fn ts(&self) -> i64 {
        match self {
            LogEvent::PlanIntro { ts, .. }
            | LogEvent::PlanCommit { ts, .. }
            | LogEvent::PlanFinalized { ts, .. }
            | LogEvent::PlanDeleted { ts, .. }
            | LogEvent::AdHoc { ts, .. } => *ts,
        }
    }

    /// The commit this event is about.
    pub fn sha(&self) -> &CommitSha {
        match self {
            LogEvent::PlanIntro { sha, .. }
            | LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanFinalized { sha, .. }
            | LogEvent::PlanDeleted { sha, .. }
            | LogEvent::AdHoc { sha, .. } => sha,
        }
    }

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
/// contiguous events into one section per accepted run. Intro and close
/// boundaries distinguish successive runs even when a name is reused.
/// Chronology is preserved; partial or historical event streams with
/// different plan keys also remain separate.
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
            if matches!(
                e,
                LogEvent::PlanFinalized { .. } | LogEvent::PlanDeleted { .. }
            ) && !pending.is_empty()
            {
                out.push((UmbrellaKey::AdHoc, std::mem::take(&mut pending)));
            }
            let same = matches!(out.last(), Some((k, run))
                if *k == key && !matches!(run.last(), Some(LogEvent::PlanIntro { .. })));
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
                Some((k, run))
                    if key == UmbrellaKey::AdHoc
                        && (*k == UmbrellaKey::AdHoc
                            || !matches!(
                                run.last(),
                                Some(LogEvent::PlanFinalized { .. } | LogEvent::PlanDeleted { .. })
                            )) =>
                {
                    run.push(e)
                }
                Some((k, run))
                    if *k == key
                        && !matches!(
                            run.last(),
                            Some(LogEvent::PlanFinalized { .. } | LogEvent::PlanDeleted { .. })
                        ) =>
                {
                    run.push(e)
                }
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

/// Classify against the single accepted plan, including a just-accepted intro.
#[derive(Debug, Clone)]
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    pub active_plan: Option<&'a PlanKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierOutput {
    pub plan_attribution: Option<PlanKey>,
}

/// The first bracketed prefix names exactly one candidate, without splitting
/// on commas. `[a,b]` is the literal invalid name `a,b`; `[a] [b] work`
/// names `a` and leaves `[b] work` in the body. History is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitlePrefix {
    Plan(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSubject<'a> {
    pub prefix: Option<TitlePrefix>,
    pub body: &'a str,
}

pub fn parse_subject(subject: &str) -> ParsedSubject<'_> {
    let trimmed = subject.trim_start();
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
    let name = rest[..close].trim();
    if name.is_empty() {
        return ParsedSubject {
            prefix: None,
            body: subject,
        };
    }
    let body = &rest[close + 1..];
    ParsedSubject {
        prefix: Some(TitlePrefix::Plan(name.to_owned())),
        body: body.strip_prefix(' ').unwrap_or(body),
    }
}

pub fn parse_title_prefix(subject: &str) -> Option<TitlePrefix> {
    parse_subject(subject).prefix
}

/// Tags attribute code only when they identify the accepted plan. Unknown
/// tags remain tolerated in history; the HEAD gate diagnoses mistakes.
pub fn classify(inputs: ClassifierInputs<'_>) -> ClassifierOutput {
    let plan_attribution = match parse_title_prefix(inputs.subject) {
        Some(TitlePrefix::Plan(name)) => PlanKey::parse(&name)
            .ok()
            .filter(|key| inputs.active_plan == Some(key)),
        None => None,
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

    /// The finalize record for `plan`, if it has one.
    ///
    /// A name can be finished more than once (re-introduced and
    /// re-finalized), so `finished_plans` may hold several records
    /// for it. Which one a reader means has to be the SAME everywhere
    /// — a reader that picks a different record than the one whose
    /// `[intro, finalized_at]` interval it then walks reports one
    /// cycle's boundary against another's commits (codex a7f47e4).
    /// This is that choice, in one place: the first, which is what
    /// the rewrite preview walks.
    ///
    /// Says nothing about whether the plan is ALSO active — a
    /// re-introduced plan has old finished records and an active run; the latter wins.
    pub fn finished_record(&self, plan: &PlanKey) -> Option<&FinishedPlan> {
        self.finished_plans.iter().find(|f| &f.plan == plan)
    }

    /// Look up the accepted timeline by identity, including historical callers
    /// that also query finished runs. There is no alternative active container.
    pub fn plan(&self, key: &PlanKey) -> Option<&PlanState> {
        self.active_plan
            .as_ref()
            .filter(|p| &p.key == key)
            .map(|p| &p.state)
    }

    pub(crate) fn ignored_run(&self, key: &PlanKey) -> bool {
        self.plan(key).is_none()
            && self.warnings.iter().any(|w| match &w.warning {
                Warning::RejectedPlanIntro { plan, reason } if plan == key => match reason {
                    // A duplicate intro of the occupied name cannot denote another
                    // file/run. Warn about the commit, but keep the accepted identity
                    // authoritative even after it finishes (including hard-forget).
                    IntroRejection::PlanAlreadyActive { active } => active != plan,
                    IntroRejection::MultipleIntroductions => true,
                },
                _ => false,
            })
    }

    /// Shared fold/HEAD-gate disposition. Ignored work must not become either
    /// an ad-hoc review or a tag correction that pauses the accepted run.
    pub(crate) fn ignores_commit<'a>(
        &self,
        sha: &CommitSha,
        subject: &str,
        touched: impl IntoIterator<Item = &'a PlanKey>,
    ) -> bool {
        if self
            .warnings
            .iter()
            .any(|w| &w.sha == sha && matches!(w.warning, Warning::RejectedPlanIntro { .. }))
        {
            return true;
        }
        let mut had_touch = false;
        for key in touched {
            had_touch = true;
            if !self.ignored_run(key) {
                return false;
            }
        }
        if had_touch {
            return true;
        }
        match parse_title_prefix(subject) {
            Some(TitlePrefix::Plan(name)) => PlanKey::parse(&name)
                .ok()
                .is_some_and(|key| self.ignored_run(&key)),
            None => false,
        }
    }

    pub fn apply_commit(&mut self, event: &CommitEvent) -> Vec<LogEvent> {
        let mut intros = event
            .plan_touches
            .iter()
            .filter(|t| t.kind == TouchKind::Intro);
        let intro = intros.next();
        let several_intros = intros.next().is_some();
        // Reject the whole introduction commit before any lifecycle, attribution,
        // adoption or ad-hoc mutation. The warnings are also the ignored-run
        // provenance needed to protect an earlier finished run with the same name.
        let rejection = if let Some(active) = &self.active_plan {
            Some(IntroRejection::PlanAlreadyActive {
                active: active.key.clone(),
            })
        } else if several_intros {
            Some(IntroRejection::MultipleIntroductions)
        } else {
            None
        };
        if let Some(reason) = rejection {
            if intro.is_some() {
                for intro in event
                    .plan_touches
                    .iter()
                    .filter(|t| t.kind == TouchKind::Intro)
                {
                    self.warnings.push(RepoWarning {
                        sha: event.sha.clone(),
                        plan: Some(intro.plan.clone()),
                        warning: Warning::RejectedPlanIntro {
                            plan: intro.plan.clone(),
                            reason: reason.clone(),
                        },
                    });
                }
                return Vec::new();
            }
        }

        let was_adopted = self.adopted;
        let mut log = Vec::new();
        if let Some(intro) = intro {
            // A real introduction, rather than an edit/tag, starts a new run and
            // retires the previous rejection of this name. Other warnings survive.
            self.warnings.retain(|w| {
                !matches!(&w.warning,
                Warning::RejectedPlanIntro { plan, .. } if plan == &intro.plan)
            });
            self.active_plan = Some(ActivePlan {
                key: intro.plan.clone(),
                state: PlanState::default(),
            });
            self.ad_hoc.clear();
            log.push(LogEvent::PlanIntro {
                plan: intro.plan.clone(),
                sha: event.sha.clone(),
                ts: event.author_ts,
                subject: event.subject.clone(),
            });
        }

        if self.ignores_commit(
            &event.sha,
            &event.subject,
            event.plan_touches.iter().map(|t| &t.plan),
        ) {
            return log;
        }
        let touches: Vec<_> = event
            .plan_touches
            .iter()
            .filter(|t| !self.ignored_run(&t.plan))
            .collect();
        let attribution = classify(ClassifierInputs {
            subject: &event.subject,
            active_plan: self.active_plan.as_ref().map(|p| &p.key),
        })
        .plan_attribution;
        let mut affected = false;
        if let Some(active) = &self.active_plan {
            let key = active.key.clone();
            for touch in touches
                .iter()
                .filter(|t| t.kind == TouchKind::Delete && t.plan != key)
            {
                self.finished_plans.retain(|f| f.plan != touch.plan);
                self.warnings
                    .retain(|w| !warning_mentions_plan(&w.warning, &touch.plan));
                self.adopted = true;
                affected = true;
                log.push(LogEvent::PlanDeleted {
                    plan: touch.plan.clone(),
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                });
            }
        }
        if let Some(active) = &mut self.active_plan {
            let touch = touches.iter().find(|t| t.plan == active.key);
            let touched_code = event.has_code_changes && attribution.as_ref() == Some(&active.key);
            if touch.is_some() || attribution.as_ref() == Some(&active.key) {
                affected = true;
                self.adopted = true;
                let kind = touch.map(|t| t.kind);
                if kind != Some(TouchKind::Delete) {
                    active.state.commits.push(PlanTimelineEvent {
                        sha: event.sha.clone(),
                        ts: event.author_ts,
                        touched_plan: touch.is_some(),
                        touched_code,
                    });
                }
                match kind {
                    Some(TouchKind::Delete) => {
                        let key = active.key.clone();
                        self.active_plan = None;
                        self.finished_plans.retain(|f| f.plan != key);
                        self.warnings
                            .retain(|w| !warning_mentions_plan(&w.warning, &key));
                        log.push(LogEvent::PlanDeleted {
                            plan: key,
                            sha: event.sha.clone(),
                            ts: event.author_ts,
                        });
                    }
                    Some(TouchKind::Finish) => {
                        let active = self.active_plan.take().unwrap();
                        let intro = active.state.commits.first().unwrap();
                        let duration_secs = plan_duration_secs(intro.ts, event.author_ts);
                        self.finished_plans.push(FinishedPlan {
                            plan: active.key.clone(),
                            intro: intro.sha.clone(),
                            finalized_at: event.sha.clone(),
                            duration_secs,
                        });
                        log.push(LogEvent::PlanFinalized {
                            duration_secs,
                            plan: active.key,
                            sha: event.sha.clone(),
                            ts: event.author_ts,
                            subject: event.subject.clone(),
                        });
                    }
                    Some(TouchKind::Intro) => {}
                    _ => log.push(LogEvent::PlanCommit {
                        plan: active.key.clone(),
                        sha: event.sha.clone(),
                        ts: event.author_ts,
                        touched_plan: touch.is_some(),
                        touched_code,
                        subject: event.subject.clone(),
                    }),
                }
            }
        } else {
            // A squashed finished plan has no separate intro. Preserve that
            // historical boundary without ever opening a second active plan.
            for touch in &touches {
                match touch.kind {
                    TouchKind::Finish => {
                        affected = true;
                        self.adopted = true;
                        let duration_secs = plan_duration_secs(event.author_ts, event.author_ts);
                        self.finished_plans.push(FinishedPlan {
                            plan: touch.plan.clone(),
                            intro: event.sha.clone(),
                            finalized_at: event.sha.clone(),
                            duration_secs,
                        });
                        log.push(LogEvent::PlanFinalized {
                            duration_secs,
                            plan: touch.plan.clone(),
                            sha: event.sha.clone(),
                            ts: event.author_ts,
                            subject: event.subject.clone(),
                        });
                    }
                    TouchKind::Delete => {
                        affected = true;
                        self.adopted = true;
                        self.finished_plans.retain(|f| f.plan != touch.plan);
                        self.warnings
                            .retain(|w| !warning_mentions_plan(&w.warning, &touch.plan));
                        log.push(LogEvent::PlanDeleted {
                            plan: touch.plan.clone(),
                            sha: event.sha.clone(),
                            ts: event.author_ts,
                        });
                    }
                    TouchKind::Revise => {
                        affected = true;
                        self.adopted = true;
                    }
                    TouchKind::Intro => unreachable!("intro already accepted"),
                }
            }
        }
        if !affected && touches.is_empty() && self.adopted {
            if event.has_code_changes {
                self.ad_hoc.push(AdHocEvent {
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                    touched_code: true,
                });
            }
            log.push(LogEvent::AdHoc {
                sha: event.sha.clone(),
                ts: event.author_ts,
                subject: event.subject.clone(),
            });
        }
        if !was_adopted && self.adopted && self.adopted_at.is_none() {
            self.adopted_at = Some(event.sha.clone());
        }
        log
    }
}

/// True iff this warning belongs to `key`. Hard-forget clears that plan's
/// warnings, but not a different rejected run's provenance merely because
/// its diagnostic names `key` as the plan that prevented its introduction.
pub fn warning_mentions_plan(warning: &Warning, key: &PlanKey) -> bool {
    match warning {
        Warning::DanglingPlanRef { plan } | Warning::RejectedPlanIntro { plan, .. } => plan == key,
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
    /// Accepted plan the master attributed this commit to; `None` is ad-hoc.
    pub plan_attribution: Option<PlanKey>,
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

    #[test]
    fn finalized_duration_uses_the_accepted_boundaries_and_survives_name_reuse() {
        let mut state = RepoState::default();
        for event in [
            ev(
                "a1",
                100,
                "[run] intro",
                touches(&[("run", TouchKind::Intro)]),
            ),
            ev(
                "a2",
                700,
                "[run] revise",
                touches(&[("run", TouchKind::Revise)]),
            ),
            ev(
                "a3",
                90_000,
                "[ignored] intro",
                touches(&[("ignored", TouchKind::Intro)]),
            ),
            ev(
                "a4",
                1000,
                "[run] finish",
                touches(&[("run", TouchKind::Finish)]),
            ),
            ev(
                "a5",
                2000,
                "[run] intro again",
                touches(&[("run", TouchKind::Intro)]),
            ),
            ev(
                "a6",
                1900,
                "[run] finish again",
                touches(&[("run", TouchKind::Finish)]),
            ),
            ev(
                "a7",
                3000,
                "[squashed] whole run",
                touches(&[("squashed", TouchKind::Finish)]),
            ),
        ] {
            let emitted = state.apply_commit(&event);
            for log in emitted {
                if let LogEvent::PlanFinalized {
                    duration_secs, sha, ..
                } = log
                {
                    let record = state.finished_plans.last().unwrap();
                    assert_eq!(record.finalized_at, sha);
                    assert_eq!(record.duration_secs, duration_secs);
                }
            }
        }
        assert_eq!(
            state
                .finished_plans
                .iter()
                .map(|p| (p.plan.as_str(), p.duration_secs))
                .collect::<Vec<_>>(),
            vec![("run", 900), ("run", 0), ("squashed", 0)]
        );
        assert_eq!(plan_duration_secs(i64::MIN, i64::MAX), u64::MAX);
        assert!(state.active_plan.is_none());
    }

    #[test]
    fn umbrella_sections_keep_reintroduced_names_and_post_close_work_separate() {
        let intro = |n: &str| LogEvent::PlanIntro {
            plan: plan("repeat"),
            sha: sha(n),
            ts: 1,
            subject: "[repeat] intro".into(),
        };
        // Equal author clocks cannot erase the lifecycle boundaries.
        let (first, second) = (intro("a1"), intro("a3"));
        let (end_first, end_second) = (log_finish("repeat", 2), log_finish("repeat", 4));
        let post_close = log_ev(None, 5);
        let oldest = [&first, &end_first, &second, &end_second, &post_close];
        let forward = umbrella_sections(&oldest, false);
        assert_eq!(
            forward.iter().map(|(_, run)| run.len()).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert_eq!(forward[2].0, UmbrellaKey::AdHoc);
        let newest: Vec<_> = oldest.into_iter().rev().collect();
        let backward = umbrella_sections(&newest, true);
        assert_eq!(
            backward
                .iter()
                .map(|(_, run)| run.len())
                .collect::<Vec<_>>(),
            [1, 2, 2]
        );
        assert_eq!(backward[0].0, UmbrellaKey::AdHoc);
        assert_eq!(backward[1].1[1].sha(), second.sha());
        assert_eq!(backward[2].1[1].sha(), first.sha());
    }

    // ============ Classifier ============
    fn classify_with(subject: &str, active: &str) -> Option<PlanKey> {
        classify(ClassifierInputs {
            subject,
            active_plan: Some(&plan(active)),
        })
        .plan_attribution
    }

    #[test]
    fn matching_tag_attributes_only_the_accepted_plan() {
        assert_eq!(classify_with("[foo] revise", "foo"), Some(plan("foo")));
        for subject in [
            "[ghost] work",
            "[misc] work",
            "plain work",
            "[foo,bar] work",
        ] {
            assert_eq!(classify_with(subject, "foo"), None, "{subject}");
        }
    }

    #[test]
    fn consecutive_brackets_preserve_the_second_in_the_body() {
        let parsed = parse_subject("[foo] [bar] work");
        assert_eq!(parsed.prefix, Some(TitlePrefix::Plan("foo".into())));
        assert_eq!(parsed.body, "[bar] work");
        assert_eq!(classify_with("[foo] [bar] work", "foo"), Some(plan("foo")));
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
        assert!(s.plan(&plan("foo")).is_some());
        assert_eq!(s.plan(&plan("foo")).unwrap().commits.len(), 1);
        assert!(s.plan(&plan("foo")).unwrap().commits[0].touched_plan);
        assert!(!s.plan(&plan("foo")).unwrap().commits[0].touched_code);
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
        let p = &s.plan(&plan("foo")).unwrap();
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
            duration_secs: 0,
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
        assert!(!s.plan(&plan("foo")).is_some());
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
        assert!(!s.plan(&plan("foo")).is_some());
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
        assert!(s.active_plan.is_none());
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
        // After PlanDeleted, the active slot is empty AND
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
        assert!(s.active_plan.is_none());
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
    fn ad_hoc_survives_revision_until_an_accepted_intro() {
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
        assert_eq!(s.ad_hoc.len(), 1, "revision does not start a new lifecycle");
        s.apply_commit(&ev(
            "4444",
            4,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        assert_eq!(s.ad_hoc.len(), 1);
        s.apply_commit(&ev(
            "5555",
            5,
            "[bar] intro",
            touches(&[("bar", TouchKind::Intro)]),
        ));
        assert!(s.ad_hoc.is_empty());
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

    fn introduce(s: &mut RepoState, name: &str, n: &str) {
        s.apply_commit(&ev(
            n,
            1,
            &format!("[{name}] intro"),
            touches(&[(name, TouchKind::Intro)]),
        ));
    }

    #[test]
    fn overlapping_intro_is_atomic_except_for_its_warning() {
        let mut s = RepoState::new();
        introduce(&mut s, "foo", "1111");
        s.ad_hoc.push(AdHocEvent {
            sha: sha("aaaa"),
            ts: 1,
            touched_code: true,
        });
        let before = s.clone();
        let mut event = ev(
            "2222",
            2,
            "[foo] work and [bar] intro",
            touches(&[("foo", TouchKind::Revise), ("bar", TouchKind::Intro)]),
        );
        event.has_code_changes = true;
        assert!(s.apply_commit(&event).is_empty());
        assert_eq!(s.warnings.len(), 1);
        assert!(matches!(&s.warnings[0].warning,
            Warning::RejectedPlanIntro { plan: p, reason: IntroRejection::PlanAlreadyActive { active } }
                if p == &plan("bar") && active == &plan("foo")));
        s.warnings.clear();
        assert_eq!(
            s, before,
            "no timeline, adoption, finished or ad-hoc mutation"
        );
    }

    #[test]
    fn ignored_run_cannot_mutate_any_work_before_or_after_active_ends() {
        for end in [TouchKind::Finish, TouchKind::Delete] {
            let mut s = RepoState::new();
            // Rejected reuse of an old name must not erase its accepted history.
            introduce(&mut s, "bar", "0001");
            s.apply_commit(&ev(
                "0002",
                1,
                "[bar] finish",
                touches(&[("bar", TouchKind::Finish)]),
            ));
            introduce(&mut s, "foo", "1111");
            introduce(&mut s, "bar", "2222");
            for phase in 0..2 {
                let before = s.clone();
                for kind in [TouchKind::Revise, TouchKind::Finish, TouchKind::Delete] {
                    let mut event = ev("3333", 3, "[bar] work", touches(&[("bar", kind)]));
                    event.has_code_changes = true;
                    assert!(s.apply_commit(&event).is_empty());
                    assert_eq!(s, before);
                }
                let mut tag_only = ev("4444", 4, "[bar] implementation", Vec::new());
                tag_only.has_code_changes = true;
                assert!(s.apply_commit(&tag_only).is_empty());
                assert_eq!(s, before, "ignored tags cannot create ad-hoc reviewer work");
                if phase == 0 {
                    s.apply_commit(&ev("5555", 5, "[foo] end", touches(&[("foo", end)])));
                }
            }
            assert_eq!(s.finished_record(&plan("bar")).unwrap().intro, sha("0001"));
            introduce(&mut s, "bar", "6666");
            assert_eq!(s.active_plan.as_ref().unwrap().key, plan("bar"));
            assert_eq!(s.plan(&plan("bar")).unwrap().commits[0].sha, sha("6666"));
            s.apply_commit(&ev(
                "7777",
                7,
                "[bar] finish",
                touches(&[("bar", TouchKind::Finish)]),
            ));
            assert_eq!(
                s.finished_plans
                    .iter()
                    .filter(|f| f.plan == plan("bar"))
                    .count(),
                2
            );
        }
    }

    #[test]
    fn a_duplicate_intro_does_not_reassign_the_occupied_identity() {
        let mut s = RepoState::new();
        introduce(&mut s, "foo", "1111");
        let before = s.active_plan.clone();
        introduce(&mut s, "foo", "2222");
        assert_eq!(s.active_plan, before);
        assert_eq!(s.warnings.len(), 1);
        s.apply_commit(&ev(
            "3333",
            3,
            "[foo] revise",
            touches(&[("foo", TouchKind::Revise)]),
        ));
        assert_eq!(s.plan(&plan("foo")).unwrap().commits.len(), 2);
        s.apply_commit(&ev(
            "4444",
            4,
            "[foo] finish",
            touches(&[("foo", TouchKind::Finish)]),
        ));
        s.apply_commit(&ev(
            "5555",
            5,
            "[foo] delete",
            touches(&[("foo", TouchKind::Delete)]),
        ));
        assert!(s.finished_record(&plan("foo")).is_none());
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn hard_forget_of_finished_history_preserves_the_different_active_run() {
        let mut s = RepoState::new();
        introduce(&mut s, "old", "0001");
        s.apply_commit(&ev(
            "0002",
            1,
            "[old] finish",
            touches(&[("old", TouchKind::Finish)]),
        ));
        introduce(&mut s, "live", "1111");
        let active = s.active_plan.clone();
        let log = s.apply_commit(&ev(
            "2222",
            2,
            "[old] delete",
            touches(&[("old", TouchKind::Delete)]),
        ));
        assert_eq!(s.active_plan, active);
        assert!(s.finished_record(&plan("old")).is_none());
        assert!(matches!(&log[0], LogEvent::PlanDeleted { plan: key, .. } if key == &plan("old")));
    }

    #[test]
    fn several_intros_in_one_commit_require_an_unambiguous_new_intro() {
        let mut s = RepoState::new();
        let before = s.clone();
        assert!(
            s.apply_commit(&ev(
                "1111",
                1,
                "[a,b] intro",
                touches(&[("a", TouchKind::Intro), ("b", TouchKind::Intro)])
            ))
            .is_empty()
        );
        assert_eq!(s.warnings.len(), 2);
        let warnings = std::mem::take(&mut s.warnings);
        assert_eq!(s, before);
        s.warnings = warnings;
        assert!(
            s.apply_commit(&ev(
                "2222",
                2,
                "[a] revise",
                touches(&[("a", TouchKind::Revise)])
            ))
            .is_empty()
        );
        introduce(&mut s, "a", "3333");
        assert_eq!(s.active_plan.as_ref().unwrap().key, plan("a"));
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
        assert_eq!(p.prefix, Some(TitlePrefix::Plan("foo".into())));
        assert_eq!(p.body, "intro");
    }

    #[test]
    fn parse_subject_comma_is_one_literal_candidate() {
        let p = parse_subject("[foo,bar] shared work");
        assert_eq!(p.prefix, Some(TitlePrefix::Plan("foo,bar".into())));
        assert_eq!(p.body, "shared work");
    }

    #[test]
    fn parse_subject_misc_is_just_a_plan_name() {
        // No special `[misc]` — it parses as a one-element plan list
        // (that won't match a real plan, so it's ad-hoc).
        let p = parse_subject("[misc] drive-by");
        assert_eq!(p.prefix, Some(TitlePrefix::Plan("misc".into())));
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
        assert_eq!(p.prefix, Some(TitlePrefix::Plan("foo".into())));
        assert_eq!(p.body, " double-space");
    }

    #[test]
    fn parse_subject_no_body() {
        let p = parse_subject("[foo]");
        assert_eq!(p.prefix, Some(TitlePrefix::Plan("foo".into())));
        assert_eq!(p.body, "");
    }

    #[test]
    fn parse_title_prefix_still_works() {
        // Backwards-compat wrapper.
        assert_eq!(
            parse_title_prefix("[foo] x"),
            Some(TitlePrefix::Plan("foo".into()))
        );
        assert_eq!(
            parse_title_prefix("[misc] x"),
            Some(TitlePrefix::Plan("misc".into()))
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
            duration_secs: 0,
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
