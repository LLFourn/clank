//! MCP / HTTP response shaping over the filesystem-truth core.
//!
//! Response builders construct typed `trinity_wire` DTOs from the
//! daemon's internal state. The MCP dispatch (`server::mcp`) is
//! the single point where these DTOs hit the protocol envelope
//! via `serde_json::to_value`. UI response builders
//! (`ui_response.rs`) own their own conversion to typed DTOs in
//! Phase 5.

use std::path::Path;

use crate::lifecycle::{AgentLabel, ContentHash, content_hash};
use crate::projection::{
    all_implementation_commits, all_plan_revisions, current_posture, expected_action,
    impl_gate_for, plan_gate_for, plan_worktree_status, waiting_on,
};
use crate::repo_state::{Plan, PlanWorktreeStatus, RepoState};
use crate::review_state::CommitGate;
use trinity_wire::dto::{
    ArchivedCycle, CommitRef, CommitRow, FeedbackSummary, GetContextResponse, ListPlansResponse,
    PlanConflict, PlanRow, PrHint, PrHintOption, ReviewGate, ReviewTarget, TimelineEvent,
    WaitingOn as WireWaitingOn, WriteFeedback,
};
use trinity_wire::vocab::{CommitGateState, CommitKind, PlanTouchKind, ReviewTargetPhase};

pub trait PlanStatusReader {
    fn compute(
        &self,
        repo_root: &Path,
        plan_path: &Path,
        body_hash: &ContentHash,
    ) -> std::io::Result<PlanWorktreeStatus>;
}

pub struct DiskPlanStatusReader;

impl PlanStatusReader for DiskPlanStatusReader {
    fn compute(
        &self,
        repo_root: &Path,
        plan_path: &Path,
        body_hash: &ContentHash,
    ) -> std::io::Result<PlanWorktreeStatus> {
        compute_plan_worktree_status_parts(repo_root, plan_path, body_hash)
    }
}

