//! MCP / HTTP response shaping over the filesystem-truth core.
//!
//! Response builders over owned runtime snapshots. Request handlers clone
//! snapshots under the runtime mutex, release it, then call these builders
//! to do working-tree status reads and assemble MCP / HTTP JSON.

use std::path::Path;

use serde_json::{Value, json};

use crate::lifecycle::{AgentLabel, ContentHash, PlanKey, content_hash, plan_path_counterpart};
use crate::projection::{
    all_implementation_commits, all_plan_revisions, expected_action, impl_gate_for, phase,
    plan_gate_for, plan_worktree_status, waiting_on,
};
use crate::repo_state::{PlanWorktreeStatus, RepoState, WaitingOn};
use crate::review_state::ReviewGateDecision;
use crate::runtime_snapshot::{PlanSnapshotBundle, RepoSnapshot};

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
    let counterpart_rel = plan_path_counterpart(plan_path);
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

/// `list_plans` response over a single repo.
///
/// Returns `{ plans: [...], conflicts: [...] }`. Each plan row carries the
/// fields the homepage / agent loop needs to drive a session table without
/// further queries. Conflict rows surface stems that map to multiple files
/// on disk — work is not routed through them until the operator resolves
/// the collision (see plan-path-identity §1b).
pub fn list_plans_response(snapshot: &RepoSnapshot) -> std::io::Result<Value> {
    list_plans_response_with_status_reader(snapshot, &DiskPlanStatusReader)
}

