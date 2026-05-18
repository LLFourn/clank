//! `wait_for_work` long-poll: block until the named plan needs the caller's
//! role, then return the work + locations to act on.
//!
//! Single-plan focus: the caller names `plan_path` (and implicitly or
//! explicitly the `repo`) so the response is always for one plan in one
//! repo — no fan-out, no cross-repo. The response carries `work` (an
//! imperative action verb) plus `locations` (the repo-relative paths the
//! caller should read or write to do that work).
//!
//! Lock boundary: resolve the plan and snapshot its cheap gate state under
//! the runtime mutex, release the lock, then per-poll read
//! `plan_worktree_status` from disk and derive `waiting_on`. Status-driven
//! master waits (commit_plan_revision, commit_done_move, etc.) stay
//! correct without holding the mutex across disk I/O.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey};
use crate::mcp_response::compute_plan_worktree_status_parts;
use crate::projection::waiting_on;
use crate::repo_state::{Trinity, WaitingReason, WaitingRole};
use crate::review_state::CommitGate;
use crate::runtime::Runtime;

const DEFAULT_TIMEOUT_SECS: u64 = 1800;

#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub role: String,
    /// Optional under phase 2.9. When omitted, the MCP dispatcher
    /// infers the plan from `repo` (or the caller's cwd) — see
    /// `resolve_plan_id`. The HTTP surface still requires a value
    /// here because it has no cwd context; HTTP callers must pre-
    /// resolve plan_id.
    #[serde(default)]
    pub plan_id: String,
    /// Optional repo filter (basename or absolute path) used by the
    /// inference path when `plan_id` is omitted. Ignored when
    /// `plan_id` is explicit.
    #[serde(default)]
    pub repo: Option<String>,
    /// Optional on the wire so schema-strict MCP clients allow the shim
    /// to autofill from its cache; the daemon rejects calls that arrive
    /// without one once autofill has had its chance.
    #[serde(default)]
    pub author_label: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Top-level WFW response: either a `Work` payload or a `Timeout`.
/// `untagged` so the wire keeps the field-presence discriminator
/// (`work` vs `timed_out`) the existing consumers depend on.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WaitResponse {
    Work(WorkPayload),
    Timeout { timed_out: bool },
}

/// A work assignment. `plan_id` / `repo` / `locations` are always
/// present; the action-specific fields live on the flattened
/// [`WorkAction`] variant, so the type system enforces that
/// `target_sha` / `commit_kind` / `prompt_hint` only exist for
/// actions that actually need them.
#[derive(Debug, Clone, Serialize)]
pub struct WorkPayload {
    pub plan_id: String,
    /// Canonical absolute path of the repo, convenience for resolving
    /// the repo-relative `locations` without re-parsing `plan_id`.
    pub repo: String,
    /// Repo-relative paths the caller should read or write.
    pub locations: Vec<String>,
    #[serde(flatten)]
    pub action: WorkAction,
}

/// Tagged by the wire `work` discriminator. Variants that carry a
/// `target_sha` also carry `commit_kind` and `prompt_hint`; variants
/// that are pure worktree-status moves (`RestoreOrCommitPlanFile`,
/// `SessionFinished`) carry only the envelope fields on
/// `WorkPayload`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "work", rename_all = "snake_case")]
pub enum WorkAction {
    ReviewCommit {
        target_sha: String,
        commit_kind: String,
        prompt_hint: String,
    },
    AddressCommitChanges {
        target_sha: String,
        commit_kind: String,
        prompt_hint: String,
    },
    CommitPlanRevision {
        target_sha: String,
        commit_kind: String,
        prompt_hint: String,
    },
    RestoreOrCommitPlanFile,
    StartImplementation {
        target_sha: String,
        commit_kind: String,
        prompt_hint: String,
    },
    SessionFinished,
}

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("invalid role: {0} (expected `master` or `reviewers`)")]
    InvalidRole(String),
    #[error("plan_id is required")]
    MissingPlanId,
    #[error("author_label is required")]
    MissingAuthorLabel,
    #[error("invalid plan_id: {0}")]
    InvalidPlanId(String),
    #[error("invalid author_label: {0}")]
    InvalidAuthorLabel(String),
    #[error("unknown repo basename: {0}")]
    UnknownRepo(String),
    #[error("unknown plan: {0}")]
    UnknownPlan(String),
    #[error("plan conflict: stem `{key}` maps to {paths:?}")]
    PlanConflict { key: PlanKey, paths: Vec<PathBuf> },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Block until the named plan needs the caller's role. Returns the work
