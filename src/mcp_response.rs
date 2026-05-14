//! MCP / HTTP response shaping over the filesystem-truth core.
//!
//! Pure response builders: given a `RepoState` and a request shape,
//! return a `serde_json::Value` matching the MCP / HTTP schema. These
//! functions are the bridge between the pure core (rebuild + projection)
//! and the wire surface.
//!
//! `compute_plan_worktree_status` is the one piece that reaches into the
//! working tree (it has to — by definition it's comparing HEAD to disk).
//! Everything else here operates over the in-memory state.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::disk_format::plan_path_is_done;
use crate::lifecycle::{AgentLabel, ContentHash, SessionId, content_hash};
use crate::projection::{
    all_implementation_commits, all_plan_revisions, impl_gate_for, latest_impl_commit,
    latest_plan_touching_commit, phase, plan_gate_for, plan_worktree_status, waiting_on,
};
use crate::repo_state::{PlanWorktreeStatus, RepoState, WaitingOn};
use crate::review_state::{ReviewGateDecision, ReviewPhase};

/// Compare the working-tree plan file at `session.plan_path` to HEAD's
/// blob (already cached in `session.body_hash`). Returns the four-state
/// `PlanWorktreeStatus`.
///
/// This is the only function in this module that touches the disk; it's
/// called by `get_context_response` to produce a fresh status at request
/// time so the response is never stale.
pub fn compute_plan_worktree_status(
    repo_root: &Path,
    session: &crate::repo_state::Session,
) -> std::io::Result<PlanWorktreeStatus> {
    compute_plan_worktree_status_parts(repo_root, &session.plan_path, &session.body_hash)
}

