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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey, PlanPath};
use crate::mcp_response::compute_plan_worktree_status_parts;
use crate::projection::{
    expected_action, impl_gate_for, latest_impl_commit, latest_plan_touching_commit, phase,
    plan_gate_for, waiting_on,
};
use crate::repo_state::{Phase, PlanLookupError, Trinity, WaitingReason, WaitingRole};
use crate::review_state::ReviewGateDecision;
use crate::runtime::Runtime;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub role: String,
    pub plan_path: String,
    /// Optional on the wire so schema-strict MCP clients allow the shim
    /// to autofill from its cache; the daemon rejects calls that arrive
    /// without one once autofill has had its chance.
    #[serde(default)]
    pub author_label: Option<String>,
    /// Absolute repo root. The MCP dispatcher fills it from the caller's
    /// cwd (via `git rev-parse --show-toplevel`) when absent; the HTTP
    /// route rejects the request if absent. Canonicalized via
    /// `dunce::canonicalize` so symlink and case-normalized variants
    /// match the runtime's `Trinity.repos` keys. `~/…` is NOT expanded
    /// — callers pass absolute paths.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WaitResponse {
    Work {
        /// Imperative action verb naming what the caller should do, e.g.
        /// `review_impl`, `address_plan_request_changes`. Matches the
        /// `expected_action` vocabulary from the projection layer.
        work: String,
        /// Repo-relative paths the caller should read or write. Meaning
        /// depends on `work` — see the per-action mapping in the tool
        /// description.
        locations: Vec<String>,
    },
    Timeout {
        timed_out: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("invalid role: {0} (expected `master` or `reviewers`)")]
    InvalidRole(String),
    #[error("plan_path is required")]
    MissingPlanPath,
    #[error("author_label is required")]
    MissingAuthorLabel,
    #[error("repo is required (HTTP) or could not be resolved from cwd (MCP)")]
    MissingRepo,
    #[error("invalid plan_path: {0}")]
    InvalidPlanPath(PlanPath),
    #[error("unknown plan in repo: {0}")]
    UnknownPlan(String),
    #[error("plan path mismatch: current is {current}, requested {requested}")]
    PlanPathMismatch {
        current: PlanPath,
        requested: PlanPath,
    },
    #[error("plan conflict: stem `{key}` maps to {paths:?}")]
    PlanConflict { key: PlanKey, paths: Vec<PlanPath> },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Block until the named plan in the named repo needs the caller's role.
/// Returns the work + locations to act on. After `timeout_secs` returns
/// `{timed_out: true}` with no work.
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> Result<WaitResponse, WaitError> {
    let role = parse_role(&args.role)?;
    if args.plan_path.is_empty() {
        return Err(WaitError::MissingPlanPath);
    }
    let author_label = args
        .author_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(WaitError::MissingAuthorLabel)?;
    let repo_str = args.repo.as_ref().ok_or(WaitError::MissingRepo)?;
    let repo = dunce::canonicalize(repo_str).unwrap_or_else(|_| PathBuf::from(repo_str));
    let plan_path = PlanPath::new(&args.plan_path);
    let author = AgentLabel::from(author_label);

    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    );
    let started_at = Instant::now();
    let mut rx = runtime.subscribe_events();

    if let Some(work) = compute_match(runtime, &repo, &plan_path, role, &author).await? {
        return Ok(WaitResponse::Work {
            work: work.work.to_string(),
            locations: work.locations,
        });
    }

    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitResponse::Timeout { timed_out: true });
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                if let Some(work) = compute_match(runtime, &repo, &plan_path, role, &author).await?
                {
                    return Ok(WaitResponse::Work {
                        work: work.work.to_string(),
                        locations: work.locations,
                    });
                }
            }
            Ok(Err(RecvError::Lagged(_))) => {
                rx = runtime.subscribe_events();
                if let Some(work) = compute_match(runtime, &repo, &plan_path, role, &author).await?
                {
                    return Ok(WaitResponse::Work {
                        work: work.work.to_string(),
                        locations: work.locations,
                    });
                }
            }
            Ok(Err(RecvError::Closed)) | Err(_) => {
                return Ok(WaitResponse::Timeout { timed_out: true });
            }
        }
    }
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
    work: &'static str,
    locations: Vec<String>,
}