/// + locations to act on. After `timeout_secs` returns
///   `{timed_out: true}` with no work.
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> Result<WaitResponse, WaitError> {
    let role = parse_role(&args.role)?;
    if args.plan_id.is_empty() {
        return Err(WaitError::MissingPlanId);
    }
    let plan_id = crate::lifecycle::PlanId::parse(&args.plan_id)
        .map_err(|e| WaitError::InvalidPlanId(e.to_string()))?;
    let author_label = args
        .author_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(WaitError::MissingAuthorLabel)?;
    let author = AgentLabel::parse(author_label)
        .map_err(|e| WaitError::InvalidAuthorLabel(e.to_string()))?;

    let timeout = derive_timeout(args.timeout_secs);
    let started_at = Instant::now();
    let mut rx = runtime.subscribe_events();

    if let Some(work) = compute_match(runtime, &plan_id, role, &author).await? {
        return Ok(work.into_response(plan_id.to_string()));
    }

    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitResponse::Timeout { timed_out: true });
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                if let Some(work) = compute_match(runtime, &plan_id, role, &author).await? {
                    return Ok(work.into_response(plan_id.to_string()));
                }
            }
            Ok(Err(RecvError::Lagged(_))) => {
                rx = runtime.subscribe_events();
                if let Some(work) = compute_match(runtime, &plan_id, role, &author).await? {
                    return Ok(work.into_response(plan_id.to_string()));
                }
            }
            Ok(Err(RecvError::Closed)) | Err(_) => {
                return Ok(WaitResponse::Timeout { timed_out: true });
            }
        }
    }
}

/// Derive the long-poll duration from the caller's `timeout_secs`.
/// Defaults to `DEFAULT_TIMEOUT_SECS` (1800 / 30 min) on `None`;
/// floors at 1s; no upper cap (phase 11).
fn derive_timeout(timeout_secs: Option<u64>) -> Duration {
    Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1))
}

fn parse_role(s: &str) -> Result<WaitingRole, WaitError> {
    match s {
        "master" => Ok(WaitingRole::Master),
        "reviewers" => Ok(WaitingRole::Reviewers),
        other => Err(WaitError::InvalidRole(other.to_string())),
    }
}

#[derive(Debug, Clone)]
struct WorkItem {
    /// Canonical absolute path of the repo (echo'd back to the caller as
    /// a convenience field on the response).
    repo: String,
    locations: Vec<String>,
    action: WorkAction,
}

impl WorkItem {
    fn into_response(self, plan_id: String) -> WaitResponse {
        WaitResponse::Work(WorkPayload {
            plan_id,
            repo: self.repo,
            locations: self.locations,
            action: self.action,
        })
    }
}