pub(crate) fn list_plans_response_with_status_reader(
    snapshot: &RepoSnapshot,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let state = snapshot.to_repo_state();
    let mut plans = Vec::with_capacity(state.plans.len());
    for plan_snapshot in &snapshot.plans {
        let plan = state
            .plans
            .get(&plan_snapshot.id)
            .expect("snapshot plan must be present in temporary state");
        let worktree_status = status_reader.compute(
            &snapshot.root,
            &plan_snapshot.plan_path,
            &plan_snapshot.body_hash,
        )?;
        let plan_phase = phase(plan, &state.attribution);
        let gate = crate::projection::latest_reviewable_commit_gate_for(
            &plan.id,
            &plan.commits,
            &state.commit_order,
            &state.plan_touches,
            &state.attribution,
        );
        let w = waiting_on(
            matches!(plan.state, crate::repo_state::PlanState::Done),
            worktree_status,
            gate.as_ref(),
        );
        plans.push(plan_summary(
            &snapshot.root,
            plan,
            plan_phase,
            worktree_status,
            &w,
        ));
    }
    let conflicts: Vec<Value> = snapshot
        .plan_conflicts
        .iter()
        .map(|(key, paths)| {
            json!({
                "slug": key.as_str(),
                "paths": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "plans": plans,
        "conflicts": conflicts,
    }))
}

fn plan_summary(
    repo_root: &Path,
    plan: &crate::repo_state::Plan,
    plan_phase: crate::repo_state::Phase,
    worktree_status: PlanWorktreeStatus,
    w: &WaitingOn,
) -> Value {
    let plan_id = crate::lifecycle::RepoBasename::from_repo_root(repo_root)
        .map(|b| crate::lifecycle::PlanId::new(b, plan.id.clone()).to_string());
    json!({
        "repo": repo_root.to_string_lossy(),
        "plan_id": plan_id,
        "slug": plan.id.as_str(),
        "state": plan.state.as_str(),
        "current_path": plan.plan_path.to_string_lossy(),
        "phase": plan_phase.as_str(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(w),
    })
}

/// `get_context` response for a specific session + author.
pub fn get_context_response(
    snapshot: &PlanSnapshotBundle,
    author_label: &AgentLabel,
) -> std::io::Result<Value> {
    get_context_response_with_status_reader(snapshot, author_label, &DiskPlanStatusReader)
}

pub(crate) fn get_context_response_with_status_reader(
    snapshot: &PlanSnapshotBundle,
    author_label: &AgentLabel,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let worktree_status = status_reader.compute(
        &snapshot.root,
        &snapshot.plan.plan_path,
        &snapshot.plan.body_hash,
    )?;
    Ok(get_context_response_from_snapshot(
        snapshot,
        worktree_status,
        author_label,
    ))
}

pub fn get_context_response_from_snapshot(
    snapshot: &PlanSnapshotBundle,
    worktree_status: PlanWorktreeStatus,
    author_label: &AgentLabel,
) -> Value {
    let state = snapshot.to_repo_state();
    let session_id = &snapshot.plan.id;
    let session = state
        .plans
        .get(session_id)
        .expect("snapshot session must be present in temporary state");
    let session_phase = phase(session, &state.attribution);
    let plan_gate = plan_gate_for(session, &state);
    let impl_gate = impl_gate_for(session, &state);
    let gate = crate::projection::latest_reviewable_commit_gate_for(
        &session.id,
        &session.commits,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
    );
    let w = waiting_on(
        matches!(session.state, crate::repo_state::PlanState::Done),
        worktree_status,
        gate.as_ref(),
    );

    let pr_hint = if matches!(session_phase, crate::repo_state::Phase::Implementing) {
        Some(pr_hint_value(session, &state))
    } else {
        None
    };

    // Phase 2.3: feedback is stored per-commit. The pre-cutover wire
    // shape split feedback into "plan_feedback" and "impl_feedback" by
    // which legacy map the disk file lived in. Map back through the
    // commit's `CommitKind` for the same split until phase 2.5 drops
    // the dichotomy from the wire.
    let (plan_feedback, impl_feedback) = phase_split_feedback(session, &state);
    let timeline = timeline_value(&state, session_id);

    let plan_revisions: Vec<String> = all_plan_revisions(session, &state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let implementation_commits: Vec<String> = all_implementation_commits(session, &state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();

    // Canonical write-feedback path + review_target + expected_action.
    // Reads from `latest_reviewable_commit_for` so the target SHA the
    // wire publishes is the same SHA the gate was computed on — no
    // chance of pointing reviewers at a non-reviewable
    // (MultiPlan / DoneMove) commit.
    let review_target_sha = crate::projection::latest_reviewable_commit_for(
        &session.id,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
    );
    let review_target_phase = if matches!(session_phase, crate::repo_state::Phase::Implementing) {
        "impl"
    } else {
        "plan"
    };
    let review_target = review_target_sha.as_ref().map(|sha| {
        json!({
            "phase": review_target_phase,
            "commit_sha": sha.as_str(),
        })
    });
    let write_feedback = review_target_sha.as_ref().map(|sha| {
        let rel = format!(
            ".trinity/feedback/{session}/commits/{sha}/{author}.md",
            session = session.id.as_str(),
            sha = sha.as_str(),
            author = author_label.as_str(),
        );
        json!({
            "phase": review_target_phase,
            "target_sha": sha.as_str(),
            "path": rel,
        })
    });
    let expected_action_str = expected_action(w.reason);

    let plan_id = crate::lifecycle::RepoBasename::from_repo_root(&snapshot.root)
        .map(|b| crate::lifecycle::PlanId::new(b, session.id.clone()).to_string());

    let commits_value = commits_array(session, &state);
    let latest_relevant_commit = crate::projection::latest_reviewable_commit_for(
        &session.id,
        &state.commit_order,
        &state.plan_touches,
        &state.attribution,
    )
    .map(|s| s.as_str().to_string());

    json!({
        "repo": snapshot.root.to_string_lossy(),
        "plan_id": plan_id,
        "slug": session.id.as_str(),
        "state": session.state.as_str(),
        "current_path": session.plan_path.to_string_lossy(),
        "phase": session_phase.as_str(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(&w),
        "expected_action": expected_action_str,
        "review_target": review_target,
        "write_feedback": write_feedback,
        "review_gate": gate_value(plan_gate.as_ref(), impl_gate.as_ref(), session_phase),
        "latest_plan_revision": latest_plan_revision(session, &state),
        "latest_implementation_revision": latest_impl_revision(session, &state),
        "plan_revisions": plan_revisions,
        "implementation_commits": implementation_commits,
        "plan_feedback": plan_feedback,
        "impl_feedback": impl_feedback,
        // Phase 2.7: commit-keyed wire shape, additive. Phase 2.8
        // drops `plan_feedback`/`impl_feedback` once the frontend
        // reads from here. `latest_relevant_commit` is the SHA the
        // gate / waiting_on / review_target are computed against.
        "commits": commits_value,
        "latest_relevant_commit": latest_relevant_commit,
        "timeline": timeline,
        "pr_hint": pr_hint,
    })
}

/// Build the per-commit `commits[]` array for `get_context`. Each
/// entry: `{sha, kind, gate, feedback[]}` in chronological
/// (`commit_order`) order. Only commits whose kind is reviewable
/// for this plan are emitted (`PlanOnly` | `CodeOnly` | `Mixed`);
/// `DoneMove` / `MultiPlan` / `Unattributed` are skipped because
/// they don't carry a gate.
fn commits_array(plan: &crate::repo_state::Plan, state: &RepoState) -> Vec<Value> {
    use crate::projection::commit_kind_for;
    let mut out = Vec::new();
    for sha in &state.commit_order {
        let kind = commit_kind_for(&plan.id, sha, &state.plan_touches, &state.attribution);
        if !kind.is_reviewable() {
            continue;
        }
        let gate_value = plan.commits.get(sha).map(|g| {
            json!({
                "state": g.state.as_str(),
                "participants": g.participants.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "approvers": g.approvers.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "requesters": g.requesters.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "ambiguous": g.ambiguous.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "missing": g.missing.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            })
        });
        let feedback_array: Vec<Value> = plan
            .commits
            .get(sha)
            .map(|g| {
                g.feedback
                    .iter()
                    .map(|(author, fb)| {
                        json!({
                            "author": author.as_str(),
                            "verdict": fb.verdict.as_str(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(json!({
            "sha": sha.as_str(),
            "kind": kind.as_str(),
            "gate": gate_value,
            "feedback": feedback_array,
        }));
    }
    out
}

/// Serialize the per-session timeline (from `RepoState::timeline_for`)
/// into the response shape. Each event becomes a `{ kind, ... }` object.
fn timeline_value(state: &RepoState, session_id: &PlanKey) -> Vec<Value> {
    state
        .timeline_for(session_id)
        .into_iter()
        .map(|e| match e {
            crate::repo_state::TimelineEvent::Commit {
                sha,
                plan_touch,
                has_code_changes,
                subject,
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
                    "subject": subject,
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
        })
        .collect()
}

fn phase_split_feedback(
    plan: &crate::repo_state::Plan,
    state: &RepoState,
) -> (Vec<Value>, Vec<Value>) {
    use crate::projection::commit_kind_for;
    use crate::repo_state::CommitKind;
    let mut plan_fb = Vec::new();
    let mut impl_fb = Vec::new();
    for (sha, gate) in &plan.commits {
        let kind = commit_kind_for(&plan.id, sha, &state.plan_touches, &state.attribution);
        let target = match kind {
            CommitKind::PlanOnly => &mut plan_fb,
            CommitKind::CodeOnly | CommitKind::Mixed => &mut impl_fb,
            _ => continue,
        };
        for (author, fb) in &gate.feedback {
            target.push(json!({
                "target_sha": sha.as_str(),
                "author": author.as_str(),
                "verdict": fb.verdict.as_str(),
            }));
        }
    }
    (plan_fb, impl_fb)
}

fn pr_hint_value(session: &crate::repo_state::Plan, state: &RepoState) -> Value {
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
    // `phase` field on the wire is back-derived by the caller from
    // the surrounding `session_phase` argument so existing consumers
    // keep working until phase 2.8 (frontend) drops the field.
    let phase_str = match session_phase {
        crate::repo_state::Phase::Planning => "plan",
        crate::repo_state::Phase::Implementing => "impl",
        crate::repo_state::Phase::Done => "plan",
    };
    match gate {
        Some(g) => json!({
            "state": g.state.as_str(),
            "phase": phase_str,
            "participants": g.participants.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "approvals": g.approvals.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "request_changes": g.request_changes.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "missing_approvals": g.missing_approvals.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    }
}

fn latest_plan_revision(session: &crate::repo_state::Plan, state: &RepoState) -> Value {
    match all_plan_revisions(session, state).last() {
        Some(sha) => json!({ "commit_sha": sha.as_str() }),
        None => Value::Null,
    }
}

fn latest_impl_revision(session: &crate::repo_state::Plan, state: &RepoState) -> Value {
    match all_implementation_commits(session, state).last() {
        Some(sha) => json!({ "commit_sha": sha.as_str() }),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::rebuild_repo;
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

    fn status_for_session(repo: &Path, session: &crate::repo_state::Plan) -> PlanWorktreeStatus {
        compute_plan_worktree_status_parts(repo, &session.plan_path, &session.body_hash).unwrap()
    }

    fn context_from_state(state: &RepoState, sid: &str, author: &str) -> Option<serde_json::Value> {
        let sid = PlanKey::from(sid.to_string());
        let snapshot = PlanSnapshotBundle::from_state_for(state, &sid)?;
        Some(get_context_response(&snapshot, &AgentLabel::from(author.to_string())).unwrap())
    }

    #[tokio::test]
    async fn plan_worktree_clean_after_commit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.plans.values().next().unwrap();
        let status = status_for_session(dir.path(), session);
        assert_eq!(status, PlanWorktreeStatus::Clean);
    }

    #[tokio::test]
    async fn plan_worktree_body_dirty_after_edit() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "add plan v1");
        // Edit without committing
        write_file(
            dir.path(),
            ".trinity/plans/foo.md",
            "# foo v2 uncommitted\n",
        );

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = state.plans.values().next().unwrap();
        let status = status_for_session(dir.path(), session);
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
        let session = state.plans.values().next().unwrap();
        let status = status_for_session(dir.path(), session);
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
        let session = state.plans.values().next().unwrap();
        let status = status_for_session(dir.path(), session);
        assert_eq!(status, PlanWorktreeStatus::MissingActivePlanFile);
    }

    #[tokio::test]
    async fn list_plans_response_includes_waiting_on() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let snapshot = RepoSnapshot::from_state(&state);
        let v = list_plans_response(&snapshot).unwrap();
        let arr = v["plans"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["slug"], "foo");
        assert_eq!(arr[0]["current_path"], ".trinity/plans/foo.md");
        assert_eq!(arr[0]["state"], "active");
        assert_eq!(arr[0]["phase"], "planning");
        assert_eq!(arr[0]["plan_worktree_status"], "clean");
        // Just-committed plan with no reviews → reviewers / plan_needs_initial_review.
        assert_eq!(arr[0]["waiting_on"]["role"], "reviewers");
        assert_eq!(arr[0]["waiting_on"]["reason"], "commit_needs_review");
        assert_eq!(v["conflicts"], json!([]));
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
        // Edit uncommitted
        write_file(dir.path(), ".trinity/plans/foo.md", "# v2 uncommitted\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
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
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
        assert_eq!(v["phase"], "implementing");
        // No impl reviews yet → reviewers / impl_needs_initial_review
        assert_eq!(v["waiting_on"]["role"], "reviewers");
        assert_eq!(v["waiting_on"]["reason"], "commit_needs_review");
        assert!(v["latest_implementation_revision"].is_object());
    }

    #[tokio::test]
    async fn get_context_request_changes_routes_to_master() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "add plan");

        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.plans[&PlanKey::from("foo".to_string())]
            .plan_intro
            .clone();
        // Use full SHA in the feedback path. Trinity in production will
        // tell agents the canonical (full-SHA) path via get_context;
        // prefix resolution at ingest is a future enhancement.
        let feedback_rel = format!(".trinity/feedback/foo/commits/{}/codex.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "REQUEST_CHANGES\n\nMissing X.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "reviewer").unwrap();
        assert_eq!(v["waiting_on"]["role"], "master");
        assert_eq!(v["waiting_on"]["reason"], "address_commit_changes");
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
        let intro = state0.plans[&PlanKey::from("foo".to_string())]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/commits/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLGTM.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let v = context_from_state(&state, "foo", "master").unwrap();
        assert_eq!(v["waiting_on"]["role"], "master");
        assert_eq!(v["waiting_on"]["reason"], "ready_to_move_forward");
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
            .snapshot_session(dir.path(), &PlanKey::from("foo".to_string()))
            .await
            .unwrap()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let reader = BlockingStatusReader {
            entered: entered_tx,
            release: release_rx,
        };
        let author = AgentLabel::from("reviewer".to_string());

        let handle = tokio::task::spawn_blocking(move || {
            get_context_response_with_status_reader(&snapshot, &author, &reader).unwrap()
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("status reader did not start");

        tokio::time::timeout(Duration::from_secs(1), runtime.snapshot_repo(dir.path()))
            .await
            .expect("runtime mutex was held while status reader blocked")
            .unwrap();

        release_tx.send(()).unwrap();
        let v = handle.await.unwrap();
        assert_eq!(v["plan_worktree_status"], "clean");
    }
}