/// Same as `compute_plan_worktree_status` but takes the minimum inputs
/// directly. Used by `wait_for_work` where we snapshot the path + hash
/// under the runtime lock and want to compute the worktree status without
/// holding a full `Session` reference (so the lock can be released first).
pub fn compute_plan_worktree_status_parts(
    repo_root: &Path,
    plan_path: &Path,
    body_hash: &ContentHash,
) -> std::io::Result<PlanWorktreeStatus> {
    let active_path = repo_root.join(plan_path);
    let counterpart_rel = swap_active_done(plan_path);
    let counterpart_abs = counterpart_rel.as_ref().map(|p| repo_root.join(p));

    let wt_hash = match std::fs::read_to_string(&active_path) {
        Ok(body) => Some(content_hash(&body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let counterpart_exists = counterpart_abs
        .as_ref()
        .map(|p| p.exists())
        .unwrap_or(false);

    Ok(plan_worktree_status(
        Some(body_hash),
        wt_hash.as_ref(),
        counterpart_exists,
    ))
}

/// Given a plan path (relative), return its counterpart in the
/// active↔done flip. `.trinity/plans/<id>.md` ↔ `.trinity/plans/done/<id>.md`.
pub fn swap_active_done(plan_path: &Path) -> Option<PathBuf> {
    let name = plan_path.file_name()?;
    if plan_path_is_done(plan_path) {
        Some(PathBuf::from(".trinity/plans").join(name))
    } else {
        Some(PathBuf::from(".trinity/plans/done").join(name))
    }
}

/// `list_sessions` response over a single repo.
///
/// Returns an array of session summaries: id, plan_path, phase,
/// waiting_on, plan_worktree_status. Each row is enough to drive the
/// homepage's session table without further queries.
pub fn list_sessions_response(repo_root: &Path, state: &RepoState) -> std::io::Result<Value> {
    let mut sessions = Vec::with_capacity(state.sessions.len());
    for session in state.sessions.values() {
        let worktree_status = compute_plan_worktree_status(repo_root, session)?;
        let session_phase = phase(session, &state.attribution);
        let plan_gate = plan_gate_for(session, state);
        let impl_gate = impl_gate_for(session, state);
        let w = waiting_on(
            session_phase,
            worktree_status,
            plan_gate.as_ref(),
            impl_gate.as_ref(),
        );
        sessions.push(session_summary(session, session_phase, worktree_status, &w));
    }
    Ok(Value::Array(sessions))
}

fn session_summary(
    session: &crate::repo_state::Session,
    session_phase: crate::repo_state::Phase,
    worktree_status: PlanWorktreeStatus,
    w: &WaitingOn,
) -> Value {
    json!({
        "id": session.id.as_str(),
        "plan_path": session.plan_path.to_string_lossy(),
        "phase": session_phase.as_str(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(w),
    })
}

/// `get_context` response for a specific session + author.
///
/// Returns `Ok(None)` if the session doesn't exist in HEAD (the caller
/// should map this to `{ error: "session_not_committed", ... }`).
pub fn get_context_response(
    repo_root: &Path,
    state: &RepoState,
    session_id: &SessionId,
    _author_label: &AgentLabel,
) -> std::io::Result<Option<Value>> {
    let Some(session) = state.sessions.get(session_id) else {
        return Ok(None);
    };
    let worktree_status = compute_plan_worktree_status(repo_root, session)?;
    let session_phase = phase(session, &state.attribution);
    let plan_gate = plan_gate_for(session, state);
    let impl_gate = impl_gate_for(session, state);
    let w = waiting_on(
        session_phase,
        worktree_status,
        plan_gate.as_ref(),
        impl_gate.as_ref(),
    );

    let pr_hint = if matches!(session_phase, crate::repo_state::Phase::Implementing) {
        Some(pr_hint_value(session, state))
    } else {
        None
    };

    let plan_feedback = feedback_entries(&session.plan_feedback);
    let impl_feedback = feedback_entries(&session.impl_feedback);
    let timeline = timeline_value(state, session_id);

    let plan_revisions: Vec<String> = all_plan_revisions(session, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let implementation_commits: Vec<String> =
        all_implementation_commits(session, state)
            .into_iter()
            .map(|s| s.as_str().to_string())
            .collect();

    // Canonical write-feedback path + review_target + expected_action for
    // the caller. Tells reviewers exactly where to drop their next file.
    let (review_target_phase, review_target_sha) = match session_phase {
        crate::repo_state::Phase::Planning => (
            "plan",
            latest_plan_touching_commit(session, state).map(|s| s.as_str().to_string()),
        ),
        crate::repo_state::Phase::Implementing => (
            "impl",
            latest_impl_commit(session, state).map(|s| s.as_str().to_string()),
        ),
        crate::repo_state::Phase::Done => ("plan", None),
    };
    let review_target = review_target_sha.as_ref().map(|sha| {
        json!({
            "phase": review_target_phase,
            "commit_sha": sha,
        })
    });
    let write_feedback = review_target_sha.as_ref().map(|sha| {
        let rel = format!(
            ".trinity/feedback/{session}/{phase}/{sha}/{author}.md",
            session = session.id.as_str(),
            phase = review_target_phase,
            sha = sha,
            author = _author_label.as_str(),
        );
        json!({
            "phase": review_target_phase,
            "target_sha": sha,
            "path": rel,
        })
    });
    let expected_action = expected_action_for(&w);

    Ok(Some(json!({
        "session_id": session.id.as_str(),
        "phase": session_phase.as_str(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(&w),
        "expected_action": expected_action,
        "review_target": review_target,
        "write_feedback": write_feedback,
        "plan_path": session.plan_path.to_string_lossy(),
        "review_gate": gate_value(plan_gate.as_ref(), impl_gate.as_ref(), session_phase),
        "latest_plan_revision": latest_plan_revision(session, state),
        "latest_implementation_revision": latest_impl_revision(session, state),
        "plan_revisions": plan_revisions,
        "implementation_commits": implementation_commits,
        "plan_feedback": plan_feedback,
        "impl_feedback": impl_feedback,
        "timeline": timeline,
        "pr_hint": pr_hint,
    })))
}

/// Map `waiting_on.reason` to the caller-facing `expected_action` string
/// that tells the agent what to actually do next.
fn expected_action_for(w: &crate::repo_state::WaitingOn) -> &'static str {
    use crate::repo_state::WaitingReason::*;
    match w.reason {
        SessionDone => "none",
        CommitDoneMove => "commit_done_move",
        RestoreOrCommitDoneMove => "restore_or_commit_done_move",
        CommitPlanRevision => "commit_plan_revision",
        AddressPlanRequestChanges => "address_plan_request_changes",
        ReadyToImplement => "implement_and_commit",
        PlanNeedsInitialReview => "review_plan",
        PlanNeedsRereview => "review_plan",
        AddressImplRequestChanges => "address_impl_request_changes",
        ReadyToFinish => "move_to_done",
        ImplNeedsInitialReview => "review_impl",
        ImplNeedsRereview => "review_impl",
    }
}

/// Serialize the per-session timeline (from `RepoState::timeline_for`)
/// into the response shape. Each event becomes a `{ kind, ... }` object.
fn timeline_value(state: &RepoState, session_id: &SessionId) -> Vec<Value> {
    state
        .timeline_for(session_id)
        .into_iter()
        .map(|e| match e {
            crate::repo_state::TimelineEvent::Commit {
                sha,
                plan_touch,
                has_code_changes,
            } => {
                let kind = match (plan_touch.is_some(), has_code_changes) {
                    (true, true) => "commit_mixed",
                    (true, false) => "commit_plan",
                    (false, true) => "commit_impl",
                    (false, false) => "commit_other",
                };
                json!({
                    "kind": kind,
                    "sha": sha.as_str(),
                    "plan_touch": plan_touch.as_ref().map(|k| k.as_str()),
                    "has_code_changes": has_code_changes,
                })
            }
            crate::repo_state::TimelineEvent::Review {
                phase,
                target,
                author,
                verdict,
            } => json!({
                "kind": "review",
                "phase": phase.as_str(),
                "target": target.as_str(),
                "author": author.as_str(),
                "verdict": verdict.as_str(),
            }),
            crate::repo_state::TimelineEvent::HeldFeedback { author, reason } => json!({
                "kind": "held_feedback",
                "author": author.as_str(),
                "reason": reason,
            }),
        })
        .collect()
}

fn feedback_entries(
    map: &std::collections::BTreeMap<(crate::lifecycle::CommitSha, crate::lifecycle::AgentLabel), crate::repo_state::Feedback>,
) -> Vec<Value> {
    map.iter()
        .map(|((target, author), fb)| {
            json!({
                "target_sha": target.as_str(),
                "author": author.as_str(),
                "verdict": fb.verdict.as_str(),
            })
        })
        .collect()
}

fn pr_hint_value(session: &crate::repo_state::Session, state: &RepoState) -> Value {
    let impl_commits: Vec<String> = all_implementation_commits(session, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();

    let plan_intro = session.plan_intro.as_str();
    let plan_intro_parent = session.plan_intro_parent.as_ref().map(|s| s.as_str());
    let base_for_squash = plan_intro_parent.unwrap_or(plan_intro);
    let plan_path = session.plan_path.to_string_lossy().to_string();
    let suggested = format!("Implement {}", session.id.as_str());

    let mut options = Vec::with_capacity(2);
    options.push(json!({
        "name": "keep_plan_in_pr",
        "base": base_for_squash,
        "command": format!(
            "git reset --soft {base} && git commit -m '{msg}'",
            base = base_for_squash,
            msg = suggested
        ),
    }));
    options.push(json!({
        "name": "exclude_plan_from_pr",
        "base": base_for_squash,
        "command": format!(
            "git reset --soft {base} && git rm {plan} && git commit -m '{msg}'",
            base = base_for_squash,
            plan = plan_path,
            msg = suggested
        ),
    }));

    json!({
        "plan_intro": plan_intro,
        "plan_intro_parent": plan_intro_parent,
        "implementation_commits": impl_commits,
        "options": options,
        "suggested_message": suggested,
    })
}

fn waiting_on_value(w: &WaitingOn) -> Value {
    json!({
        "role": w.role.as_str(),
        "reason": w.reason.as_str(),
        "agents": w.agents.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        "description": w.description,
    })
}

fn gate_value(
    plan_gate: Option<&ReviewGateDecision>,
    impl_gate: Option<&ReviewGateDecision>,
    session_phase: crate::repo_state::Phase,
) -> Value {
    let gate = match session_phase {
        crate::repo_state::Phase::Planning => plan_gate,
        crate::repo_state::Phase::Implementing => impl_gate,
        crate::repo_state::Phase::Done => None,
    };
    match gate {
        Some(g) => json!({
            "state": g.state.as_str(),
            "phase": match g.phase { ReviewPhase::Plan => "plan", ReviewPhase::Impl => "impl" },
            "participants": g.participants.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "approvals": g.approvals.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "request_changes": g.request_changes.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "missing_approvals": g.missing_approvals.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    }
}

fn latest_plan_revision(session: &crate::repo_state::Session, state: &RepoState) -> Value {
    match all_plan_revisions(session, state).last() {
        Some(sha) => json!({ "commit_sha": sha.as_str() }),
        None => Value::Null,
    }
}

fn latest_impl_revision(session: &crate::repo_state::Session, state: &RepoState) -> Value {
    match all_implementation_commits(session, state).last() {
        Some(sha) => json!({ "commit_sha": sha.as_str() }),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::rebuild_repo;
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

    #[tokio::test]
    async fn plan_worktree_clean_after_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.sessions.values().next().unwrap();
        let status = compute_plan_worktree_status(dir.path(), session).unwrap();
        assert_eq!(status, PlanWorktreeStatus::Clean);
    }

    #[tokio::test]
    async fn plan_worktree_body_dirty_after_edit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add plan v1");
        // Edit without committing
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 uncommitted\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.sessions.values().next().unwrap();
        let status = compute_plan_worktree_status(dir.path(), session).unwrap();
        assert_eq!(status, PlanWorktreeStatus::BodyDirty);
    }

    #[tokio::test]
    async fn plan_worktree_done_move_pending() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        // Move to done without committing
        let from = dir.path().join(".trinity/plans/foo.md");
        let to_dir = dir.path().join(".trinity/plans/done");
        std::fs::create_dir_all(&to_dir).unwrap();
        std::fs::rename(from, to_dir.join("foo.md")).unwrap();

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.sessions.values().next().unwrap();
        let status = compute_plan_worktree_status(dir.path(), session).unwrap();
        assert_eq!(status, PlanWorktreeStatus::DoneMovePending);
    }

    #[tokio::test]
    async fn plan_worktree_missing_active_file() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        // Just rm the file without committing or moving.
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.sessions.values().next().unwrap();
        let status = compute_plan_worktree_status(dir.path(), session).unwrap();
        assert_eq!(status, PlanWorktreeStatus::MissingActivePlanFile);
    }

    #[tokio::test]
    async fn list_sessions_response_includes_waiting_on() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = list_sessions_response(dir.path(), &state).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "foo");
        assert_eq!(arr[0]["phase"], "planning");
        assert_eq!(arr[0]["plan_worktree_status"], "clean");
        // Just-committed plan with no reviews → reviewers / plan_needs_initial_review.
        assert_eq!(arr[0]["waiting_on"]["role"], "reviewers");
        assert_eq!(arr[0]["waiting_on"]["reason"], "plan_needs_initial_review");
    }

    #[tokio::test]
    async fn get_context_returns_none_for_unknown_session() {
        let dir = init_repo();
        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = get_context_response(
            dir.path(),
            &state,
            &SessionId::from("missing".to_string()),
            &AgentLabel::from("reviewer".to_string()),
        )
        .unwrap();
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn get_context_waiting_on_master_when_plan_dirty() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# v1\n");
        commit(dir.path(), "add v1");
        // Edit uncommitted
        write_file(dir.path(), ".trinity/plans/foo.md", "# v2 uncommitted\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = get_context_response(
            dir.path(),
            &state,
            &SessionId::from("foo".to_string()),
            &AgentLabel::from("reviewer".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["plan_worktree_status"], "body_dirty");
        assert_eq!(v["waiting_on"]["role"], "master");
        assert_eq!(v["waiting_on"]["reason"], "commit_plan_revision");
    }

    #[tokio::test]
    async fn get_context_phase_implementing_after_code_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");
        write_file(dir.path(), "src/foo.rs", "fn x() {}\n");
        commit(dir.path(), "impl foo");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = get_context_response(
            dir.path(),
            &state,
            &SessionId::from("foo".to_string()),
            &AgentLabel::from("reviewer".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["phase"], "implementing");
        // No impl reviews yet → reviewers / impl_needs_initial_review
        assert_eq!(v["waiting_on"]["role"], "reviewers");
        assert_eq!(v["waiting_on"]["reason"], "impl_needs_initial_review");
        assert!(v["latest_implementation_revision"].is_object());
    }

    #[tokio::test]
    async fn get_context_request_changes_routes_to_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.sessions[&SessionId::from("foo".to_string())]
            .plan_intro
            .clone();
        // Use full SHA in the feedback path. Trinity in production will
        // tell agents the canonical (full-SHA) path via get_context;
        // prefix resolution at ingest is a future enhancement.
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/codex.md", intro.as_str());
        write_file(
            dir.path(),
            &feedback_rel,
            "REQUEST_CHANGES\n\nMissing X.\n",
        );

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = get_context_response(
            dir.path(),
            &state,
            &SessionId::from("foo".to_string()),
            &AgentLabel::from("reviewer".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["waiting_on"]["role"], "master");
        assert_eq!(v["waiting_on"]["reason"], "address_plan_request_changes");
        let agents = v["waiting_on"]["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0], "codex");
    }

    #[tokio::test]
    async fn get_context_approve_routes_to_master_ready_to_implement() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.sessions[&SessionId::from("foo".to_string())]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLGTM.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = get_context_response(
            dir.path(),
            &state,
            &SessionId::from("foo".to_string()),
            &AgentLabel::from("master".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["waiting_on"]["role"], "master");
        assert_eq!(v["waiting_on"]["reason"], "ready_to_implement");
    }
}
