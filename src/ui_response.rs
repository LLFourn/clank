//! UI-only response builders for the Leptos SPA's `/api/*` surface.
//!
//! These functions are deliberately separate from `mcp_response::*` — the
//! web UI's shape is allowed to be richer than the MCP tool surface (it
//! never costs an agent context-window tokens). The builders take owned
//! snapshot types directly (`RepoSnapshot` / `SessionSnapshotBundle` from
//! `runtime_snapshot`) and never round-trip back into `RepoState`, so all
//! disk I/O happens after the runtime mutex has been released.
//!
//! Disk reads go through the `PlanStatusReader` trait so tests can drop
//! in a fake.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::lifecycle::{AgentLabel, CommitSha, SessionId};
use crate::mcp_response::PlanStatusReader;
use crate::projection::{
    all_implementation_commits_for, all_plan_revisions_for, expected_action, impl_gate_for_parts,
    latest_impl_commit_for, latest_plan_touching_commit_for, phase_for, plan_gate_for_parts,
    waiting_on,
};
use crate::repo_state::{
    AttributionResult, Feedback, HeldFeedback, Phase, PlanTouchKind, Verdict, WaitingOn,
};
use crate::review_state::{ReviewGateDecision, ReviewPhase};
use crate::runtime_snapshot::{RepoSnapshot, SessionSnapshot, SessionSnapshotBundle};