/// Snapshot the candidate under lock, release, then disk-read
/// `plan_worktree_status` and derive the work item if any.
async fn compute_match(
    runtime: &Runtime,
    plan_id: &crate::lifecycle::PlanId,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WorkItem>, WaitError> {
    let candidate = {
        let trinity_arc = runtime.state();
        let trinity = trinity_arc.lock().await;
        collect_candidate(&trinity, plan_id)
    }?;

    let status = compute_plan_worktree_status_parts(
        &candidate.repo_root,
        &candidate.plan_path,
        &candidate.body_hash,
    )?;
    let w = waiting_on(candidate.is_finished, status, candidate.gate.as_ref());
    if w.role != role {
        return Ok(None);
    }
    if matches!(role, WaitingRole::Reviewers) && caller_already_voted(&candidate, w.reason, author)
    {
        return Ok(None);
    }
    let locations = derive_locations(&candidate, w.reason, author);
    let repo = candidate.repo_root.to_string_lossy().into_owned();
    let action = build_action(w.reason, &candidate);

    Ok(Some(WorkItem {
        repo,
        locations,
        action,
    }))
}

/// Build the typed `WorkAction` for one `(reason, candidate)` pair.
/// Variants that carry a target SHA pull it (and its `commit_kind` +
/// `prompt_hint`) from the candidate's pre-computed
/// `review_target` / `review_target_kind`. Worktree-status moves
/// (`CommitDoneMove`, `RestoreOrCommitDoneMove`, `SessionDone`) carry
/// nothing else.
fn build_action(reason: WaitingReason, candidate: &Candidate) -> WorkAction {
    use WaitingReason::*;
    let target_sha = || {
        candidate
            .review_target
            .as_ref()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    };
    let commit_kind = || {
        candidate
            .review_target_kind
            .map(|k| k.as_str().to_string())
            .unwrap_or_default()
    };
    let prompt = |kind: Option<&str>| {
        prompt_hint_for(reason, kind, &candidate.plan_path).unwrap_or_default()
    };
    match reason {
        SessionFinished => WorkAction::SessionFinished,
        RestoreOrCommitPlanFile => WorkAction::RestoreOrCommitPlanFile,
        CommitPlanRevision => {
            let kind = commit_kind();
            let kind_opt = if kind.is_empty() {
                None
            } else {
                Some(kind.as_str())
            };
            WorkAction::CommitPlanRevision {
                target_sha: target_sha(),
                commit_kind: kind.clone(),
                prompt_hint: prompt(kind_opt),
            }
        }
        AddressCommitChanges => {
            let kind = commit_kind();
            let kind_opt = if kind.is_empty() {
                None
            } else {
                Some(kind.as_str())
            };
            WorkAction::AddressCommitChanges {
                target_sha: target_sha(),
                commit_kind: kind.clone(),
                prompt_hint: prompt(kind_opt),
            }
        }
        ReadyToStartImplementation => {
            let kind = commit_kind();
            let kind_opt = if kind.is_empty() {
                None
            } else {
                Some(kind.as_str())
            };
            WorkAction::StartImplementation {
                target_sha: target_sha(),
                commit_kind: kind.clone(),
                prompt_hint: prompt(kind_opt),
            }
        }
        CommitNeedsReview => {
            let kind = commit_kind();
            let kind_opt = if kind.is_empty() {
                None
            } else {
                Some(kind.as_str())
            };
            WorkAction::ReviewCommit {
                target_sha: target_sha(),
                commit_kind: kind.clone(),
                prompt_hint: prompt(kind_opt),
            }
        }
    }
}

/// Default `prompt_hint` text for a given `(reason, commit_kind)`.
/// Short imperative; the agent can override. Returns `None` for
/// terminal/no-work states.
fn prompt_hint_for(
    reason: WaitingReason,
    commit_kind: Option<&str>,
    plan_path: &std::path::Path,
) -> Option<String> {
    use WaitingReason::*;
    let plan_path_str = plan_path.to_string_lossy();
    Some(match (reason, commit_kind) {
        (CommitNeedsReview, Some("plan_only")) => format!(
            "This commit only changes the plan. Read the plan file at {plan_path_str} and \
             review the proposed approach."
        ),
        (CommitNeedsReview, Some("code_only")) => {
            "This commit makes implementation changes. Review the diff and check it against the \
             approved plan."
                .to_string()
        }
        (CommitNeedsReview, Some("mixed")) => format!(
            "This commit changes both the plan and code. Read the updated plan at {plan_path_str} \
             and review the diff together."
        ),
        (CommitNeedsReview, _) => "Review the latest commit on this plan.".to_string(),
        (AddressCommitChanges, _) => "Review requested changes on the latest commit. \
             Read each REQUEST_CHANGES file in the locations and address them with a follow-up commit."
            .to_string(),
        (CommitPlanRevision, _) => format!(
            "Plan has uncommitted changes at {plan_path_str}. Commit the revision to release \
             blocked reviews."
        ),
        (RestoreOrCommitPlanFile, _) => format!(
            "Plan file is missing at {plan_path_str}. Either restore it \
             (`git checkout -- {plan_path_str}`) or commit the deletion."
        ),
        (ReadyToStartImplementation, _) => "Latest commit is approved. Continue with the next \
             commit or finalize the plan."
            .to_string(),
        (SessionFinished, _) => return None,
    })
}

/// True if `author` already has a current-target verdict for the
/// reviewer work named by `reason`. Used to keep `wait_for_work` from
/// re-waking a reviewer for a target they've already voted on (their
/// vote still stands; the remaining wait is on someone else).
///
/// Master-role reasons return false — caller-already-voted is a
/// reviewer-only concept. The match is exhaustive: adding any new
/// `WaitingReason` forces a compile error here. **The compiler enforces
/// totality, not arm placement** — a maintainer who adds a new
/// reviewer-role variant must place it in the gate-lookup branch, not
/// the `return false` branch, or the self-wakeup bug returns. The
/// integration test `caller_already_voted_does_not_re_wake_reviewer`
/// (and its wire counterpart in `server::http::wire_tests`) is the
/// semantic guard for arm placement.
fn caller_already_voted(cand: &Candidate, reason: WaitingReason, author: &AgentLabel) -> bool {
    use WaitingReason::*;
    let gate = match reason {
        // CommitNeedsReview is the only reviewer-role reason. The gate
        // is the latest reviewable commit's gate — same one driving
        // waiting_on and review_target. Master-role reasons return
        // false; caller-already-voted is a reviewer-only concept.
        CommitNeedsReview => cand.gate.as_ref(),
        SessionFinished
        | RestoreOrCommitPlanFile
        | CommitPlanRevision
        | AddressCommitChanges
        | ReadyToStartImplementation => return false,
    };
    let Some(gate) = gate else { return false };
    gate.approvers.contains(author) || gate.requesters.contains(author)
}

/// Produce the repo-relative paths to attach to the response. The
/// review target (and its kind) come from `Candidate`'s pre-computed
/// fields so this function reads from the same projection that
/// drives `waiting_on`, `target_sha`, and `commit_kind`. No second
/// selector.
fn derive_locations(cand: &Candidate, reason: WaitingReason, author: &AgentLabel) -> Vec<String> {
    let plan_file = cand.plan_path.to_string_lossy().into_owned();
    let sid = cand.plan_key.as_str();

    match reason {
        WaitingReason::CommitNeedsReview => {
            let Some(target) = &cand.review_target else {
                return Vec::new();
            };
            vec![feedback_path(sid, target, author.as_str())]
        }
        WaitingReason::AddressCommitChanges => {
            let mut out = rc_feedback_paths(cand.review_target.as_ref(), cand.gate.as_ref(), sid);
            // Plan-side RC (kind is PlanOnly or Mixed) also surfaces
            // the plan file because addressing the RC means revising
            // the plan body. Pure code-side RC (CodeOnly) is fixed by
            // amending code; no extra location needed.
            let plan_side = matches!(
                cand.review_target_kind,
                Some(
                    crate::repo_state::CommitKind::PlanOnly | crate::repo_state::CommitKind::Mixed
                )
            );
            if plan_side {
                out.push(plan_file);
            }
            out
        }
        WaitingReason::RestoreOrCommitPlanFile
        | WaitingReason::CommitPlanRevision
        | WaitingReason::ReadyToStartImplementation => vec![plan_file],
        WaitingReason::SessionFinished => Vec::new(),
    }
}

fn feedback_path(sid: &str, target: &CommitSha, author: &str) -> String {
    format!(
        ".trinity/feedback/{}/{}/{}.md",
        sid,
        target.as_str(),
        author
    )
}

fn rc_feedback_paths(
    target: Option<&CommitSha>,
    gate: Option<&CommitGate>,
    sid: &str,
) -> Vec<String> {
    let (Some(target), Some(gate)) = (target, gate) else {
        return Vec::new();
    };
    gate.requesters
        .iter()
        .map(|author| feedback_path(sid, target, author.as_str()))
        .collect()
}

#[derive(Debug, Clone)]
struct Candidate {
    repo_root: PathBuf,
    plan_key: PlanKey,
    plan_path: PathBuf,
    body_hash: ContentHash,
    is_finished: bool,
    /// One gate: the latest reviewable commit's gate. Folds the old
    /// (plan_gate, impl_gate) pair into the single value that drives
    /// waiting_on, locations, target_sha, and commit_kind.
    gate: Option<CommitGate>,
    /// The SHA the gate was computed on — the same SHA reviewers
    /// should write to. Equal to `latest_reviewable_commit_for`
    /// output, which skips MultiPlan / DoneMove / Unattributed.
    /// `None` when no reviewable commit exists yet.
    review_target: Option<CommitSha>,
    /// `CommitKind` of `review_target`, threaded through so callers
    /// don't re-derive it. `None` iff `review_target` is `None`.
    review_target_kind: Option<crate::repo_state::CommitKind>,
}

fn collect_candidate(
    trinity: &Trinity,
    plan_id: &crate::lifecycle::PlanId,
) -> Result<Candidate, WaitError> {
    let repo_root = trinity
        .repo_basenames
        .get(plan_id.repo())
        .ok_or_else(|| WaitError::UnknownRepo(plan_id.repo().as_str().to_string()))?
        .clone();
    let repo_state = trinity
        .repos
        .get(&repo_root)
        .ok_or_else(|| WaitError::UnknownRepo(plan_id.repo().as_str().to_string()))?;
    if let Some(paths) = repo_state.plan_conflicts.get(plan_id.key()) {
        return Err(WaitError::PlanConflict {
            key: plan_id.key().clone(),
            paths: paths.clone(),
        });
    }
    let plan = repo_state
        .plans
        .get(plan_id.key())
        .ok_or_else(|| WaitError::UnknownPlan(plan_id.to_string()))?;
    let review_target = crate::projection::latest_reviewable_commit_for(plan);
    let review_target_kind = review_target
        .as_ref()
        .map(|sha| crate::projection::commit_kind_for(plan, sha));
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan).cloned();
    Ok(Candidate {
        repo_root,
        plan_key: plan.id.clone(),
        plan_path: plan.plan_path.clone(),
        body_hash: plan.body_hash.clone(),
        is_finished: plan.frozen_at.is_some(),
        gate,
        review_target,
        review_target_kind,
    })
}