/// Snapshot the candidate under lock, release, then disk-read
/// `plan_worktree_status` and derive the work item if any.
async fn compute_match(
    runtime: &Runtime,
    repo: &Path,
    plan_path: &PlanPath,
    role: WaitingRole,
    author: &AgentLabel,
) -> Result<Option<WorkItem>, WaitError> {
    let candidate = {
        let trinity_arc = runtime.state();
        let trinity = trinity_arc.lock().await;
        collect_candidate(&trinity, repo, plan_path)
    }?;

    let status = compute_plan_worktree_status_parts(
        &candidate.repo_root,
        &candidate.plan_path,
        &candidate.body_hash,
    )?;
    let w = waiting_on(
        candidate.session_phase,
        status,
        candidate.plan_gate.as_ref(),
        candidate.impl_gate.as_ref(),
    );
    if w.role != role {
        return Ok(None);
    }
    if matches!(role, WaitingRole::Reviewers) && caller_already_voted(&candidate, w.reason, author)
    {
        return Ok(None);
    }
    let work = expected_action(w.reason);
    let locations = derive_locations(&candidate, w.reason, author);
    Ok(Some(WorkItem { work, locations }))
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
        PlanNeedsInitialReview | PlanNeedsRereview => cand.plan_gate.as_ref(),
        ImplNeedsInitialReview | ImplNeedsRereview => cand.impl_gate.as_ref(),
        SessionDone
        | CommitDoneMove
        | RestoreOrCommitDoneMove
        | CommitPlanRevision
        | AddressPlanRequestChanges
        | ReadyToImplement
        | AddressImplRequestChanges
        | ReadyToFinish => return false,
    };
    let Some(gate) = gate else { return false };
    gate.approvals.contains(author) || gate.request_changes.contains(author)
}

/// Produce the repo-relative paths to attach to the response. Meaning
/// per work type:
/// - `review_plan` / `review_impl`: the canonical file the caller should
///   create at `.trinity/feedback/<sid>/<phase>/<sha>/<author>.md`.
/// - `address_plan_request_changes`: every RC feedback file on the
///   current plan target, plus the plan file (which the caller will
///   revise).
/// - `address_impl_request_changes`: every RC feedback file on the
///   current impl target (the caller addresses these by changing code).
/// - `commit_plan_revision` / `commit_done_move` /
///   `restore_or_commit_done_move` / `implement_and_commit` /
///   `move_to_done`: the plan file itself.
fn derive_locations(cand: &Candidate, reason: WaitingReason, author: &AgentLabel) -> Vec<String> {
    let plan_file = cand.plan_path.to_string_lossy().into_owned();
    let sid = cand.plan_key.as_str();

    match reason {
        WaitingReason::PlanNeedsInitialReview | WaitingReason::PlanNeedsRereview => {
            let Some(target) = &cand.plan_target else {
                return Vec::new();
            };
            vec![feedback_path(sid, "plan", target, author.as_str())]
        }
        WaitingReason::ImplNeedsInitialReview | WaitingReason::ImplNeedsRereview => {
            let Some(target) = &cand.impl_target else {
                return Vec::new();
            };
            vec![feedback_path(sid, "impl", target, author.as_str())]
        }
        WaitingReason::AddressPlanRequestChanges => {
            let mut out = rc_feedback_paths(
                cand.plan_target.as_ref(),
                cand.plan_gate.as_ref(),
                sid,
                "plan",
            );
            out.push(plan_file);
            out
        }
        WaitingReason::AddressImplRequestChanges => rc_feedback_paths(
            cand.impl_target.as_ref(),
            cand.impl_gate.as_ref(),
            sid,
            "impl",
        ),
        WaitingReason::CommitDoneMove
        | WaitingReason::RestoreOrCommitDoneMove
        | WaitingReason::CommitPlanRevision
        | WaitingReason::ReadyToImplement
        | WaitingReason::ReadyToFinish => vec![plan_file],
        WaitingReason::SessionDone => Vec::new(),
    }
}

fn feedback_path(sid: &str, phase: &str, target: &CommitSha, author: &str) -> String {
    format!(
        ".trinity/feedback/{}/{}/{}/{}.md",
        sid,
        phase,
        target.as_str(),
        author
    )
}

fn rc_feedback_paths(
    target: Option<&CommitSha>,
    gate: Option<&ReviewGateDecision>,
    sid: &str,
    phase: &str,
) -> Vec<String> {
    let (Some(target), Some(gate)) = (target, gate) else {
        return Vec::new();
    };
    gate.request_changes
        .iter()
        .map(|author| feedback_path(sid, phase, target, author.as_str()))
        .collect()
}

#[derive(Debug, Clone)]
struct Candidate {
    repo_root: PathBuf,
    plan_key: PlanKey,
    plan_path: PlanPath,
    body_hash: ContentHash,
    session_phase: Phase,
    plan_gate: Option<ReviewGateDecision>,
    impl_gate: Option<ReviewGateDecision>,
    plan_target: Option<CommitSha>,
    impl_target: Option<CommitSha>,
}

