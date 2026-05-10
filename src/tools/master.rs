use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::AppState;
use crate::daemon::git;
use crate::daemon::internal_api::ToolCallRequest;
use crate::domain::{EventKind, TargetKind};
use crate::storage::{
    agents, batches, events as ev_store, implementation_revisions as impl_revs, plan_revisions,
    plans,
};

use super::ToolError;

const MASTER_STALE_SECONDS: i64 = 300; // 5 minutes

#[derive(Debug, Deserialize)]
struct RegisterPlanFileArgs {
    path: String,
    label: String,
}

#[derive(Debug, Deserialize)]
struct PollDirectiveArgs {
    plan_id: String,
}

#[derive(Debug, Deserialize)]
struct AckDirectiveArgs {
    plan_id: String,
    batch_id: i64,
}

#[derive(Debug, Deserialize)]
struct RegisterImplementationCommitArgs {
    plan_id: String,
    commit_sha: String,
    #[serde(default)]
    force: bool,
}

pub async fn register_plan_file(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: RegisterPlanFileArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;

    if args.label.trim().is_empty() {
        return Err(ToolError::Invalid("label must be non-empty".into()));
    }

    let raw_path = expand_home(&args.path);
    let abs_path = if raw_path.is_absolute() {
        raw_path
    } else {
        req.cwd.join(raw_path)
    };
    let plan_path = dunce::canonicalize(&abs_path).map_err(|e| {
        ToolError::Invalid(format!(
            "plan file does not exist or is unreadable ({}): {e}",
            abs_path.display()
        ))
    })?;

    if !plan_path.is_file() {
        return Err(ToolError::Invalid(format!(
            "plan path is not a regular file: {}",
            plan_path.display()
        )));
    }

    let repo_root = git::rev_parse_show_toplevel(&req.cwd)
        .await
        .map_err(|e| match e {
            git::GitError::NotInRepo(p) => ToolError::Invalid(format!(
                "caller cwd is not in a git repository: {}",
                p.display()
            )),
            other => ToolError::Internal(anyhow::anyhow!(other)),
        })?;
    let repo_root_canonical = dunce::canonicalize(&repo_root)
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("canonicalize repo_root: {e}")))?;

    let plan_id = plans::compute_plan_id(&repo_root_canonical, &plan_path);

    // Reject collision: same canonical plan_path, *different* repo_root, plan still
    // active. The watcher can only map one plan_id per path.
    let plan_path_str = plan_path.to_string_lossy().into_owned();
    if let Some(other) = sqlx::query_as::<_, (String, String)>(
        "SELECT id, repo_root FROM plans WHERE plan_path = ? AND archived_at IS NULL AND id != ?",
    )
    .bind(&plan_path_str)
    .bind(&plan_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    {
        return Err(ToolError::Forbidden(format!(
            "plan file `{}` is already actively registered under repo_root `{}` (plan {}); archive that plan or use a different file",
            plan_path_str, other.1, other.0
        )));
    }

    let body = tokio::fs::read_to_string(&plan_path)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!("read plan file: {e}")))?;
    let content_hash = plan_revisions::compute_content_hash(&body);

    let now = chrono::Utc::now().timestamp();

    let display_title = plan_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plan")
        .to_string();

    let revision_id;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    let existing = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    if let Some(plan) = existing {
        // Reject re-registering a terminal-state lifecycle. The previous
        // session ended; v0 doesn't silently revive `done` or `archived`
        // plans. The human must reset/fork via the web UI (post-v0) or
        // pick a different plan file.
        let plan_state = crate::domain::WorkState::parse(&plan.state).ok_or_else(|| {
            ToolError::Internal(anyhow::anyhow!("plan has invalid state: {}", plan.state))
        })?;
        if plan_state.is_terminal() {
            return Err(ToolError::Forbidden(format!(
                "this plan file has been registered before and that session is `{}`. \
                 v0 does not silently revive terminal sessions — use a different plan file path \
                 (rename or move the existing file) to start a new lifecycle. \
                 Proper reset/fork is a v1 concern.",
                plan_state
            )));
        }

        if let Some(master) = agents::fetch_master(&state.pool, &plan_id)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        {
            let stale = now - master.last_seen >= MASTER_STALE_SECONDS;
            if master.label != args.label && !stale {
                return Err(ToolError::Forbidden(format!(
                    "plan already has live master `{}` (last seen {}s ago); evict via UI or wait {}s",
                    master.label,
                    now - master.last_seen,
                    MASTER_STALE_SECONDS - (now - master.last_seen)
                )));
            }
        }

        let latest = plan_revisions::latest(&state.pool, &plan_id)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let needs_snapshot =
            latest.as_ref().map(|r| r.content_hash.as_str()) != Some(content_hash.as_str());
        // v0 lifecycle gate: plan-file edits become revisions only while the
        // plan is `planning` or `plan_approved`. Matches the watcher behavior.
        let accepts_revisions = plan_state.accepts_plan_revisions();
        let actor = format!("master:{}", args.label);
        if needs_snapshot && !accepts_revisions {
            let payload = json!({
                "path": plan_path.to_string_lossy(),
                "new_content_hash": content_hash,
                "note": "register_plan_file detected drift after implementation; v0 ignores.",
            });
            ev_store::append(
                &mut *tx,
                &ev_store::NewEvent::note(
                    &plan_id,
                    EventKind::PlanFileChangedAfterImplementation,
                    &actor,
                    &payload,
                    now,
                ),
            )
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            // No new revision — surface the latest existing one to the caller.
            revision_id = latest.expect("latest exists").id;
        } else if needs_snapshot {
            let new_rev_id = plan_revisions::append(
                &mut *tx,
                &plan_id,
                &content_hash,
                &body,
                now,
                "resume_snapshot",
            )
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            let new_rev_id_str = new_rev_id.to_string();
            let payload = json!({"detected_by": "resume_snapshot"});
            ev_store::append(
                &mut *tx,
                &ev_store::NewEvent::against_target(
                    &plan_id,
                    EventKind::PlanRevisionCreated,
                    &actor,
                    &payload,
                    now,
                    ev_store::EventTarget {
                        kind: TargetKind::PlanRevision,
                        id: &new_rev_id_str,
                    },
                ),
            )
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
            revision_id = new_rev_id;
        } else {
            revision_id = latest.expect("latest exists").id;
        }

        let agent_id = match agents::fetch_by_label(&state.pool, &plan_id, &args.label)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        {
            Some(existing_agent) => {
                if existing_agent.role != "master" {
                    return Err(ToolError::Forbidden(format!(
                        "label `{}` is already a {} on this plan",
                        args.label, existing_agent.role
                    )));
                }
                agents::touch_last_seen(&mut *tx, existing_agent.id, now)
                    .await
                    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                existing_agent.id
            }
            None => {
                let new_id =
                    agents::insert(&mut *tx, &plan_id, agents::Role::Master, &args.label, now)
                        .await
                        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                let payload = json!({"role": "master", "label": args.label});
                ev_store::append(
                    &mut *tx,
                    &ev_store::NewEvent::note(
                        &plan_id,
                        EventKind::AgentJoined,
                        &actor,
                        &payload,
                        now,
                    ),
                )
                .await
                .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
                new_id
            }
        };

        plans::set_master_agent(&mut *tx, &plan.id, agent_id, now)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    } else {
        let plan = plans::Plan {
            id: plan_id.clone(),
            repo_root: repo_root_canonical.to_string_lossy().into_owned(),
            plan_path: Some(plan_path.to_string_lossy().into_owned()),
            display_title: Some(display_title),
            state: "planning".to_string(),
            master_agent_id: None,
            current_implementation_id: None,
            created_at: now,
            updated_at: now,
            archived_at: None,
        };
        let actor = format!("master:{}", args.label);
        plans::insert(&mut *tx, &plan)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let new_rev_id = plan_revisions::append(
            &mut *tx,
            &plan_id,
            &content_hash,
            &body,
            now,
            "register_plan_file",
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let new_rev_id_str = new_rev_id.to_string();
        let rev_payload = json!({"detected_by": "register_plan_file"});
        ev_store::append(
            &mut *tx,
            &ev_store::NewEvent::against_target(
                &plan_id,
                EventKind::PlanRevisionCreated,
                &actor,
                &rev_payload,
                now,
                ev_store::EventTarget {
                    kind: TargetKind::PlanRevision,
                    id: &new_rev_id_str,
                },
            ),
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let agent_id = agents::insert(&mut *tx, &plan_id, agents::Role::Master, &args.label, now)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let join_payload = json!({"role": "master", "label": args.label});
        ev_store::append(
            &mut *tx,
            &ev_store::NewEvent::note(&plan_id, EventKind::AgentJoined, &actor, &join_payload, now),
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        plans::set_master_agent(&mut *tx, &plan_id, agent_id, now)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        revision_id = new_rev_id;
    }

    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    if let Err(err) = state.watcher.watch(&plan_path, &plan_id) {
        tracing::warn!(plan_id = %plan_id, error = ?err, "watcher.watch failed (continuing)");
    }

    Ok(json!({
        "plan_id": plan_id,
        "revision_id": revision_id,
    }))
}

pub async fn poll_directive(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: PollDirectiveArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let label = require_label(req)?;
    let _master = require_master(state, &args.plan_id, &label).await?;

    let Some(batch) = batches::oldest_unacked(&state.pool, &args.plan_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    else {
        return Ok(json!({"directive": "none"}));
    };

    let items = sqlx::query_as::<_, ev_store::Event>(
        "SELECT events.* FROM events \
         JOIN directive_batch_items ON directive_batch_items.event_id = events.id \
         WHERE directive_batch_items.batch_id = ? ORDER BY events.id ASC",
    )
    .bind(batch.id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    let items_json: Vec<Value> = items
        .iter()
        .map(|ev| {
            let payload: Value = serde_json::from_str(&ev.payload).unwrap_or(Value::Null);
            json!({
                "event_id": ev.id,
                "target_kind": ev.target_kind,
                "target_id": ev.target_id,
                "text": payload.get("text").cloned().unwrap_or(Value::Null),
                "actor": ev.actor,
                "ts": ev.ts,
            })
        })
        .collect();

    Ok(json!({
        "directive": "feedback",
        "batch_id": batch.id,
        "target_kind": batch.target_kind,
        "items": items_json,
    }))
}

pub async fn ack_directive(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: AckDirectiveArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let label = require_label(req)?;

    let batch = batches::fetch(&state.pool, args.batch_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("batch {} not found", args.batch_id)))?;

    // Belt-and-suspenders: the shim already enforces that `plan_id` matches
    // the bound plan, but the daemon must independently verify so a label
    // shared across plans can't ack the wrong batch by id.
    if args.plan_id != batch.plan_id {
        return Err(ToolError::Forbidden(format!(
            "batch {} belongs to plan `{}`, not `{}`",
            batch.id, batch.plan_id, args.plan_id
        )));
    }

    let _master = require_master(state, &batch.plan_id, &label).await?;

    if batch.acked_at.is_some() {
        return Ok(json!({"ok": true, "already_acked": true}));
    }

    let now = chrono::Utc::now().timestamp();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let actor = format!("master:{}", label);
    batches::ack(&mut *tx, batch.id, &actor, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let payload = json!({"batch_id": batch.id});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &batch.plan_id,
            EventKind::DirectiveAcked,
            &actor,
            &payload,
            now,
        ),
    )
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    plans::touch_updated_at(&mut *tx, &batch.plan_id, now)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    Ok(json!({"ok": true}))
}

pub async fn register_implementation_commit(
    state: &AppState,
    req: &ToolCallRequest,
) -> Result<Value, ToolError> {
    let args: RegisterImplementationCommitArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let label = require_label(req)?;
    let plan = plans::fetch(&state.pool, &args.plan_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("plan {} not found", args.plan_id)))?;
    let _master = require_master(state, &plan.id, &label).await?;

    register_commit_inner(
        state,
        &plan,
        &args.commit_sha,
        args.force,
        &format!("master:{}", label),
        "register_implementation_commit",
    )
    .await
}

/// Shared logic between the master MCP tool and the curator's UI fallback.
pub async fn register_commit_inner(
    state: &AppState,
    plan: &plans::Plan,
    rev: &str,
    force: bool,
    registered_by: &str,
    detected_by: &str,
) -> Result<Value, ToolError> {
    use crate::domain::WorkState;
    let plan_state = WorkState::parse(&plan.state).ok_or_else(|| {
        ToolError::Internal(anyhow::anyhow!("plan has invalid state: {}", plan.state))
    })?;
    if plan_state.is_terminal() {
        return Err(ToolError::Forbidden(format!(
            "plan is in terminal state `{plan_state}`; cannot register a new implementation commit"
        )));
    }
    let repo = std::path::Path::new(&plan.repo_root).to_path_buf();
    let full_sha = git::resolve_to_full_sha(&repo, rev)
        .await
        .map_err(|e| match e {
            git::GitError::CommitNotFound { .. } => {
                ToolError::Invalid(format!("commit {rev} not found in {}", plan.repo_root))
            }
            other => ToolError::Internal(anyhow::anyhow!(other)),
        })?;
    let head_sha = git::rev_parse_head(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let is_head = full_sha == head_sha;
    if !is_head && !force {
        return Err(ToolError::Invalid(format!(
            "commit {} is not HEAD ({}); pass force=true to register an older commit",
            git::short_sha(&full_sha),
            git::short_sha(&head_sha)
        )));
    }

    let parent = git::parent_sha(&repo, &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let branch = git::current_branch(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let message = git::commit_message(&repo, &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let stat = git::diff_stat(&repo, parent.as_deref(), &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let porcelain = git::worktree_porcelain(&repo)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    let dirty = !porcelain.trim().is_empty();
    let worktree_status = if dirty {
        Some(porcelain.as_str())
    } else {
        Some("clean")
    };

    let now = chrono::Utc::now().timestamp();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    // Idempotent: if same SHA already registered, return it.
    if let Some(existing) = impl_revs::fetch_by_sha(&state.pool, &plan.id, &full_sha)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
    {
        return Ok(json!({
            "implementation_revision_id": existing.id,
            "commit_sha": existing.commit_sha,
            "already_registered": true,
        }));
    }

    let new_id = impl_revs::insert(
        &mut *tx,
        &impl_revs::NewImplementationRevision {
            plan_id: &plan.id,
            commit_sha: &full_sha,
            parent_sha: parent.as_deref(),
            branch: branch.as_deref(),
            commit_message: &message,
            diff_stat: &stat,
            worktree_status,
            is_head,
            registered_by,
            detected_by,
            created_at: now,
        },
    )
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    impl_revs::set_current(&mut *tx, &plan.id, new_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    let prior_state = plan.state.clone();
    if matches!(plan_state, WorkState::Planning | WorkState::PlanApproved) {
        sqlx::query(
            "UPDATE plans SET state = 'implementation_review', updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(&plan.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
        let st_payload = json!({"from": prior_state, "to": "implementation_review"});
        ev_store::append(
            &mut *tx,
            &ev_store::NewEvent::note(
                &plan.id,
                EventKind::StateTransition,
                registered_by,
                &st_payload,
                now,
            ),
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    } else {
        plans::touch_updated_at(&mut *tx, &plan.id, now)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }

    let impl_payload = json!({
        "branch": branch,
        "is_head": is_head,
        "force": force,
    });
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::against_target(
            &plan.id,
            EventKind::ImplRevisionCreated,
            registered_by,
            &impl_payload,
            now,
            ev_store::EventTarget {
                kind: TargetKind::ImplementationCommit,
                id: &full_sha,
            },
        ),
    )
    .await
    .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    if dirty {
        let dirty_payload = json!({"porcelain": porcelain});
        ev_store::append(
            &mut *tx,
            &ev_store::NewEvent::against_target(
                &plan.id,
                EventKind::DirtyWorktreeWarning,
                registered_by,
                &dirty_payload,
                now,
                ev_store::EventTarget {
                    kind: TargetKind::ImplementationCommit,
                    id: &full_sha,
                },
            ),
        )
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;
    }

    tx.commit()
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    Ok(json!({
        "implementation_revision_id": new_id,
        "commit_sha": full_sha,
        "is_head": is_head,
        "dirty_worktree": dirty,
    }))
}

fn require_label(req: &ToolCallRequest) -> Result<String, ToolError> {
    req.label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ToolError::Forbidden(
                "this tool requires you to first call register_plan_file to bind a master label"
                    .into(),
            )
        })
}

async fn require_master(
    state: &AppState,
    plan_id: &str,
    label: &str,
) -> Result<agents::Agent, ToolError> {
    let agent = agents::fetch_by_label(&state.pool, plan_id, label)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            ToolError::Forbidden(format!(
                "no agent `{label}` on plan {plan_id}; call register_plan_file first"
            ))
        })?;
    if agent.role != "master" {
        return Err(ToolError::Forbidden(format!(
            "label `{label}` joined as {} on plan {plan_id}; only the master can poll directives",
            agent.role
        )));
    }
    let now = chrono::Utc::now().timestamp();
    let _ = agents::touch_last_seen(&state.pool, agent.id, now).await;
    Ok(agent)
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}