#[cfg(test)]
mod tests {
    //! Pure tests over `derive_locations` + `parse_role`. Lock-bound
    //! orchestration (snapshot under mutex → disk read → match) gets
    //! covered by `integration_tests`.

    use super::*;
    use crate::lifecycle::content_hash;
    use crate::review_state::CommitGateState;

    fn agents(labels: &[&str]) -> Vec<AgentLabel> {
        labels
            .iter()
            .map(|s| AgentLabel::parse(s).unwrap())
            .collect()
    }

    fn gate(
        state: CommitGateState,
        participants: Vec<AgentLabel>,
        approvers: Vec<AgentLabel>,
        requesters: Vec<AgentLabel>,
        missing: Vec<AgentLabel>,
    ) -> CommitGate {
        CommitGate {
            state,
            participants,
            approvers,
            requesters,
            ambiguous: Vec::new(),
            missing,
            feedback: std::collections::BTreeMap::new(),
        }
    }

    fn cand(target: Option<&str>, kind: Option<crate::repo_state::CommitKind>) -> Candidate {
        Candidate {
            repo_root: PathBuf::from("/repo"),
            plan_key: PlanKey::parse("sid").unwrap(),
            plan_path: PathBuf::from(".trinity/plans/sid.md"),
            body_hash: content_hash("x"),
            is_finished: false,
            gate: None,
            review_target: target.map(|s| CommitSha::parse(s).unwrap()),
            review_target_kind: kind,
        }
    }