fn collect_candidate(
    trinity: &Trinity,
    repo: &Path,
    plan_path: &PlanPath,
) -> Result<Candidate, WaitError> {
    let Some(repo_state) = trinity.repos.get(repo) else {
        return Err(WaitError::UnknownPlan(format!(
            "{} in {}",
            plan_path,
            repo.display()
        )));
    };
    let plan = repo_state
        .resolve_plan(plan_path)
        .map_err(|err| match err {
            PlanLookupError::InvalidPlanPath(p) => WaitError::InvalidPlanPath(p),
            PlanLookupError::UnknownPlan(k) => {
                WaitError::UnknownPlan(format!("{} in {}", k, repo.display()))
            }
            PlanLookupError::PlanPathMismatch { current, requested } => {
                WaitError::PlanPathMismatch { current, requested }
            }
            PlanLookupError::PlanConflict { key, paths } => WaitError::PlanConflict { key, paths },
        })?;
    Ok(Candidate {
        repo_root: repo.to_path_buf(),
        plan_key: plan.id.clone(),
        plan_path: plan.plan_path.clone(),
        body_hash: plan.body_hash.clone(),
        session_phase: phase(plan, &repo_state.attribution),
        plan_gate: plan_gate_for(plan, repo_state),
        impl_gate: impl_gate_for(plan, repo_state),
        plan_target: latest_plan_touching_commit(plan, repo_state),
        impl_target: latest_impl_commit(plan, repo_state),
    })
}

#[cfg(test)]
mod tests {
    //! Pure tests over `derive_locations` + `parse_role`. Lock-bound
    //! orchestration (snapshot under mutex → disk read → match) gets
    //! covered by `integration_tests`.