/// `GET /api/sessions[?repo=<path>]` — array of session index rows.
/// Public wrapper binds the production `DiskPlanStatusReader`.
pub fn sessions_index(snapshot: &RepoSnapshot) -> std::io::Result<Value> {
    sessions_index_with_reader(snapshot, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn sessions_index_with_reader(
    snapshot: &RepoSnapshot,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let mut out = Vec::with_capacity(snapshot.sessions.len());
    for session in &snapshot.sessions {
        let session_phase = phase_for(&session.plan_path, &session.id, &snapshot.attribution);
        let plan_gate = plan_gate_for_parts(
            &session.id,
            &session.plan_feedback,
            &snapshot.commit_order,
            &snapshot.plan_touches,
        );
        let impl_gate = impl_gate_for_parts(
            &session.id,
            &session.impl_feedback,
            &snapshot.commit_order,
            &snapshot.attribution,
        );
        let worktree_status =
            status_reader.compute(&snapshot.root, &session.plan_path, &session.body_hash)?;
        let w = waiting_on(
            session_phase,
            worktree_status,
            plan_gate.as_ref(),
            impl_gate.as_ref(),
        );
        out.push(json!({
            "repo": snapshot.root.to_string_lossy(),
            "session_id": session.id.as_str(),
            "plan_path": session.plan_path.to_string_lossy(),
            "phase": session_phase.as_str(),
            "worktree_status": worktree_status.as_str(),
            "waiting_on": waiting_on_value(&w),
        }));
    }
    Ok(Value::Array(out))
}

/// `GET /api/sessions/:id?repo=<path>` — rich session detail.
/// Public wrapper binds the production `DiskPlanStatusReader`.
pub fn session_page(bundle: &SessionSnapshotBundle) -> std::io::Result<Value> {
    session_page_with_reader(bundle, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn session_page_with_reader(
    bundle: &SessionSnapshotBundle,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Value> {
    let session = &bundle.session;
    let worktree_status =
        status_reader.compute(&bundle.root, &session.plan_path, &session.body_hash)?;
    let session_phase = phase_for(&session.plan_path, &session.id, &bundle.attribution);
    let plan_gate = plan_gate_for_parts(
        &session.id,
        &session.plan_feedback,
        &bundle.commit_order,
        &bundle.plan_touches,
    );
    let impl_gate = impl_gate_for_parts(
        &session.id,
        &session.impl_feedback,
        &bundle.commit_order,
        &bundle.attribution,
    );
    let w = waiting_on(
        session_phase,
        worktree_status,
        plan_gate.as_ref(),
        impl_gate.as_ref(),
    );

    let plan_revisions: Vec<String> =
        all_plan_revisions_for(&session.id, &bundle.commit_order, &bundle.plan_touches)
            .into_iter()
            .map(|s| s.as_str().to_string())
            .collect();
    let implementation_commits: Vec<String> =
        all_implementation_commits_for(&session.id, &bundle.commit_order, &bundle.attribution)
            .into_iter()
            .map(|s| s.as_str().to_string())
            .collect();

    let (review_target_phase, review_target_sha) = match session_phase {
        Phase::Planning => (
            "plan",
            latest_plan_touching_commit_for(
                &session.id,
                &bundle.commit_order,
                &bundle.plan_touches,
            )
            .map(|s| s.as_str().to_string()),
        ),
        Phase::Implementing => (
            "impl",
            latest_impl_commit_for(&session.id, &bundle.commit_order, &bundle.attribution)
                .map(|s| s.as_str().to_string()),
        ),
        Phase::Done => ("plan", None),
    };
    let review_target = review_target_sha
        .as_ref()
        .map(|sha| json!({ "phase": review_target_phase, "commit_sha": sha }));
    let latest_plan_revision = plan_revisions
        .last()
        .map(|sha| json!({ "commit_sha": sha }))
        .unwrap_or(Value::Null);
    let latest_implementation_revision = implementation_commits
        .last()
        .map(|sha| json!({ "commit_sha": sha }))
        .unwrap_or(Value::Null);

    let plan_feedback = feedback_entries(&session.plan_feedback);
    let impl_feedback = feedback_entries(&session.impl_feedback);
    let timeline = timeline_value(
        session,
        &bundle.attribution,
        &bundle.plan_touches,
        &bundle.commit_order,
    );
    let pr_hint = if matches!(session_phase, Phase::Implementing) {
        Some(pr_hint_value(session, &implementation_commits))
    } else {
        None
    };

    Ok(json!({
        "repo": bundle.root.to_string_lossy(),
        "session_id": session.id.as_str(),
        "phase": session_phase.as_str(),
        "plan_path": session.plan_path.to_string_lossy(),
        "plan_worktree_status": worktree_status.as_str(),
        "waiting_on": waiting_on_value(&w),
        "expected_action": expected_action(w.reason),
        "review_target": review_target,
        "review_gate": gate_value(plan_gate.as_ref(), impl_gate.as_ref(), session_phase),
        "latest_plan_revision": latest_plan_revision,
        "latest_implementation_revision": latest_implementation_revision,
        "plan_revisions": plan_revisions,
        "implementation_commits": implementation_commits,
        "plan_feedback": plan_feedback,
        "impl_feedback": impl_feedback,
        "held_plan_feedback": held_feedback_entries(&session.held_plan_feedback),
        "timeline": timeline,
        "pr_hint": pr_hint,
    }))
}

/// Build the rich UI feedback entries for one of the `plan_feedback` /
/// `impl_feedback` maps. Each entry carries the raw body, rendered HTML,
/// canonical write path, and file mtime so the UI can sort and display
/// without any further server round-trip.
fn feedback_entries(map: &BTreeMap<(CommitSha, AgentLabel), Feedback>) -> Vec<Value> {
    map.iter()
        .map(|((target, author), fb)| {
            json!({
                "target_sha": target.as_str(),
                "author": author.as_str(),
                "verdict": fb.verdict.as_str(),
                "body_raw": fb.body,
                "body_html": render_feedback_body(&fb.body, fb.verdict),
                "path": fb.path.to_string_lossy(),
                "created_at": fb.created_at,
            })
        })
        .collect()
}

fn held_feedback_entries(held: &[HeldFeedback]) -> Vec<Value> {
    held.iter()
        .map(|h| {
            let verdict = crate::disk_format::parse_verdict(&h.body);
            json!({
                "author": h.author.as_str(),
                "verdict": verdict.as_str(),
                "body_raw": h.body,
                "body_html": render_feedback_body(&h.body, verdict),
                "path": h.path.to_string_lossy(),
                "reason": h.reason,
                "created_at": h.created_at,
            })
        })
        .collect()
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

fn pr_hint_value(session: &SessionSnapshot, impl_commits: &[String]) -> Value {
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
    plan_gate: Option<&ReviewGateDecision>,
    impl_gate: Option<&ReviewGateDecision>,
    session_phase: Phase,
) -> Value {
    let gate = match session_phase {
        Phase::Planning => plan_gate,
        Phase::Implementing => impl_gate,
        Phase::Done => None,
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

fn timeline_value(
    session: &SessionSnapshot,
    attribution: &BTreeMap<CommitSha, AttributionResult>,
    plan_touches: &BTreeMap<CommitSha, Vec<(SessionId, PlanTouchKind)>>,
    commit_order: &[CommitSha],
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
        out.push(json!({
            "kind": kind,
            "sha": sha.as_str(),
            "plan_touch": plan_touch.as_ref().map(|k| k.as_str()),
            "has_code_changes": has_code_changes,
        }));
        for ((target, author), fb) in &session.plan_feedback {
            if target == sha {
                out.push(json!({
                    "kind": "review",
                    "phase": "plan",
                    "target": target.as_str(),
                    "author": author.as_str(),
                    "verdict": fb.verdict.as_str(),
                    "created_at": fb.created_at,
                }));
            }
        }
        for ((target, author), fb) in &session.impl_feedback {
            if target == sha {
                out.push(json!({
                    "kind": "review",
                    "phase": "impl",
                    "target": target.as_str(),
                    "author": author.as_str(),
                    "verdict": fb.verdict.as_str(),
                    "created_at": fb.created_at,
                }));
            }
        }
    }
    for held in &session.held_plan_feedback {
        out.push(json!({
            "kind": "held_feedback",
            "author": held.author.as_str(),
            "reason": held.reason,
            "created_at": held.created_at,
        }));
    }
    out
}

/// Look up feedback entries (rich form) targeting `sha` across both
/// plan and impl feedback maps. Used by the `/api/sessions/:id/plan/:sha`
/// and `/api/sessions/:id/commit/:sha` route handlers.
pub fn feedback_for_target(session: &SessionSnapshot, sha: &CommitSha) -> Vec<Value> {
    let mut out = Vec::new();
    for ((target, author), fb) in &session.plan_feedback {
        if target == sha {
            out.push(feedback_entry(target, author, fb, ReviewPhase::Plan));
        }
    }
    for ((target, author), fb) in &session.impl_feedback {
        if target == sha {
            out.push(feedback_entry(target, author, fb, ReviewPhase::Impl));
        }
    }
    out
}

fn feedback_entry(
    target: &CommitSha,
    author: &AgentLabel,
    fb: &Feedback,
    phase: ReviewPhase,
) -> Value {
    json!({
        "target_sha": target.as_str(),
        "author": author.as_str(),
        "verdict": fb.verdict.as_str(),
        "body_raw": fb.body,
        "body_html": render_feedback_body(&fb.body, fb.verdict),
        "path": fb.path.to_string_lossy(),
        "created_at": fb.created_at,
        "phase": phase.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::content_hash;
    use crate::repo_state::PlanWorktreeStatus;
    use std::path::Path;

    fn empty_snapshot() -> RepoSnapshot {
        RepoSnapshot {
            root: std::path::PathBuf::from("/r"),
            head: None,
            sessions: Vec::new(),
            attribution: BTreeMap::new(),
            plan_touches: BTreeMap::new(),
            commit_order: Vec::new(),
        }
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
    fn sessions_index_empty_snapshot_returns_empty_array() {
        let snap = empty_snapshot();
        let v = sessions_index_with_reader(&snap, &StaticStatusReader(PlanWorktreeStatus::Clean))
            .unwrap();
        assert_eq!(v, Value::Array(Vec::new()));
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
        let session = SessionSnapshot {
            id: SessionId::from("foo"),
            plan_path: std::path::PathBuf::from(".trinity/plans/foo.md"),
            body: String::new(),
            body_hash: content_hash(""),
            plan_intro: CommitSha::from("intro"),
            plan_intro_parent: Some(CommitSha::from("parent")),
            plan_feedback: BTreeMap::new(),
            impl_feedback: BTreeMap::new(),
            held_plan_feedback: Vec::new(),
        };
        let v = pr_hint_value(&session, &[]);
        assert_eq!(v["plan_intro_parent"], "parent");
        // Both options use the parent as the squash base.
        let cmds: Vec<&str> = v["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["command"].as_str().unwrap())
            .collect();
        assert!(cmds.iter().all(|c| c.contains("parent")));
    }
}