    fn me() -> AgentLabel {
        AgentLabel::parse("codex").unwrap()
    }

    #[test]
    fn review_plan_location_is_canonical_write_path_for_caller() {
        let c = cand(
            Some("abc123"),
            Some(crate::repo_state::CommitKind::PlanOnly),
        );
        let v = derive_locations(&c, WaitingReason::CommitNeedsReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/abc123/codex.md"]);
    }

    #[test]
    fn review_impl_location_uses_target() {
        let c = cand(
            Some("def456"),
            Some(crate::repo_state::CommitKind::CodeOnly),
        );
        let v = derive_locations(&c, WaitingReason::CommitNeedsReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/def456/codex.md"]);
    }

    #[test]
    fn review_returns_empty_when_no_target() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::CommitNeedsReview, &me());
        assert!(v.is_empty());
    }

    #[test]
    fn address_plan_request_changes_lists_rc_files_then_plan() {
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["alice", "bob"]),
            Vec::new(),
        );
        let mut c = cand(Some("a1a1"), Some(crate::repo_state::CommitKind::PlanOnly));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(
            v,
            vec![
                ".trinity/feedback/sid/a1a1/alice.md",
                ".trinity/feedback/sid/a1a1/bob.md",
                ".trinity/plans/sid.md",
            ]
        );
    }

    #[test]
    fn address_impl_request_changes_lists_rc_files_only() {
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["dana"]),
            Vec::new(),
            agents(&["dana"]),
            Vec::new(),
        );
        let mut c = cand(Some("1019"), Some(crate::repo_state::CommitKind::CodeOnly));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/1019/dana.md"]);
    }

    #[test]
    fn address_mixed_kind_treated_as_plan_side() {
        // A `Mixed` commit (plan touch + code) is plan-side for the
        // address-RC location list — the plan file goes on the end.
        let g = gate(
            CommitGateState::ChangesRequested,
            agents(&["alice"]),
            Vec::new(),
            agents(&["alice"]),
            Vec::new(),
        );
        let mut c = cand(Some("3137"), Some(crate::repo_state::CommitKind::Mixed));
        c.gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressCommitChanges, &me());
        assert_eq!(
            v,
            vec![
                ".trinity/feedback/sid/3137/alice.md",
                ".trinity/plans/sid.md",
            ]
        );
    }

    #[test]
    fn commit_plan_revision_location_is_plan_file() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::CommitPlanRevision, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn ready_to_finish_location_is_plan_file() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::ReadyToStartImplementation, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn ready_to_implement_location_is_plan_file() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::ReadyToStartImplementation, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn session_done_yields_no_locations() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::SessionFinished, &me());
        assert!(v.is_empty());
    }

    #[test]
    fn parse_role_rejects_unknown() {
        assert!(matches!(
            parse_role("reviewer"),
            Err(WaitError::InvalidRole(_))
        ));
        assert!(matches!(parse_role(""), Err(WaitError::InvalidRole(_))));
        assert!(matches!(parse_role("none"), Err(WaitError::InvalidRole(_))));
    }

    #[test]
    fn parse_role_accepts_canonical() {
        assert_eq!(parse_role("master").unwrap(), WaitingRole::Master);
        assert_eq!(parse_role("reviewers").unwrap(), WaitingRole::Reviewers);
    }

    #[test]
    fn derive_timeout_default_is_30_minutes() {
        assert_eq!(derive_timeout(None), Duration::from_secs(1800));
    }

    #[test]
    fn derive_timeout_above_300_is_not_clamped() {
        // Regression for the phase-11 cap removal: reintroducing
        // `.clamp(1, 300)` would change this output.
        assert_eq!(derive_timeout(Some(3600)), Duration::from_secs(3600));
        assert_eq!(derive_timeout(Some(86_400)), Duration::from_secs(86_400));
    }

    #[test]
    fn derive_timeout_floors_at_one_second() {
        assert_eq!(derive_timeout(Some(0)), Duration::from_secs(1));
    }
}