    use super::*;
    use crate::lifecycle::content_hash;
    use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase};

    fn agents(labels: &[&str]) -> Vec<AgentLabel> {
        labels.iter().map(|s| AgentLabel::from(*s)).collect()
    }

    fn gate(
        phase: ReviewPhase,
        state: ReviewGateState,
        participants: Vec<AgentLabel>,
        approvals: Vec<AgentLabel>,
        request_changes: Vec<AgentLabel>,
        missing_approvals: Vec<AgentLabel>,
    ) -> ReviewGateDecision {
        ReviewGateDecision {
            phase,
            state,
            approval_rule: "all_participants",
            participants,
            approvals,
            request_changes,
            unmarked: Vec::new(),
            missing_approvals,
        }
    }

    fn cand(plan_target: Option<&str>, impl_target: Option<&str>) -> Candidate {
        Candidate {
            repo_root: PathBuf::from("/repo"),
            plan_key: PlanKey::from("sid"),
            plan_path: PlanPath::new(".trinity/plans/sid.md"),
            body_hash: content_hash("x"),
            session_phase: Phase::Planning,
            plan_gate: None,
            impl_gate: None,
            plan_target: plan_target.map(CommitSha::from),
            impl_target: impl_target.map(CommitSha::from),
        }
    }

    fn me() -> AgentLabel {
        AgentLabel::from("codex")
    }

    #[test]
    fn review_plan_location_is_canonical_write_path_for_caller() {
        let c = cand(Some("abc123"), None);
        let v = derive_locations(&c, WaitingReason::PlanNeedsInitialReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/plan/abc123/codex.md"]);
    }

    #[test]
    fn review_impl_location_uses_impl_target() {
        let c = cand(None, Some("def456"));
        let v = derive_locations(&c, WaitingReason::ImplNeedsInitialReview, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/impl/def456/codex.md"]);
    }

    #[test]
    fn review_plan_returns_empty_when_no_plan_target() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::PlanNeedsRereview, &me());
        assert!(v.is_empty());
    }

    #[test]
    fn address_plan_request_changes_lists_rc_files_then_plan() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::ChangesRequested,
            agents(&["alice", "bob"]),
            Vec::new(),
            agents(&["alice", "bob"]),
            Vec::new(),
        );
        let mut c = cand(Some("plan1"), None);
        c.plan_gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressPlanRequestChanges, &me());
        assert_eq!(
            v,
            vec![
                ".trinity/feedback/sid/plan/plan1/alice.md",
                ".trinity/feedback/sid/plan/plan1/bob.md",
                ".trinity/plans/sid.md",
            ]
        );
    }

    #[test]
    fn address_impl_request_changes_lists_rc_files_only() {
        let g = gate(
            ReviewPhase::Impl,
            ReviewGateState::ChangesRequested,
            agents(&["dana"]),
            Vec::new(),
            agents(&["dana"]),
            Vec::new(),
        );
        let mut c = cand(None, Some("impl9"));
        c.impl_gate = Some(g);
        let v = derive_locations(&c, WaitingReason::AddressImplRequestChanges, &me());
        assert_eq!(v, vec![".trinity/feedback/sid/impl/impl9/dana.md"]);
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
        let v = derive_locations(&c, WaitingReason::ReadyToFinish, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn ready_to_implement_location_is_plan_file() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::ReadyToImplement, &me());
        assert_eq!(v, vec![".trinity/plans/sid.md"]);
    }

    #[test]
    fn session_done_yields_no_locations() {
        let c = cand(None, None);
        let v = derive_locations(&c, WaitingReason::SessionDone, &me());
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
        WaitArgs {
            role: role.to_string(),
            plan_path: format!(".trinity/plans/{sid}.md"),
            author_label: Some(author.to_string()),
            repo: Some(repo.to_string_lossy().into_owned()),
            timeout_secs: Some(2),
        }
    }

    fn expect_work(r: WaitResponse) -> (String, Vec<String>) {
        match r {
            WaitResponse::Work { work, locations } => (work, locations),
            WaitResponse::Timeout { .. } => panic!("expected work, got timeout"),
        }
    }

    fn expect_timeout(r: WaitResponse) {
        match r {
            WaitResponse::Timeout { timed_out } => assert!(timed_out),
            WaitResponse::Work { work, locations } => {
                panic!("expected timeout, got work={work} locations={locations:?}")
            }
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
        assert_eq!(work, "review_plan");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].starts_with(".trinity/feedback/foo/plan/"),
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
                s.plans[&PlanKey::from("foo")].plan_intro.clone()
            })
            .await
            .unwrap();
        // Two RC feedbacks at the canonical path.
        let bob_path = format!(".trinity/feedback/foo/plan/{}/bob.md", intro.as_str());
        let dana_path = format!(".trinity/feedback/foo/plan/{}/dana.md", intro.as_str());
        write_file(dir.path(), &bob_path, "REQUEST_CHANGES\n");
        write_file(dir.path(), &dana_path, "REQUEST_CHANGES\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten {
                parsed: crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
                    "foo/plan/{}/bob.md",
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
                    "foo/plan/{}/dana.md",
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
        assert_eq!(work, "address_plan_request_changes");
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
                s.plans[&PlanKey::from("foo")].plan_intro.clone()
            })
            .await
            .unwrap();
        // Both codex + bob approve the intro target → participants.
        for author in ["codex", "bob"] {
            let rel = format!(
                ".trinity/feedback/foo/plan/{}/{}.md",
                intro.as_str(),
                author
            );
            write_file(dir.path(), &rel, "APPROVE\n");
            let parsed_rel = PathBuf::from(format!("foo/plan/{}/{}.md", intro.as_str(), author));
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
                crate::projection::latest_plan_touching_commit(&s.plans[&PlanKey::from("foo")], s)
                    .unwrap()
            })
            .await
            .unwrap();
        let codex_rel = format!(".trinity/feedback/foo/plan/{}/codex.md", revised.as_str());
        write_file(dir.path(), &codex_rel, "APPROVE\n");
        let parsed = crate::disk_format::parse_feedback_path(&PathBuf::from(format!(
            "foo/plan/{}/codex.md",
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
        assert_eq!(work, "review_plan");
        assert_eq!(locations.len(), 1);
        assert!(
            locations[0].ends_with("/bob.md"),
            "bob's write path should be returned, got {}",
            locations[0]
        );
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
                session_id: PlanKey::from("foo"),
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
    async fn missing_repo_errors() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_path: ".trinity/plans/foo.md".to_string(),
            author_label: Some("codex".to_string()),
            repo: None,
            timeout_secs: Some(1),
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingRepo));
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_none() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_path: ".trinity/plans/foo.md".to_string(),
            author_label: None,
            repo: Some("/anywhere".to_string()),
            timeout_secs: Some(1),
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }

    #[tokio::test]
    async fn missing_author_label_errors_when_blank() {
        let rt = Runtime::new();
        let a = WaitArgs {
            role: "reviewers".to_string(),
            plan_path: ".trinity/plans/foo.md".to_string(),
            author_label: Some("   ".to_string()),
            repo: Some("/anywhere".to_string()),
            timeout_secs: Some(1),
        };
        let err = wait_for_work(&rt, a).await.unwrap_err();
        assert!(matches!(err, WaitError::MissingAuthorLabel));
    }
}