/// Compare the working-tree plan file to HEAD's blob hash using only
/// copied snapshot fields.
pub fn compute_plan_worktree_status_parts(
    repo_root: &Path,
    plan_path: &Path,
    body_hash: &ContentHash,
) -> std::io::Result<PlanWorktreeStatus> {
    let active_path = repo_root.join(plan_path);
    let wt_hash = match std::fs::read_to_string(&active_path) {
        Ok(body) => Some(content_hash(&body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    Ok(plan_worktree_status(Some(body_hash), wt_hash.as_ref()))
}

// ============================================================
// list_plans
// ============================================================

pub fn list_plans_response(state: &RepoState) -> std::io::Result<ListPlansResponse> {
    list_plans_response_with_status_reader(state, &DiskPlanStatusReader)
}

pub(crate) fn list_plans_response_with_status_reader(
    state: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<ListPlansResponse> {
    let mut plans = Vec::with_capacity(state.plans.len());
    for plan in state.plans.values() {
        let worktree_status =
            status_reader.compute(&state.root, &plan.plan_path, &plan.body_hash)?;
        // Active plan whose file has been uncommitted-deleted —
        // totally hidden until the operator restores or commits
        // the deletion. See Plan::is_visible.
        if !plan.is_visible(worktree_status) {
            continue;
        }
        plans.push(build_plan_row(&state.root, plan, worktree_status, state));
    }
    let conflicts = state
        .plan_conflicts
        .iter()
        .map(|(key, paths)| PlanConflict {
            plan_id: plan_id_string(&state.root, key),
            slug: key.as_str().to_string(),
            paths: paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
        })
        .collect();
    Ok(ListPlansResponse { plans, conflicts })
}

fn build_plan_row(
    repo_root: &Path,
    plan: &Plan,
    worktree_status: PlanWorktreeStatus,
    state: &RepoState,
) -> PlanRow {
    let plan_phase = current_posture(plan, state);
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan);
    let w = waiting_on(plan.is_frozen(), worktree_status, gate);
    let lifecycle = plan.lifecycle();
    PlanRow {
        repo: repo_root.to_string_lossy().to_string(),
        plan_id: plan_id_string(repo_root, &plan.id),
        slug: plan.id.as_str().to_string(),
        state: lifecycle,
        lifecycle,
        current_path: plan.plan_path.to_string_lossy().to_string(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        waiting_on: build_waiting_on(&w),
        archived_cycles: plan.archived_cycles.iter().map(build_archived).collect(),
        last_activity_ts: crate::projection::last_activity_ts_for(plan),
    }
}

// ============================================================
// get_context
// ============================================================

pub fn get_context_response(
    snapshot: &RepoState,
    author_label: &AgentLabel,
) -> std::io::Result<Option<GetContextResponse>> {
    get_context_response_with_status_reader(snapshot, author_label, &DiskPlanStatusReader)
}

pub(crate) fn get_context_response_with_status_reader(
    snapshot: &RepoState,
    author_label: &AgentLabel,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Option<GetContextResponse>> {
    let plan = snapshot
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let worktree_status =
        status_reader.compute(&snapshot.root, &plan.plan_path, &plan.body_hash)?;
    if !plan.is_visible(worktree_status) {
        return Ok(None);
    }
    Ok(Some(get_context_response_from_snapshot(
        snapshot,
        worktree_status,
        author_label,
    )))
}

pub fn get_context_response_from_snapshot(
    state: &RepoState,
    worktree_status: PlanWorktreeStatus,
    author_label: &AgentLabel,
) -> GetContextResponse {
    let session = state
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let session_phase = current_posture(session, state);
    let plan_gate = plan_gate_for(session, state);
    let impl_gate = impl_gate_for(session, state);
    let gate = crate::projection::latest_reviewable_commit_gate_for(session);
    let w = waiting_on(session.is_frozen(), worktree_status, gate);
    let review_target_phase = posture_to_review_target_phase(session_phase);

    let review_target_sha = crate::projection::latest_reviewable_commit_for(session);
    let review_target = review_target_sha.as_ref().map(|sha| ReviewTarget {
        commit_sha: sha.as_str().to_string(),
        phase: review_target_phase,
    });
    let write_feedback = review_target_sha.as_ref().map(|sha| WriteFeedback {
        phase: review_target_phase,
        target_sha: sha.as_str().to_string(),
        path: format!(
            ".trinity/feedback/{session}/{sha}/{author}.md",
            session = session.id.as_str(),
            sha = sha.as_str(),
            author = author_label.as_str(),
        ),
    });
    let plan_revisions = all_plan_revisions(session, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let implementation_commits = all_implementation_commits(session, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let lifecycle = session.lifecycle();
    let pr_hint = if matches!(session_phase, crate::repo_state::Posture::Implementing) {
        Some(build_pr_hint(session, state))
    } else {
        None
    };

    GetContextResponse {
        repo: state.root.to_string_lossy().to_string(),
        plan_id: plan_id_string(&state.root, &session.id),
        slug: session.id.as_str().to_string(),
        state: lifecycle,
        lifecycle,
        current_path: session.plan_path.to_string_lossy().to_string(),
        phase: session_phase,
        plan_worktree_status: worktree_status,
        waiting_on: build_waiting_on(&w),
        expected_action: expected_action(w.reason),
        review_target,
        write_feedback,
        review_gate: build_review_gate(plan_gate, impl_gate, session_phase),
        latest_plan_revision: all_plan_revisions(session, state)
            .last()
            .map(|s| CommitRef {
                commit_sha: s.as_str().to_string(),
            }),
        latest_implementation_revision: all_implementation_commits(session, state).last().map(
            |s| CommitRef {
                commit_sha: s.as_str().to_string(),
            },
        ),
        plan_revisions,
        implementation_commits,
        commits: build_commits_array(session),
        latest_relevant_commit: crate::projection::latest_reviewable_commit_for(session)
            .map(|s| s.as_str().to_string()),
        timeline: build_timeline(session, state),
        pr_hint,
        archived_cycles: session.archived_cycles.iter().map(build_archived).collect(),
    }
}

// ============================================================
// Composing helpers
// ============================================================

fn plan_id_string(repo_root: &Path, plan_key: &crate::lifecycle::PlanKey) -> Option<String> {
    crate::lifecycle::RepoBasename::from_repo_root(repo_root)
        .map(|b| crate::lifecycle::PlanId::new(b, plan_key.clone()).to_string())
}

fn build_waiting_on(w: &crate::repo_state::WaitingOn) -> WireWaitingOn {
    WireWaitingOn {
        role: w.role,
        reason: w.reason,
        agents: w.agents.iter().map(|a| a.as_str().to_string()).collect(),
        description: w.description.clone(),
    }
}

fn build_archived(c: &crate::repo_state::ArchivedCycleSummary) -> ArchivedCycle {
    ArchivedCycle {
        closer: c.closer.as_str().to_string(),
        approver_count: c.approver_count,
    }
}

fn posture_to_review_target_phase(p: crate::repo_state::Posture) -> ReviewTargetPhase {
    match p {
        crate::repo_state::Posture::Planning => ReviewTargetPhase::Plan,
        crate::repo_state::Posture::Implementing => ReviewTargetPhase::Impl,
    }
}

/// Per-commit `commits[]` array. Chronological (the fold appended
/// events in that order). Only reviewable kinds emitted; `MultiPlan`
/// / `Finalize` are non-reviewable so they're skipped.
fn build_commits_array(plan: &Plan) -> Vec<CommitRow> {
    let mut out = Vec::new();
    for event in &plan.timeline {
        if !event.kind.is_reviewable() {
            continue;
        }
        let gate = event.gate.as_ref().map(build_commit_gate);
        let feedback = event
            .gate
            .as_ref()
            .map(|g| {
                g.feedback
                    .iter()
                    .map(|(author, fb)| FeedbackSummary {
                        author: author.as_str().to_string(),
                        verdict: fb.verdict,
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(CommitRow {
            sha: event.sha.as_str().to_string(),
            kind: event.kind,
            gate,
            feedback,
        });
    }
    out
}

fn build_commit_gate(g: &CommitGate) -> trinity_wire::dto::CommitGate {
    trinity_wire::dto::CommitGate {
        state: g.state,
        participants: g
            .participants
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
        approvers: g.approvers.iter().map(|a| a.as_str().to_string()).collect(),
        requesters: g
            .requesters
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
        ambiguous: g.ambiguous.iter().map(|a| a.as_str().to_string()).collect(),
        missing: g.missing.iter().map(|a| a.as_str().to_string()).collect(),
        feedback: g
            .feedback
            .iter()
            .map(|(author, fb)| {
                (
                    author.as_str().to_string(),
                    trinity_wire::dto::Feedback {
                        author: author.as_str().to_string(),
                        verdict: fb.verdict,
                        body_raw: fb.body.clone(),
                        body_html: String::new(),
                        path: fb.path.to_string_lossy().to_string(),
                        created_at: fb.created_at,
                    },
                )
            })
            .collect(),
    }
}

fn build_timeline(plan: &Plan, _state: &RepoState) -> Vec<TimelineEvent> {
    let mut out = Vec::with_capacity(plan.timeline.len() * 2);
    for event in &plan.timeline {
        let sha = event.sha.as_str().to_string();
        let subject = event.subject.clone();
        let plan_touch = if event.sha == plan.plan_intro {
            PlanTouchKind::Intro
        } else {
            PlanTouchKind::Revision
        };
        let row = match event.kind {
            CommitKind::PlanOnly => TimelineEvent::CommitPlan {
                sha: sha.clone(),
                subject,
                plan_touch,
            },
            CommitKind::CodeOnly => TimelineEvent::CommitImpl {
                sha: sha.clone(),
                subject,
            },
            CommitKind::Mixed => TimelineEvent::CommitMixed {
                sha: sha.clone(),
                subject,
                plan_touch,
            },
            CommitKind::MultiPlan => TimelineEvent::CommitMultiPlan {
                sha: sha.clone(),
                subject,
                plan_touch,
            },
            CommitKind::Finalize => TimelineEvent::CommitFinalize {
                sha: sha.clone(),
                subject,
            },
            CommitKind::Unattributed => continue,
        };
        out.push(row);
        if let Some(gate) = event.gate.as_ref() {
            let phase = match event.kind {
                CommitKind::PlanOnly | CommitKind::Mixed | CommitKind::MultiPlan => {
                    ReviewTargetPhase::Plan
                }
                CommitKind::CodeOnly => ReviewTargetPhase::Impl,
                CommitKind::Finalize | CommitKind::Unattributed => continue,
            };
            for (author, fb) in &gate.feedback {
                out.push(TimelineEvent::Review {
                    target: sha.clone(),
                    author: author.as_str().to_string(),
                    verdict: fb.verdict,
                    phase,
                    created_at: fb.created_at,
                });
            }
        }
    }
    out
}

fn build_pr_hint(session: &Plan, state: &RepoState) -> PrHint {
    let impl_commits: Vec<String> = all_implementation_commits(session, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let plan_intro = session.plan_intro.as_str().to_string();
    let plan_intro_parent = session
        .plan_intro_parent
        .as_ref()
        .map(|s| s.as_str().to_string());
    let base_for_squash = plan_intro_parent
        .clone()
        .unwrap_or_else(|| plan_intro.clone());
    let plan_path = session.plan_path.to_string_lossy().to_string();
    let suggested = format!("Implement {}", session.id.as_str());
    let options = vec![
        PrHintOption {
            name: "keep_plan_in_pr".to_string(),
            base: base_for_squash.clone(),
            command: format!("git reset --soft {base_for_squash} && git commit -m '{suggested}'"),
        },
        PrHintOption {
            name: "exclude_plan_from_pr".to_string(),
            base: base_for_squash.clone(),
            command: format!(
                "git reset --soft {base_for_squash} && git rm {plan_path} && git commit -m '{suggested}'"
            ),
        },
    ];
    PrHint {
        plan_intro,
        plan_intro_parent,
        implementation_commits: impl_commits,
        options,
        suggested_message: suggested,
    }
}

fn build_review_gate(
    plan_gate: Option<&CommitGate>,
    impl_gate: Option<&CommitGate>,
    session_phase: crate::repo_state::Posture,
) -> Option<ReviewGate> {
    let phase = posture_to_review_target_phase(session_phase);
    let gate = match session_phase {
        crate::repo_state::Posture::Planning => plan_gate,
        crate::repo_state::Posture::Implementing => impl_gate,
    }?;
    Some(ReviewGate {
        state: legacy_gate_state_wire(gate.state).to_string(),
        phase,
        participants: gate
            .participants
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
        approvals: gate
            .approvers
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
        request_changes: gate
            .requesters
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
        missing_approvals: gate
            .missing
            .iter()
            .map(|a| a.as_str().to_string())
            .collect(),
    })
}

/// Legacy `review_gate.state` wire vocabulary — `ready`,
/// `needs_review`, `changes_requested`. Preserved across the
/// `ReviewGateDecision` → `CommitGate` switch. The per-commit
/// `commits[].gate.state` field uses the `CommitGateState` enum
/// directly (snake_case → `approved`/`unreviewed`/`changes_requested`).
pub(crate) fn legacy_gate_state_wire(s: CommitGateState) -> &'static str {
    match s {
        CommitGateState::Approved => "ready",
        CommitGateState::Unreviewed => "needs_review",
        CommitGateState::ChangesRequested => "changes_requested",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::PlanKey;
    use crate::rebuild::rebuild_repo;
    use crate::runtime::Runtime;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;
    use trinity_wire::dto::PlanRow;
    use trinity_wire::vocab::{
        PlanLifecycle, PlanWorktreeStatus as Pws, Posture, WaitingReason, WaitingRole,
    };

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

    fn status_for_session(repo: &Path, session: &Plan) -> PlanWorktreeStatus {
        compute_plan_worktree_status_parts(repo, &session.plan_path, &session.body_hash).unwrap()
    }

    fn context_from_state(
        state: &RepoState,
        sid: &str,
        author: &str,
    ) -> Option<GetContextResponse> {
        let sid = PlanKey::parse(sid).unwrap();
        let snapshot = state.single_plan(&sid)?;
        get_context_response(&snapshot, &AgentLabel::parse(author).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn plan_worktree_clean_after_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.plans.values().next().unwrap();
        assert_eq!(status_for_session(dir.path(), session), Pws::Clean);
    }

    #[tokio::test]
    async fn plan_worktree_body_dirty_after_edit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add plan v1");
        write_file(
            dir.path(),
            ".trinity/plans/foo.md",
            "# foo v2 uncommitted\n",
        );
        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.plans.values().next().unwrap();
        assert_eq!(status_for_session(dir.path(), session), Pws::BodyDirty);
    }

    #[tokio::test]
    async fn plan_worktree_plan_file_missing() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.plans.values().next().unwrap();
        assert_eq!(
            status_for_session(dir.path(), session),
            Pws::PlanFileMissing
        );
    }

    #[tokio::test]
    async fn unfrozen_plan_with_missing_worktree_file_is_hidden_from_list_plans() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        let state = rebuild_repo(dir.path()).await.unwrap();
        let resp = list_plans_response(&state).unwrap();
        assert!(
            resp.plans.is_empty(),
            "active plan with missing worktree file must be hidden; got {:?}",
            resp.plans
        );
        let snapshot = state.single_plan(&PlanKey::parse("foo").unwrap()).unwrap();
        let ctx = get_context_response(&snapshot, &AgentLabel::parse("master").unwrap()).unwrap();
        assert!(
            ctx.is_none(),
            "get_context must return None for hidden plan"
        );
    }

    #[tokio::test]
    async fn frozen_plan_with_missing_worktree_file_stays_visible() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        write_file(
            dir.path(),
            ".trinity/finished/foo/alice.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        let state = rebuild_repo(dir.path()).await.unwrap();
        let resp = list_plans_response(&state).unwrap();
        assert_eq!(resp.plans.len(), 1);
        assert_eq!(resp.plans[0].lifecycle, PlanLifecycle::Finished);
    }

    #[tokio::test]
    async fn list_plans_response_includes_waiting_on() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let resp = list_plans_response(&state).unwrap();
        assert_eq!(resp.plans.len(), 1);
        let row = &resp.plans[0];
        assert_eq!(row.slug, "foo");
        assert_eq!(row.current_path, ".trinity/plans/foo.md");
        assert_eq!(row.lifecycle, PlanLifecycle::Active);
        assert_eq!(row.state, PlanLifecycle::Active); // legacy alias
        assert_eq!(row.phase, Posture::Planning);
        assert_eq!(row.plan_worktree_status, Pws::Clean);
        assert_eq!(row.waiting_on.role, WaitingRole::Reviewers);
        assert_eq!(row.waiting_on.reason, WaitingReason::CommitNeedsReview);
        assert!(resp.conflicts.is_empty());
    }

    #[tokio::test]
    async fn get_context_returns_none_for_unknown_session() {
        let dir = init_repo();
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "missing", "reviewer");
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn get_context_waiting_on_master_when_plan_dirty() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v1\n");
        commit(dir.path(), "add v1");
        write_file(dir.path(), ".trinity/plans/foo.md", "# v2 uncommitted\n");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
        assert_eq!(v.plan_worktree_status, Pws::BodyDirty);
        assert_eq!(v.waiting_on.role, WaitingRole::Master);
        assert_eq!(v.waiting_on.reason, WaitingReason::CommitPlanRevision);
    }

    #[tokio::test]
    async fn get_context_phase_implementing_after_code_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        write_file(dir.path(), "src/foo.rs", "fn x() {}\n");
        commit(dir.path(), "impl foo");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
        assert_eq!(v.phase, Posture::Implementing);
        assert_eq!(v.waiting_on.role, WaitingRole::Reviewers);
        assert_eq!(v.waiting_on.reason, WaitingReason::CommitNeedsReview);
        assert!(v.latest_implementation_revision.is_some());
    }

    #[tokio::test]
    async fn get_context_request_changes_routes_to_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.plans[&PlanKey::parse("foo").unwrap()]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/{}/codex.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "REQUEST_CHANGES\n\nMissing X.\n");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
        assert_eq!(v.waiting_on.role, WaitingRole::Master);
        assert_eq!(v.waiting_on.reason, WaitingReason::AddressCommitChanges);
        assert_eq!(v.waiting_on.agents, vec!["codex".to_string()]);
    }

    #[tokio::test]
    async fn get_context_approve_routes_to_master_ready_to_implement() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.plans[&PlanKey::parse("foo").unwrap()]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLGTM.\n");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "master").unwrap();
        assert_eq!(v.waiting_on.role, WaitingRole::Master);
        assert_eq!(
            v.waiting_on.reason,
            WaitingReason::ReadyToStartImplementation
        );
    }

    /// The legacy `review_gate` wire shape (state=ready, request_changes,
    /// missing_approvals, etc.) is daemon-side mapping. We still test
    /// the field naming holds on the serialized form.
    #[tokio::test]
    async fn review_gate_wire_keys_stable_after_commit_gate_switch() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.plans[&PlanKey::parse("foo").unwrap()]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLGTM.\n");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "master").unwrap();
        let gate = v
            .review_gate
            .expect("approved plan must have a review_gate");
        assert_eq!(gate.state, "ready");
        assert_eq!(gate.phase, ReviewTargetPhase::Plan);
        assert_eq!(gate.approvals, vec!["alice".to_string()]);
        assert!(gate.request_changes.is_empty());
        assert!(gate.missing_approvals.is_empty());
        // Round-trip through serde to assert the wire-key names
        // stayed stable across the typed-DTO migration.
        let wire = serde_json::to_value(&gate).unwrap();
        for key in [
            "state",
            "phase",
            "participants",
            "approvals",
            "request_changes",
            "missing_approvals",
        ] {
            assert!(wire.get(key).is_some(), "missing wire key {key}: {wire}");
        }
        for forbidden in ["approvers", "requesters", "missing", "ambiguous"] {
            assert!(
                wire.get(forbidden).is_none(),
                "review_gate must not expose Rust field `{forbidden}`: {wire}"
            );
        }
    }

    struct BlockingStatusReader {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl PlanStatusReader for BlockingStatusReader {
        fn compute(
            &self,
            _repo_root: &Path,
            _plan_path: &Path,
            _body_hash: &ContentHash,
        ) -> std::io::Result<PlanWorktreeStatus> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(PlanWorktreeStatus::Clean)
        }
    }

    #[tokio::test]
    async fn blocked_status_reader_does_not_hold_runtime_mutex() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let runtime = Runtime::new();
        runtime.add_repo(dir.path().to_path_buf()).await.unwrap();
        let snapshot = runtime
            .snapshot_session(dir.path(), &PlanKey::parse("foo").unwrap())
            .await
            .unwrap()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let reader = BlockingStatusReader {
            entered: entered_tx,
            release: release_rx,
        };
        let author = AgentLabel::parse("reviewer").unwrap();

        let handle = tokio::task::spawn_blocking(move || {
            get_context_response_with_status_reader(&snapshot, &author, &reader)
                .unwrap()
                .expect("plan visible")
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("status reader did not start");

        tokio::time::timeout(Duration::from_secs(1), runtime.snapshot_repo(dir.path()))
            .await
            .expect("runtime mutex was held while status reader blocked")
            .unwrap();

        release_tx.send(()).unwrap();
        let v: GetContextResponse = handle.await.unwrap();
        assert_eq!(v.plan_worktree_status, Pws::Clean);
    }

    #[tokio::test]
    async fn list_plans_lifecycle_active_for_unfrozen_plan() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let resp = list_plans_response(&state).unwrap();
        let row: &PlanRow = &resp.plans[0];
        assert_eq!(row.lifecycle, PlanLifecycle::Active);
        assert!(row.archived_cycles.is_empty());
    }

    #[tokio::test]
    async fn list_plans_lifecycle_finished_after_finalize_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo");
        write_file(
            dir.path(),
            ".trinity/finished/foo/alice.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let resp = list_plans_response(&state).unwrap();
        let row = &resp.plans[0];
        assert_eq!(row.lifecycle, PlanLifecycle::Finished);
        assert_eq!(row.archived_cycles.len(), 1);
        assert_eq!(row.archived_cycles[0].approver_count, 1);
    }
}
