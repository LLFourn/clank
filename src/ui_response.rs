//! UI-only response builders for the Leptos SPA's `/api/*` surface.
//!
//! These functions are deliberately separate from `mcp_response::*` — the
//! web UI's shape is allowed to be richer than the MCP tool surface (it
//! never costs an agent context-window tokens). The builders take a
//! cloned `RepoState` directly (cloned under the runtime mutex by
//! `Runtime::snapshot_repo` / `Runtime::snapshot_session`), so all disk
//! I/O happens after the runtime mutex has been released.
//!
//! Disk reads go through the `PlanStatusReader` trait so tests can drop
//! in a fake.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use crate::mcp_response::PlanStatusReader;
use crate::projection::{
    all_implementation_commits_for, all_plan_revisions_for, expected_action, impl_gate_for_parts,
    phase_for, plan_gate_for_parts, waiting_on,
};
use crate::repo_state::{
    AttributionResult, Feedback, Phase, Plan, PlanTouchKind, RepoState, Verdict, WaitingOn,
};
use crate::review_state::CommitGate;

/// `GET /api/plans` — `{ plans, conflicts }` for the home page.
/// Public wrapper binds the production `DiskPlanStatusReader`.
pub fn plans_index(snapshot: &RepoState) -> std::io::Result<Value> {
    plans_index_with_reader(snapshot, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plans_index_with_reader(
    snapshot: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let (plans_typed, conflicts) = plans_index_parts(snapshot, status_reader)?;
    let plans: Vec<Value> = plans_typed.into_iter().map(|(_, v)| v).collect();
    Ok(json!({ "plans": plans, "conflicts": conflicts }))
}

/// Aggregated `{ plans, conflicts }` across multiple snapshots, sorted
/// by `last_activity_ts` desc. The typed `(i64, Value)` pairs survive
/// the merge so the sort key never gets serialized-and-re-extracted via
/// the JSON shape — a regression on the timestamp field type would be
/// a compile error here, not a silent sort degrade.
pub fn plans_index_across(snapshots: &[RepoState]) -> std::io::Result<Value> {
    plans_index_across_with_reader(snapshots, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plans_index_across_with_reader(
    snapshots: &[RepoState],
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let mut all_plans: Vec<IndexedPlanRow> = Vec::new();
    let mut all_conflicts: Vec<Value> = Vec::new();
    for snapshot in snapshots {
        let (plans_typed, conflicts) = plans_index_parts(snapshot, status_reader)?;
        all_plans.extend(plans_typed);
        all_conflicts.extend(conflicts);
    }
    all_plans.sort_by_key(|p| std::cmp::Reverse(p.0));
    let plans: Vec<Value> = all_plans.into_iter().map(|(_, v)| v).collect();
    Ok(json!({ "plans": plans, "conflicts": all_conflicts }))
}

/// One row from `plans_index_parts`: the `i64` is the
/// `last_activity_ts` sort key (kept alongside the JSON row so the
/// cross-repo merge sorts on the typed value rather than re-extracting
/// it from the serialized shape).
type IndexedPlanRow = (i64, Value);

/// Build the per-snapshot pieces with the i64 sort key still attached
/// to each plan row. Used by both the single-repo `plans_index` and the
/// cross-repo `plans_index_across` so they share one source of truth
/// for the sort key + JSON shape.
fn plans_index_parts(
    snapshot: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<(Vec<IndexedPlanRow>, Vec<Value>)> {
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&snapshot.root);
    let mut plans: Vec<IndexedPlanRow> = Vec::with_capacity(snapshot.plans.len());
    for plan in snapshot.plans.values() {
        let plan_phase = phase_for(&plan.plan_path, &plan.id, &snapshot.attribution);
        let gate = crate::projection::latest_reviewable_commit_gate_for(
            &plan.id,
            &plan.commits,
            &snapshot.commit_order,
            &snapshot.plan_touches,
            &snapshot.attribution,
        );
        let worktree_status =
            status_reader.compute(&snapshot.root, &plan.plan_path, &plan.body_hash)?;
        let w = waiting_on(
            matches!(plan.state, crate::repo_state::PlanState::Done),
            worktree_status,
            gate,
        );
        let plan_id = basename
            .as_ref()
            .map(|b| crate::lifecycle::PlanId::new(b.clone(), plan.id.clone()).to_string());
        let last_activity_ts = crate::projection::last_activity_ts_for(
            &plan.id,
            &plan.plan_intro,
            &plan.commits,
            &snapshot.commit_order,
            &snapshot.plan_touches,
            &snapshot.attribution,
            &snapshot.commit_meta,
        );
        let lifecycle = crate::repo_state::PlanLifecycle::from_plan(plan);
        plans.push((
            last_activity_ts,
            json!({
                "repo": snapshot.root.to_string_lossy(),
                "plan_id": plan_id,
                "slug": plan.id.as_str(),
                "state": lifecycle.as_str(),
                "lifecycle": lifecycle.as_str(),
                "current_path": plan.plan_path.to_string_lossy(),
                "phase": plan_phase.as_str(),
                "worktree_status": worktree_status.as_str(),
                "waiting_on": waiting_on_value(&w),
                "last_activity_ts": last_activity_ts,
            }),
        ));
    }
    plans.sort_by_key(|p| std::cmp::Reverse(p.0));
    let conflicts: Vec<Value> = snapshot
        .plan_conflicts
        .iter()
        .map(|(key, paths)| {
            let plan_id = basename
                .as_ref()
                .map(|b| crate::lifecycle::PlanId::new(b.clone(), key.clone()).to_string());
            json!({
                "plan_id": plan_id,
                "slug": key.as_str(),
                "paths": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok((plans, conflicts))
}

/// `GET /api/plan/{repo}/{stem_md}` — rich plan detail.
/// Public wrapper binds the production `DiskPlanStatusReader`.
pub fn plan_page(bundle: &RepoState) -> std::io::Result<Value> {
    plan_page_with_reader(bundle, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plan_page_with_reader(
    bundle: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let plan = bundle
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&bundle.root);
    let worktree_status = status_reader.compute(&bundle.root, &plan.plan_path, &plan.body_hash)?;
    let plan_phase = phase_for(&plan.plan_path, &plan.id, &bundle.attribution);
    let plan_gate = plan_gate_for_parts(
        &plan.id,
        &plan.commits,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    );
    let impl_gate = impl_gate_for_parts(
        &plan.id,
        &plan.commits,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    );
    let gate = crate::projection::latest_reviewable_commit_gate_for(
        &plan.id,
        &plan.commits,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    );
    let w = waiting_on(
        matches!(plan.state, crate::repo_state::PlanState::Done),
        worktree_status,
        gate,
    );

    let plan_revisions: Vec<String> =
        all_plan_revisions_for(&plan.id, &bundle.commit_order, &bundle.plan_touches)
            .into_iter()
            .map(|s| s.as_str().to_string())
            .collect();
    let implementation_commits: Vec<String> =
        all_implementation_commits_for(&plan.id, &bundle.commit_order, &bundle.attribution)
            .into_iter()
            .map(|s| s.as_str().to_string())
            .collect();

    // Single review target: the latest reviewable commit. Same SHA
    // the gate is computed on, so the wire shape can't drift from
    // gate state. Phase-tagged "plan"/"impl" for back-compat.
    let review_target_sha = crate::projection::latest_reviewable_commit_for(
        &plan.id,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    );
    let review_target_phase = if matches!(plan_phase, Phase::Implementing) {
        "impl"
    } else {
        "plan"
    };
    let review_target = review_target_sha
        .as_ref()
        .map(|sha| json!({ "phase": review_target_phase, "commit_sha": sha.as_str() }));
    let latest_plan_revision = plan_revisions
        .last()
        .map(|sha| json!({ "commit_sha": sha }))
        .unwrap_or(Value::Null);
    let latest_implementation_revision = implementation_commits
        .last()
        .map(|sha| json!({ "commit_sha": sha }))
        .unwrap_or(Value::Null);

    let timeline = timeline_value(
        plan,
        &bundle.attribution,
        &bundle.plan_touches,
        &bundle.commit_order,
        &bundle.commit_meta,
    );
    let pr_hint = if matches!(plan_phase, Phase::Implementing) {
        Some(pr_hint_value(plan, &implementation_commits))
    } else {
        None
    };

    let plan_id = basename
        .as_ref()
        .map(|b| crate::lifecycle::PlanId::new(b.clone(), plan.id.clone()).to_string());

    let plan_body_html = render_markdown(&plan.body);
    // Threshold is a hint to the SPA on whether to render the
    // see-more toggle; the full body is always sent. ~4000 chars
    // covers "more than one screen of text" for typical reading
    // widths.
    let plan_body_truncated = plan.body.chars().count() > 4000;

    // Commit-keyed wire shape — SPA reads feedback off here.
    let commits_value = commits_array_rich(
        plan,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    );
    let latest_relevant_commit = crate::projection::latest_reviewable_commit_for(
        &plan.id,
        &bundle.commit_order,
        &bundle.plan_touches,
        &bundle.attribution,
    )
    .map(|s| s.as_str().to_string());

    let lifecycle = crate::repo_state::PlanLifecycle::from_plan(plan);
    let archived_cycles: Vec<Value> = plan
        .archived_cycles
        .iter()
        .map(|c| {
            json!({
                "closer": c.closer.as_str(),
                "approver_count": c.approver_count,
            })
        })
        .collect();
    Ok(json!({
        "repo": bundle.root.to_string_lossy(),
        "plan_id": plan_id,
        "slug": plan.id.as_str(),
        "state": lifecycle.as_str(),
        "lifecycle": lifecycle.as_str(),
        "current_path": plan.plan_path.to_string_lossy(),
        "phase": plan_phase.as_str(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(&w),
        "expected_action": expected_action(w.reason),
        "review_target": review_target,
        "review_gate": gate_value(plan_gate, impl_gate, plan_phase),
        "latest_plan_revision": latest_plan_revision,
        "latest_implementation_revision": latest_implementation_revision,
        "plan_revisions": plan_revisions,
        "implementation_commits": implementation_commits,
        "commits": commits_value,
        "latest_relevant_commit": latest_relevant_commit,
        "plan_body_html": plan_body_html,
        "plan_body_truncated": plan_body_truncated,
        "timeline": timeline,
        "pr_hint": pr_hint,
        "archived_cycles": archived_cycles,
    }))
}

/// UI-flavored per-commit `commits[]` array. Like the MCP version
/// but carries body + html + path on each feedback entry so the SPA
/// can render cards without further round-trips.
fn commits_array_rich(
    plan: &Plan,
    commit_order: &[CommitSha],
    plan_touches: &BTreeMap<CommitSha, Vec<(crate::lifecycle::PlanKey, PlanTouchKind)>>,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
) -> Vec<Value> {
    use crate::projection::commit_kind_for;
    let mut out = Vec::new();
    // Iterate `commit_order` (chronological) rather than `plan.commits`
    // (BTreeMap, SHA-lex order). The SPA renders per-commit blocks in
    // history order; sorting by SHA would surface them in random-looking
    // order to the human reader.
    for sha in commit_order {
        let kind = commit_kind_for(&plan.id, sha, plan_touches, attribution);
        if !kind.is_reviewable() {
            continue;
        }
        let Some(gate) = plan.commits.get(sha) else {
            continue;
        };
        let feedback_array: Vec<Value> = gate
            .feedback
            .iter()
            .map(|(author, fb)| {
                json!({
                    "author": author.as_str(),
                    "verdict": fb.verdict.as_str(),
                    "body_raw": fb.body,
                    "body_html": render_feedback_body(&fb.body, fb.verdict),
                    "path": fb.path.to_string_lossy(),
                    "created_at": fb.created_at,
                })
            })
            .collect();
        out.push(json!({
            "sha": sha.as_str(),
            "kind": kind.as_str(),
            "gate": {
                "state": gate.state.as_str(),
                "participants": gate.participants.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "approvers": gate.approvers.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "requesters": gate.requesters.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "ambiguous": gate.ambiguous.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                "missing": gate.missing.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            },
            "feedback": feedback_array,
        }));
    }
    out
}

/// Strip the verdict marker line and render the rest of the body as
/// sanitized HTML.
fn render_feedback_body(body: &str, verdict: Verdict) -> String {
    let stripped = match verdict {
        Verdict::Approve | Verdict::RequestChanges => strip_marker_line(body),
        Verdict::Unmarked => body,
    };
    render_markdown(stripped)
}

fn strip_marker_line(body: &str) -> &str {
    let mut chars = body.char_indices();
    while let Some((_, c)) = chars.clone().next() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        break;
    }
    let after_leading = chars.as_str();
    if let Some(rest) = after_leading.strip_prefix("APPROVE") {
        skip_marker_tail(rest)
    } else if let Some(rest) = after_leading.strip_prefix("REQUEST_CHANGES") {
        skip_marker_tail(rest)
    } else {
        body
    }
}

/// Consume any trailing horizontal whitespace (spaces / tabs) on the
/// marker line, then up to one CR/LF run. Matches the leniency of
/// `disk_format::parse_verdict` which trims each candidate line before
/// matching the marker — without this, an `"APPROVE   \n\nrest"` body
/// leaves 3 leading spaces in the rendered markdown (4+ would become an
/// indented code block).
fn skip_marker_tail(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] == b'\r' || bytes[i] == b'\n') {
        i += 1;
    }
    &s[i..]
}

pub fn render_markdown(input: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(input, opts);
    let mut raw = String::new();
    html::push_html(&mut raw, parser);
    ammonia::Builder::default()
        .add_generic_attributes(["class"])
        .clean(&raw)
        .to_string()
}

fn pr_hint_value(session: &Plan, impl_commits: &[String]) -> Value {
    let plan_intro = session.plan_intro.as_str();
    let plan_intro_parent = session.plan_intro_parent.as_ref().map(|s| s.as_str());
    let base_for_squash = plan_intro_parent.unwrap_or(plan_intro);
    let plan_path = session.plan_path.to_string_lossy().to_string();
    let suggested = format!("Implement {}", session.id.as_str());
    let options = vec![
        json!({
            "name": "keep_plan_in_pr",
            "base": base_for_squash,
            "command": format!(
                "git reset --soft {base} && git commit -m '{msg}'",
                base = base_for_squash,
                msg = suggested
            ),
        }),
        json!({
            "name": "exclude_plan_from_pr",
            "base": base_for_squash,
            "command": format!(
                "git reset --soft {base} && git rm {plan} && git commit -m '{msg}'",
                base = base_for_squash,
                plan = plan_path,
                msg = suggested
            ),
        }),
    ];
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
    plan_gate: Option<&CommitGate>,
    impl_gate: Option<&CommitGate>,
    session_phase: Phase,
) -> Value {
    let gate = match session_phase {
        Phase::Planning => plan_gate,
        Phase::Implementing => impl_gate,
        Phase::Done => None,
    };
    let phase_str = match session_phase {
        Phase::Planning => "plan",
        Phase::Implementing => "impl",
        Phase::Done => "plan",
    };
    match gate {
        Some(g) => json!({
            "state": crate::mcp_response::legacy_gate_state_wire(g.state),
            "phase": phase_str,
            "participants": g.participants.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "approvals": g.approvers.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "request_changes": g.requesters.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            "missing_approvals": g.missing.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    }
}

fn timeline_value(
    session: &Plan,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
    plan_touches: &BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    commit_order: &[CommitSha],
    commit_meta: &BTreeMap<CommitSha, crate::disk_snapshot::CommitMetaEntry>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for sha in commit_order {
        let plan_touch = plan_touches
            .get(sha)
            .and_then(|touches| touches.iter().find(|(sid, _)| sid == &session.id))
            .map(|(_, kind)| *kind);
        let has_code_changes = matches!(
            attribution.get(sha),
            Some(AttributionResult::Attributed {
                session: sid,
                has_code_changes: true,
                ..
            }) if sid == &session.id
        );
        if plan_touch.is_none() && !has_code_changes {
            continue;
        }
        let kind = match (plan_touch.is_some(), has_code_changes) {
            (true, true) => "commit_mixed",
            (true, false) => "commit_plan",
            (false, true) => "commit_impl",
            (false, false) => unreachable!("filtered above"),
        };
        let subject = commit_meta
            .get(sha)
            .map(|m| m.subject.clone())
            .unwrap_or_default();
        out.push(json!({
            "kind": kind,
            "sha": sha.as_str(),
            "plan_touch": plan_touch.as_ref().map(|k| k.as_str()),
            "has_code_changes": has_code_changes,
            "subject": subject,
        }));
        // Pre-cutover review rows carried a "plan"/"impl" phase tag
        // derived from which legacy map the file lived in. Post-cutover
        // we back-derive the same tag from the commit's plan_touch /
        // has_code_changes pair until phase 2.5 drops the field.
        let phase_tag = if plan_touch.is_some() { "plan" } else { "impl" };
        if let Some(gate) = session.commits.get(sha) {
            for (author, fb) in &gate.feedback {
                out.push(json!({
                    "kind": "review",
                    "phase": phase_tag,
                    "target": sha.as_str(),
                    "author": author.as_str(),
                    "verdict": fb.verdict.as_str(),
                    "created_at": fb.created_at,
                }));
            }
        }
    }
    out
}

/// Look up feedback entries (rich form) targeting `sha`. Reads from
/// `Plan.commits[sha].feedback` (the canonical per-commit store) and
/// derives the legacy "plan"/"impl" phase tag from the commit's
/// `plan_touch` so existing wire consumers keep working until phase
/// 2.5. Used by the `/api/sessions/:id/plan/:sha` and `/api/sessions/
/// :id/commit/:sha` route handlers.
pub fn feedback_for_target(session: &Plan, sha: &CommitSha) -> Vec<Value> {
    let Some(gate) = session.commits.get(sha) else {
        return Vec::new();
    };
    gate.feedback
        .iter()
        .map(|(author, fb)| feedback_entry(author, fb))
        .collect()
}

fn feedback_entry(author: &AgentLabel, fb: &Feedback) -> Value {
    json!({
        "author": author.as_str(),
        "verdict": fb.verdict.as_str(),
        "body_raw": fb.body,
        "body_html": render_feedback_body(&fb.body, fb.verdict),
        "path": fb.path.to_string_lossy(),
        "created_at": fb.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::content_hash;
    use crate::repo_state::PlanWorktreeStatus;
    use std::path::Path;

    fn empty_snapshot() -> RepoState {
        RepoState::empty(std::path::PathBuf::from("/r"))
    }

    struct StaticStatusReader(PlanWorktreeStatus);

    impl PlanStatusReader for StaticStatusReader {
        fn compute(
            &self,
            _repo_root: &Path,
            _plan_path: &Path,
            _body_hash: &crate::lifecycle::ContentHash,
        ) -> std::io::Result<PlanWorktreeStatus> {
            Ok(self.0)
        }
    }

    #[test]
    fn plans_index_empty_snapshot_returns_empty_arrays() {
        let snap = empty_snapshot();
        let v =
            plans_index_with_reader(&snap, &StaticStatusReader(PlanWorktreeStatus::Clean)).unwrap();
        assert_eq!(v["plans"], Value::Array(Vec::new()));
        assert_eq!(v["conflicts"], Value::Array(Vec::new()));
    }

    #[test]
    fn strip_marker_line_handles_approve() {
        assert_eq!(strip_marker_line("APPROVE\n\nrest"), "rest");
        assert_eq!(strip_marker_line("APPROVE\nrest"), "rest");
    }

    #[test]
    fn strip_marker_line_handles_request_changes_with_leading_whitespace() {
        assert_eq!(
            strip_marker_line("  \nREQUEST_CHANGES\n\nfindings"),
            "findings"
        );
    }

    #[test]
    fn strip_marker_line_leaves_unmarked_body_intact() {
        assert_eq!(strip_marker_line("some prose\nmore"), "some prose\nmore");
    }

    #[test]
    fn strip_marker_line_consumes_trailing_whitespace() {
        // parse_verdict trims each line; strip_marker_line must agree, or
        // 4+ trailing spaces flip the body into an indented code block at
        // render time.
        assert_eq!(strip_marker_line("APPROVE   \n\nrest"), "rest");
        assert_eq!(strip_marker_line("REQUEST_CHANGES\t  \n\nrest"), "rest");
    }

    #[test]
    fn strip_marker_line_consumes_tab_after_marker() {
        assert_eq!(strip_marker_line("APPROVE\trest\n"), "rest\n");
    }

    #[test]
    fn render_feedback_body_strips_marker_for_verdicts() {
        let html = render_feedback_body("APPROVE\n\n**ok**", Verdict::Approve);
        assert!(html.contains("<strong>ok</strong>"), "got: {html}");
        assert!(!html.contains("APPROVE"));
    }

    #[test]
    fn render_feedback_body_keeps_unmarked_body() {
        let html = render_feedback_body("some prose", Verdict::Unmarked);
        assert!(html.contains("some prose"));
    }

    #[test]
    fn pr_hint_uses_plan_intro_parent_when_present() {
        let session = Plan {
            id: PlanKey::parse("foo").unwrap(),
            plan_path: std::path::PathBuf::from(".trinity/plans/foo.md"),
            state: crate::repo_state::PlanState::Active,
            body: String::new(),
            body_hash: content_hash(""),
            plan_intro: CommitSha::parse("dead").unwrap(),
            plan_intro_parent: Some(CommitSha::parse("ca11").unwrap()),
            commits: BTreeMap::new(),
            frozen_at: None,
            freeze_events: Vec::new(),
            archived_cycles: Vec::new(),
        };
        let v = pr_hint_value(&session, &[]);
        assert_eq!(v["plan_intro_parent"], "ca11");
        // Both options use the parent as the squash base.
        let cmds: Vec<&str> = v["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["command"].as_str().unwrap())
            .collect();
        assert!(cmds.iter().all(|c| c.contains("ca11")));
    }
}