#[cfg(test)]
mod integration_tests {
    //! End-to-end against a real `Runtime` over tempdir git repos. Covers
    //! the lock-snapshot-then-disk-read orchestration plus the
    //! broadcast-driven wake-up path.

    use super::*;
    use crate::fs_watcher::FilesystemSignal;
    use crate::runtime::Runtime;
    use std::path::Path;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        run_git(path, &["init", "--quiet", "--initial-branch=main"]);
        run_git(path, &["config", "user.email", "test@test"]);
        run_git(path, &["config", "user.name", "test"]);
        run_git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) {
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "--quiet", "-m", msg]);
    }

    fn args(repo: &Path, role: &str, sid: &str, author: &str) -> WaitArgs {
        let basename = repo
            .file_name()
            .and_then(|n| n.to_str())
            .expect("tempdir has basename");
        WaitArgs {
            role: role.to_string(),
            plan_id: format!("{basename}/{sid}.md"),
            author_label: Some(author.to_string()),
            timeout_secs: Some(2),
            repo: None,
        }
    }

    fn expect_work(r: WaitResponse) -> (String, Vec<String>) {
        match r {
            WaitResponse::Work(p) => (action_tag(&p.action).to_string(), p.locations),
            WaitResponse::Timeout { .. } => panic!("expected work, got timeout"),
        }
    }

    fn expect_timeout(r: WaitResponse) {
        match r {
            WaitResponse::Timeout { timed_out } => assert!(timed_out),
            WaitResponse::Work(p) => {
                panic!(
                    "expected timeout, got work={} locations={:?}",
                    action_tag(&p.action),
                    p.locations
                )
            }
        }
    }

    fn action_tag(a: &WorkAction) -> &'static str {
        match a {
            WorkAction::ReviewCommit { .. } => "review_commit",
            WorkAction::AddressCommitChanges { .. } => "address_commit_changes",
            WorkAction::CommitPlanRevision { .. } => "commit_plan_revision",
            WorkAction::RestoreOrCommitPlanFile => "restore_or_commit_plan_file",
            WorkAction::StartImplementation { .. } => "start_implementation",
            WorkAction::SessionFinished => "session_finished",
        }
    }

    #[tokio::test]
    async fn immediate_review_plan_after_first_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let resp = wait_for_work(&rt, args(dir.path(), "reviewers", "foo", "codex"))
            .await
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "review_commit");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].starts_with(".trinity/feedback/foo/"),
            "got: {}",
            locations[0]
        );
        assert!(locations[0].ends_with("/codex.md"));
    }

    #[tokio::test]
    async fn body_dirty_yields_commit_plan_revision_for_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();
        // Edit but don't commit.
        write_file(
            dir.path(),
            ".trinity/plans/foo.md",
            "# foo v2 uncommitted\n",
        );

        let resp = wait_for_work(&rt, args(dir.path(), "master", "foo", "lloyd"))
            .await
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "commit_plan_revision");
        assert_eq!(locations, vec![".trinity/plans/foo.md".to_string()]);
    }

    #[tokio::test]
    async fn address_plan_request_changes_lists_rc_then_plan_file() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        // Two RC feedbacks at the canonical path.
        let bob_path = format!(".trinity/feedback/foo/{}/bob.md", intro.as_str());
        let dana_path = format!(".trinity/feedback/foo/{}/dana.md", intro.as_str());
        write_file(dir.path(), &bob_path, "REQUEST_CHANGES\n");
        write_file(dir.path(), &dana_path, "REQUEST_CHANGES\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/bob.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/{}/dana.md",
                    intro.as_str()
                )))
                .unwrap(),
            },
            1,
        )
        .await
        .unwrap();

        let resp = wait_for_work(&rt, args(dir.path(), "master", "foo", "lloyd"))
            .await
            .unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "address_commit_changes");
        assert_eq!(
            locations,
            vec![bob_path, dana_path, ".trinity/plans/foo.md".to_string()]
        );
    }

    #[tokio::test]
    async fn caller_already_voted_does_not_re_wake_reviewer() {
        // Two participants on a revised plan target: codex has APPROVE'd
        // the current target, bob is still missing. The gate is
        // NeedsReview (waiting on reviewers) → `plan_needs_rereview`.
        // codex's wait_for_work must time out (their vote stands).
        // bob's wait_for_work must return review_plan.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo v1");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.plans[&PlanKey::parse("foo").unwrap()].plan_intro.clone()
            })
            .await
            .unwrap();
        // Both codex + bob approve the intro target → participants.
        for author in ["codex", "bob"] {
            let rel = format!(".trinity/feedback/foo/{}/{}.md", intro.as_str(), author);
            write_file(dir.path(), &rel, "APPROVE\n");
            let parsed_rel = PathBuf::from(format!("foo/{}/{}.md", intro.as_str(), author));
            let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
            rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 1)
                .await
                .unwrap();
        }

        // Revise the plan so the target SHA advances; codex re-approves
        // the new target. bob stays a participant but hasn't re-voted.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 2)
            .await
            .unwrap();
        let revised: CommitSha = rt
            .read_repo(dir.path(), |s| {
                let plan = &s.plans[&PlanKey::parse("foo").unwrap()];
                crate::projection::all_plan_revisions(plan, s)
                    .into_iter()
                    .next_back()
                    .unwrap()
            })
            .await
            .unwrap();
        let codex_rel = format!(".trinity/feedback/foo/{}/codex.md", revised.as_str());
        write_file(dir.path(), &codex_rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
            "foo/{}/codex.md",
            revised.as_str()
        )))
        .unwrap();
        rt.handle_signal(dir.path(), FilesystemSignal::FeedbackWritten { parsed }, 3)
            .await
            .unwrap();

        // codex polls → already voted → times out.
        let mut a = args(dir.path(), "reviewers", "foo", "codex");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        expect_timeout(resp);

        // bob polls → still missing → returns review work.
        let mut a = args(dir.path(), "reviewers", "foo", "bob");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        let (work, locations) = expect_work(resp);
        assert_eq!(work, "review_commit");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].ends_with("/bob.md"),
            "bob's write path should be returned, got {}",
            locations[0]
        );
    }

    #[tokio::test]
    async fn same_stem_different_repos_route_to_correct_repo() {
        // Two tempdir repos with distinct basenames + the same plan
        // stem. wait_for_work scoped to each repo's basename must
        // resolve to that repo's plan and return locations rooted
        // under it — never confused across repos.
        let parent_a = tempfile::tempdir().unwrap();
        let parent_b = tempfile::tempdir().unwrap();
        let dir_a = parent_a.path().join("alpha");
        let dir_b = parent_b.path().join("beta");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        for d in [&dir_a, &dir_b] {
            run_git(d, &["init", "--quiet", "--initial-branch=main"]);
            run_git(d, &["config", "user.email", "test@test"]);
            run_git(d, &["config", "user.name", "test"]);
            run_git(d, &["config", "commit.gpgsign", "false"]);
        }
        write_file(&dir_a, ".trinity/plans/shared.md", "# in alpha\n");
        commit(&dir_a, "alpha shared");
        write_file(&dir_b, ".trinity/plans/shared.md", "# in beta\n");
        commit(&dir_b, "beta shared");

        let rt = Runtime::new();
        rt.add_repo(dir_a.clone()).await.unwrap();
        rt.add_repo(dir_b.clone()).await.unwrap();

        let resp_a = wait_for_work(&rt, args(&dir_a, "reviewers", "shared", "codex"))
            .await
            .unwrap();
        let resp_b = wait_for_work(&rt, args(&dir_b, "reviewers", "shared", "codex"))
            .await
            .unwrap();
        match (resp_a, resp_b) {
            (WaitResponse::Work(p_a), WaitResponse::Work(p_b)) => {
                assert_eq!(p_a.plan_id, "alpha/shared.md");
                assert_eq!(p_b.plan_id, "beta/shared.md");
                assert_ne!(p_a.repo, p_b.repo);
                assert!(p_a.repo.ends_with("alpha"));
                assert!(p_b.repo.ends_with("beta"));
            }
            other => panic!("both calls should return Work; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_timed_out_shape() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Polling master while only reviewers have work — should time out.
        let mut a = args(dir.path(), "master", "foo", "lloyd");
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        expect_timeout(resp);
    }

    #[tokio::test]
    async fn wakes_on_plan_file_changed_event() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");
        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let repo = dir.path().to_path_buf();
        let join = tokio::spawn(async move {
            let mut a = args(&repo, "master", "foo", "lloyd");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 unsaved\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: PlanKey::parse("foo").unwrap(),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            42,
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return")
            .unwrap()
            .unwrap();
        let (work, _) = expect_work(resp);
        assert_eq!(work, "commit_plan_revision");
    }

    #[tokio::test]
    async fn unknown_session_errors() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");
        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let a = args(dir.path(), "reviewers", "does-not-exist", "codex");
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::UnknownPlan(_)));
    }

    #[tokio::test]
    async fn unknown_repo_errors() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_id: "no-such-repo/foo.md".to_string(),
            author_label: Some("codex".to_string()),
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::UnknownRepo(_)));
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_none() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_id: "anywhere/foo.md".to_string(),
            author_label: None,
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_blank() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_id: "anywhere/foo.md".to_string(),
            author_label: Some("   ".to_string()),
            timeout_secs: Some(1),
            repo: None,
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }
}
