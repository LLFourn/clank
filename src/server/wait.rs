//! `wait_for_work` long-poll: block until a session needs the caller's role.
//!
//! Snapshot under the runtime mutex, release the lock, then compute the
//! per-candidate `plan_worktree_status` from disk and run the pure matcher.
//! This is the lock-boundary fix from the plan: status is disk-derived, so
//! holding the mutex while we hit the filesystem would block watchers and
//! other handlers.
//!
//! Wire response is intentionally minimal — `{repo, session_id, reason}`
//! per match. Agents follow up with `get_context({repo, session_id})` for
//! the one they pick. The lean shape keeps loop context small.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::lifecycle::{ContentHash, SessionId};
use crate::mcp_response::compute_plan_worktree_status_parts;
use crate::projection::{impl_gate_for, phase, plan_gate_for, waiting_on};
use crate::repo_state::{
    Phase, PlanWorktreeStatus, Trinity, WaitingReason, WaitingRole,
};
use crate::review_state::ReviewGateDecision;
use crate::runtime::Runtime;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Deserialize)]
pub struct WaitArgs {
    pub role: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub exclude_authors: Vec<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct WaitResponse {
    pub matches: Vec<Match>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub repo: String,
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("invalid role: {0} (expected `master` or `reviewers`)")]
    InvalidRole(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Block until at least one session needs the caller's role, then return
/// matches. Returns `{matches: [], timed_out: true}` after `timeout_secs`.
pub async fn wait_for_work(runtime: &Runtime, args: WaitArgs) -> Result<WaitResponse, WaitError> {
    let role = parse_role(&args.role)?;
    let timeout = Duration::from_secs(
        args.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS),
    );
    let started_at = Instant::now();
    let mut rx = runtime.subscribe_events();

    let initial = compute_matches(runtime, &args, role).await?;
    if !initial.is_empty() {
        return Ok(WaitResponse {
            matches: initial,
            timed_out: false,
        });
    }

    let deadline = started_at + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(WaitResponse {
                matches: Vec::new(),
                timed_out: true,
            });
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_event)) => {
                let next = compute_matches(runtime, &args, role).await?;
                if !next.is_empty() {
                    return Ok(WaitResponse {
                        matches: next,
                        timed_out: false,
                    });
                }
            }
            Ok(Err(RecvError::Lagged(_))) => {
                rx = runtime.subscribe_events();
                let next = compute_matches(runtime, &args, role).await?;
                if !next.is_empty() {
                    return Ok(WaitResponse {
                        matches: next,
                        timed_out: false,
                    });
                }
            }
            Ok(Err(RecvError::Closed)) => {
                return Ok(WaitResponse {
                    matches: Vec::new(),
                    timed_out: true,
                });
            }
            Err(_elapsed) => {
                return Ok(WaitResponse {
                    matches: Vec::new(),
                    timed_out: true,
                });
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

/// Snapshot candidates under the runtime mutex, release it, then per-
/// candidate compute the worktree status from disk (no lock held) and
/// hand the materialized inputs to the pure matcher.
async fn compute_matches(
    runtime: &Runtime,
    args: &WaitArgs,
    role: WaitingRole,
) -> Result<Vec<Match>, WaitError> {
    let candidates = {
        let trinity_arc = runtime.state();
        let trinity = trinity_arc.lock().await;
        collect_candidates(&trinity, args)
    };

    let mut with_status: Vec<(Candidate, PlanWorktreeStatus)> = Vec::with_capacity(candidates.len());
    for c in candidates {
        let status =
            compute_plan_worktree_status_parts(&c.repo_root, &c.plan_path, &c.body_hash)?;
        with_status.push((c, status));
    }

    Ok(match_candidates(&with_status, args, role))
}

/// Pure inner matcher. Decides which `(candidate, status)` pairs match
/// the caller's filters. No I/O, no locks.
fn match_candidates(
    input: &[(Candidate, PlanWorktreeStatus)],
    args: &WaitArgs,
    role: WaitingRole,
) -> Vec<Match> {
    let mut out = Vec::new();
    for (cand, status) in input {
        let w = waiting_on(
            cand.session_phase,
            *status,
            cand.plan_gate.as_ref(),
            cand.impl_gate.as_ref(),
        );
        if w.role != role {
            continue;
        }
        if matches!(role, WaitingRole::Reviewers)
            && !args.exclude_authors.is_empty()
            && excluded_author_has_current_verdict(cand, w.reason, &args.exclude_authors)
        {
            continue;
        }
        out.push(Match {
            repo: cand.repo_root.to_string_lossy().into_owned(),
            session_id: cand.session_id.as_str().to_string(),
            reason: w.reason.as_str().to_string(),
        });
    }
    out
}

fn excluded_author_has_current_verdict(
    cand: &Candidate,
    reason: WaitingReason,
    excluded: &[String],
) -> bool {
    let gate = match reason {
        WaitingReason::PlanNeedsInitialReview | WaitingReason::PlanNeedsRereview => {
            cand.plan_gate.as_ref()
        }
        WaitingReason::ImplNeedsInitialReview | WaitingReason::ImplNeedsRereview => {
            cand.impl_gate.as_ref()
        }
        _ => return false,
    };
    let Some(gate) = gate else { return false };
    excluded.iter().any(|excluded_label| {
        gate.approvals
            .iter()
            .chain(gate.request_changes.iter())
            .any(|a| a.as_str() == excluded_label)
    })
}

#[derive(Debug, Clone)]
struct Candidate {
    repo_root: PathBuf,
    session_id: SessionId,
    plan_path: PathBuf,
    body_hash: ContentHash,
    session_phase: Phase,
    plan_gate: Option<ReviewGateDecision>,
    impl_gate: Option<ReviewGateDecision>,
}

/// Collect candidates from the current `Trinity` state. Cheap derivations
/// only — no disk reads, no heavy walks beyond what `*_gate_for` already
/// does over the in-memory feedback maps. Holds the caller's mutex
/// implicitly via `&Trinity`; finish quickly.
fn collect_candidates(trinity: &Trinity, args: &WaitArgs) -> Vec<Candidate> {
    let canonical_filter: Option<PathBuf> = args.repo.as_ref().map(|p| {
        let raw = PathBuf::from(p);
        dunce::canonicalize(&raw).unwrap_or(raw)
    });
    let session_filter: Option<SessionId> = args
        .session_id
        .as_ref()
        .map(|s| SessionId::from(s.clone()));

    let mut out = Vec::new();
    for (repo_root, repo_state) in &trinity.repos {
        if let Some(filter) = &canonical_filter
            && filter != repo_root
        {
            continue;
        }
        for (session_id, session) in &repo_state.sessions {
            if let Some(sid_filter) = &session_filter
                && sid_filter != session_id
            {
                continue;
            }
            out.push(Candidate {
                repo_root: repo_root.clone(),
                session_id: session_id.clone(),
                plan_path: session.plan_path.clone(),
                body_hash: session.body_hash.clone(),
                session_phase: phase(session, &repo_state.attribution),
                plan_gate: plan_gate_for(session, repo_state),
                impl_gate: impl_gate_for(session, repo_state),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{AgentLabel, content_hash};
    use crate::review_state::{ReviewGateDecision, ReviewGateState, ReviewPhase};

    fn agents(labels: &[&str]) -> Vec<AgentLabel> {
        labels.iter().map(|s| AgentLabel::from(*s)).collect()
    }

    fn args_role(role: &str) -> WaitArgs {
        WaitArgs {
            role: role.to_string(),
            repo: None,
            session_id: None,
            exclude_authors: Vec::new(),
            timeout_secs: None,
        }
    }

    fn candidate(
        repo: &str,
        sid: &str,
        session_phase: Phase,
        plan_gate: Option<ReviewGateDecision>,
        impl_gate: Option<ReviewGateDecision>,
    ) -> Candidate {
        Candidate {
            repo_root: PathBuf::from(repo),
            session_id: SessionId::from(sid),
            plan_path: PathBuf::from(format!(".trinity/plans/{sid}.md")),
            body_hash: content_hash("x"),
            session_phase,
            plan_gate,
            impl_gate,
        }
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

    #[test]
    fn empty_input_yields_no_matches() {
        let out = match_candidates(&[], &args_role("master"), WaitingRole::Master);
        assert!(out.is_empty());
    }

    #[test]
    fn role_match_returns_match() {
        // Planning + no participants → reviewers / plan_needs_initial_review.
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let c = candidate("/r", "s", Phase::Planning, Some(g), None);
        let input = vec![(c, PlanWorktreeStatus::Clean)];
        let out = match_candidates(&input, &args_role("reviewers"), WaitingRole::Reviewers);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].repo, "/r");
        assert_eq!(out[0].session_id, "s");
        assert_eq!(out[0].reason, "plan_needs_initial_review");
    }

    #[test]
    fn role_mismatch_filtered() {
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let c = candidate("/r", "s", Phase::Planning, Some(g), None);
        let input = vec![(c, PlanWorktreeStatus::Clean)];
        // session waiting on reviewers; caller asks for master.
        let out = match_candidates(&input, &args_role("master"), WaitingRole::Master);
        assert!(out.is_empty());
    }

    #[test]
    fn body_dirty_yields_commit_plan_revision_master_match() {
        // BodyDirty preempts gate → master / commit_plan_revision regardless
        // of plan_gate. This is the case the disk-read split exists for.
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::Ready,
            agents(&["alice"]),
            agents(&["alice"]),
            Vec::new(),
            Vec::new(),
        );
        let c = candidate("/r", "dirty", Phase::Planning, Some(g), None);
        let input = vec![(c, PlanWorktreeStatus::BodyDirty)];
        let out = match_candidates(&input, &args_role("master"), WaitingRole::Master);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reason, "commit_plan_revision");
    }

    #[test]
    fn exclude_authors_skips_session_when_excluded_has_current_verdict() {
        // Two participants on plan target; only one has voted (so still
        // needs_review). Caller excludes alice (who APPROVED). Should skip.
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            agents(&["alice", "bob"]),
            agents(&["alice"]),
            Vec::new(),
            agents(&["bob"]),
        );
        let c = candidate("/r", "s", Phase::Planning, Some(g), None);
        let input = vec![(c, PlanWorktreeStatus::Clean)];
        let args = WaitArgs {
            role: "reviewers".to_string(),
            repo: None,
            session_id: None,
            exclude_authors: vec!["alice".to_string()],
            timeout_secs: None,
        };
        let out = match_candidates(&input, &args, WaitingRole::Reviewers);
        assert!(
            out.is_empty(),
            "alice already voted; should not wake alice's caller"
        );
    }

    #[test]
    fn exclude_authors_does_not_skip_when_excluded_has_not_voted() {
        // Bob hasn't voted yet; caller excludes bob. The session is in
        // initial-review (no participants), so bob's vote is still needed.
        // Returning the match is correct — bob hasn't yet acted here.
        let g = gate(
            ReviewPhase::Plan,
            ReviewGateState::NeedsReview,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let c = candidate("/r", "fresh", Phase::Planning, Some(g), None);
        let input = vec![(c, PlanWorktreeStatus::Clean)];
        let args = WaitArgs {
            role: "reviewers".to_string(),
            repo: None,
            session_id: None,
            exclude_authors: vec!["bob".to_string()],
            timeout_secs: None,
        };
        let out = match_candidates(&input, &args, WaitingRole::Reviewers);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn done_move_pending_routes_to_master() {
        let c = candidate("/r", "moved", Phase::Planning, None, None);
        let input = vec![(c, PlanWorktreeStatus::DoneMovePending)];
        let out = match_candidates(&input, &args_role("master"), WaitingRole::Master);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reason, "commit_done_move");
    }

    #[test]
    fn done_phase_never_matches() {
        let c = candidate("/r", "done", Phase::Done, None, None);
        let input = vec![(c, PlanWorktreeStatus::Clean)];
        for role in [WaitingRole::Master, WaitingRole::Reviewers] {
            let role_s = match role {
                WaitingRole::Master => "master",
                WaitingRole::Reviewers => "reviewers",
                WaitingRole::None => unreachable!(),
            };
            let out = match_candidates(&input, &args_role(role_s), role);
            assert!(out.is_empty(), "done session must not match role={role_s}");
        }
    }

    #[test]
    fn parse_role_rejects_unknown() {
        assert!(matches!(parse_role("reviewer"), Err(WaitError::InvalidRole(_))));
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
    //! End-to-end tests using a real `Runtime`, real git tempdirs, and the
    //! broadcast channel. These cover the I/O orchestration around the
    //! pure matcher.

    use super::*;
    use crate::fs_watcher::FilesystemSignal;
    use crate::lifecycle::CommitSha;
    use crate::runtime::Runtime;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

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

    fn args(role: &str) -> WaitArgs {
        WaitArgs {
            role: role.to_string(),
            repo: None,
            session_id: None,
            exclude_authors: Vec::new(),
            timeout_secs: Some(2),
        }
    }

    #[tokio::test]
    async fn immediate_return_when_state_already_matches() {
        // A freshly-committed plan with no reviews is already in
        // `reviewers / plan_needs_initial_review` — wait_for_work should
        // return without waiting for any event.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let start = std::time::Instant::now();
        let resp = wait_for_work(&rt, args("reviewers")).await.unwrap();
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(!resp.timed_out);
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].session_id, "foo");
        assert_eq!(resp.matches[0].reason, "plan_needs_initial_review");
    }

    #[tokio::test]
    async fn body_dirty_immediately_matches_master() {
        // The disk-read split exists for this case: the in-memory state
        // says the plan is clean (HEAD blob matches body_hash), but the
        // working tree has an uncommitted edit. compute_matches must read
        // disk to see body_dirty and route to master.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Edit without committing — disk diverges from HEAD.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 uncommitted\n");

        let resp = wait_for_work(&rt, args("master")).await.unwrap();
        assert!(!resp.timed_out);
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].reason, "commit_plan_revision");
    }

    #[tokio::test]
    async fn timeout_returns_with_empty_matches() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        // Only reviewers have work — caller is master. Should time out.
        let mut a = args("master");
        a.timeout_secs = Some(1);
        let start = std::time::Instant::now();
        let resp = wait_for_work(&rt, a).await.unwrap();
        let elapsed = start.elapsed();
        assert!(resp.timed_out);
        assert!(resp.matches.is_empty());
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(2),
            "expected ~1s elapsed, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn wakes_on_plan_file_changed_event() {
        // Caller polls for master work; nothing matches initially. Edit
        // the plan file (without committing) and dispatch a
        // PlanFileChanged signal — the broadcast fires and wait_for_work
        // re-checks, sees body_dirty, returns a master match.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");

        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let join = tokio::spawn(async move {
            let mut a = args("master");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        // Give the wait task time to enter the broadcast subscribe loop.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Make the plan dirty and tell the runtime.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 unsaved\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: SessionId::from("foo"),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            42,
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return in time")
            .unwrap()
            .unwrap();
        assert!(!resp.timed_out);
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].reason, "commit_plan_revision");
    }

    #[tokio::test]
    async fn wakes_on_request_changes_feedback() {
        // Caller polls for master. Start with a fresh plan (waiting on
        // reviewers). Write a REQUEST_CHANGES feedback file → role flips
        // to master with address_plan_request_changes.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo")].plan_intro.clone()
            })
            .await
            .unwrap();

        let rt2 = std::sync::Arc::clone(&rt);
        let join = tokio::spawn(async move {
            let mut a = args("master");
            a.timeout_secs = Some(5);
            wait_for_work(&rt2, a).await
        });

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Drop a REQUEST_CHANGES feedback file at the canonical path.
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/bob.md", intro.as_str());
        write_file(
            dir.path(),
            &feedback_rel,
            "REQUEST_CHANGES\n\nNeeds revision.\n",
        );
        let parsed_rel = PathBuf::from(format!("foo/plan/{}/bob.md", intro.as_str()));
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            1,
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(3), join)
            .await
            .expect("wait_for_work didn't return in time")
            .unwrap()
            .unwrap();
        assert!(!resp.timed_out);
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].reason, "address_plan_request_changes");
    }

    #[tokio::test]
    async fn exclude_authors_skips_session_in_runtime() {
        // alice already APPROVED the current plan target; bob hasn't.
        // Polling as reviewers with exclude_authors=["alice"] must NOT
        // return this session (alice already acted; bob still needs to).
        // The gate is in NeedsReview because bob is a stale participant.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add foo");

        let rt = Runtime::new();
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let intro: CommitSha = rt
            .read_repo(dir.path(), |s| {
                s.sessions[&SessionId::from("foo")].plan_intro.clone()
            })
            .await
            .unwrap();

        // Make alice a participant by approving an earlier (here: same)
        // target.  This puts the gate in Ready, which routes the role to
        // master — not what we want. Instead seed alice's APPROVE and
        // bob's prior APPROVE on a different target so they're both
        // participants, then we bump the target. Simpler: just write
        // alice's APPROVE; gate becomes Ready → master → reviewers role
        // won't match anyway. So the test of exclude_authors needs a
        // stale-participant setup.

        // Setup: alice approves intro, then a new plan revision lands so
        // alice's vote is stale and bob has never voted. To exercise
        // exclude_authors, we need both alice and bob to have voted on
        // an EARLIER target (so they're participants), and only alice to
        // have re-voted on the current target.

        // Round 1: both vote on intro.
        let f_alice_intro = format!(".trinity/feedback/foo/plan/{}/alice.md", intro.as_str());
        let f_bob_intro = format!(".trinity/feedback/foo/plan/{}/bob.md", intro.as_str());
        write_file(dir.path(), &f_alice_intro, "APPROVE\n");
        write_file(dir.path(), &f_bob_intro, "APPROVE\n");

        // Push a new plan revision so the target SHA changes.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2\n");
        commit(dir.path(), "revise foo");
        rt.handle_signal(dir.path(), FilesystemSignal::HeadChanged, 1)
            .await
            .unwrap();

        // After rebuild, the target SHA is the new commit. alice + bob are
        // participants, no one has voted yet on the new target → gate is
        // NeedsReview with missing_approvals=[alice, bob].
        // Now alice re-approves the new target.
        let target: CommitSha = rt
            .read_repo(dir.path(), |s| {
                crate::projection::latest_plan_touching_commit(
                    &s.sessions[&SessionId::from("foo")],
                    s,
                )
                .unwrap()
            })
            .await
            .unwrap();
        let f_alice_v2 = format!(".trinity/feedback/foo/plan/{}/alice.md", target.as_str());
        write_file(dir.path(), &f_alice_v2, "APPROVE\n");
        let parsed_rel = PathBuf::from(format!("foo/plan/{}/alice.md", target.as_str()));
        let parsed = crate::disk_format::parse_feedback_path(&parsed_rel).unwrap();
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::FeedbackWritten { parsed },
            2,
        )
        .await
        .unwrap();

        // Now alice has a current verdict; bob doesn't. The session is in
        // reviewers / plan_needs_rereview.
        // exclude_authors=["alice"] should skip the session.
        let a = WaitArgs {
            role: "reviewers".to_string(),
            repo: None,
            session_id: None,
            exclude_authors: vec!["alice".to_string()],
            timeout_secs: Some(1),
        };
        let resp = wait_for_work(&rt, a).await.unwrap();
        assert!(
            resp.timed_out && resp.matches.is_empty(),
            "alice already voted on the current target; should not wake alice's caller. \
             Got: matches={:?} timed_out={}",
            resp.matches,
            resp.timed_out
        );

        // Without exclude_authors, the same poll should match (bob still owes).
        let a = WaitArgs {
            role: "reviewers".to_string(),
            repo: None,
            session_id: None,
            exclude_authors: Vec::new(),
            timeout_secs: Some(1),
        };
        let resp = wait_for_work(&rt, a).await.unwrap();
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].reason, "plan_needs_rereview");
    }

    #[tokio::test]
    async fn fan_out_two_concurrent_waits_both_wake() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add foo");

        let rt = std::sync::Arc::new(Runtime::new());
        rt.add_repo(dir.path().to_path_buf()).await.unwrap();

        let rt_a = std::sync::Arc::clone(&rt);
        let rt_b = std::sync::Arc::clone(&rt);
        let join_a = tokio::spawn(async move {
            let mut a = args("master");
            a.timeout_secs = Some(5);
            wait_for_work(&rt_a, a).await
        });
        let join_b = tokio::spawn(async move {
            let mut a = args("master");
            a.timeout_secs = Some(5);
            wait_for_work(&rt_b, a).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 unsaved\n");
        rt.handle_signal(
            dir.path(),
            FilesystemSignal::PlanFileChanged {
                session_id: SessionId::from("foo"),
                path: PathBuf::from(".trinity/plans/foo.md"),
            },
            1,
        )
        .await
        .unwrap();

        let resp_a = tokio::time::timeout(Duration::from_secs(3), join_a)
            .await
            .expect("a didn't return")
            .unwrap()
            .unwrap();
        let resp_b = tokio::time::timeout(Duration::from_secs(3), join_b)
            .await
            .expect("b didn't return")
            .unwrap()
            .unwrap();
        assert_eq!(resp_a.matches.len(), 1);
        assert_eq!(resp_b.matches.len(), 1);
        assert_eq!(resp_a.matches[0].session_id, resp_b.matches[0].session_id);
    }

    #[tokio::test]
    async fn repo_filter_restricts_matches() {
        let dir_a = init_repo();
        write_file(dir_a.path(), ".trinity/plans/in_a.md", "# a\n");
        commit(dir_a.path(), "add a");
        let dir_b = init_repo();
        write_file(dir_b.path(), ".trinity/plans/in_b.md", "# b\n");
        commit(dir_b.path(), "add b");

        let rt = Runtime::new();
        rt.add_repo(dir_a.path().to_path_buf()).await.unwrap();
        rt.add_repo(dir_b.path().to_path_buf()).await.unwrap();

        let mut a = args("reviewers");
        a.repo = Some(dir_b.path().to_string_lossy().into_owned());
        a.timeout_secs = Some(1);
        let resp = wait_for_work(&rt, a).await.unwrap();
        assert_eq!(resp.matches.len(), 1);
        assert_eq!(resp.matches[0].session_id, "in_b");
    }
}

