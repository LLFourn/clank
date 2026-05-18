//! UI-only response builders for the Leptos SPA's `/api/*` surface.
//!
//! Each builder constructs a typed `trinity_core` DTO from the
//! daemon's internal state. HTTP route handlers in `server::http`
//! return `axum::Json<TypedDto>` — no `json!` / `Value` in the
//! response shape. UI-flavored DTOs (`CommitRowDetail`,
//! `PlanDetailResponse`) carry richer per-feedback data than the
//! MCP equivalents.
//!
//! Disk reads go through the `PlanStatusReader` trait so tests can
//! drop in a fake.

use crate::lifecycle::{AgentLabel, CommitSha};
use crate::mcp_response::PlanStatusReader;
use crate::projection::{
    all_implementation_commits, all_plan_revisions, current_posture, expected_action,
    impl_gate_for, plan_gate_for, waiting_on,
};
use crate::repo_state::{Plan, RepoState};
use crate::review_state::CommitGate;
use trinity_core::dto::{
    ArchivedCycle, CommitRef, CommitRowDetail, Feedback, ListPlansResponse, PlanConflict,
    PlanDetailResponse, PlanRow, PrHint, PrHintOption, ReviewGate, ReviewTarget, TimelineEvent,
    WaitingOn,
};
use trinity_core::vocab::{CommitKind, PlanTouchKind, Posture, ReviewTargetPhase, Verdict};

/// `GET /api/plans` — `{ plans, conflicts }` for the home page.
pub fn plans_index(snapshot: &RepoState) -> std::io::Result<ListPlansResponse> {
    plans_index_with_reader(snapshot, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plans_index_with_reader(
    snapshot: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<ListPlansResponse> {
    let (plans, conflicts) = plans_index_parts(snapshot, status_reader)?;
    Ok(ListPlansResponse { plans, conflicts })
}

/// Aggregated `{ plans, conflicts }` across multiple snapshots,
/// sorted by `last_activity_ts` desc.
pub fn plans_index_across(snapshots: &[RepoState]) -> std::io::Result<ListPlansResponse> {
    plans_index_across_with_reader(snapshots, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plans_index_across_with_reader(
    snapshots: &[RepoState],
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<ListPlansResponse> {
    let mut all_plans: Vec<PlanRow> = Vec::new();
    let mut all_conflicts: Vec<PlanConflict> = Vec::new();
    for snapshot in snapshots {
        let (plans, conflicts) = plans_index_parts(snapshot, status_reader)?;
        all_plans.extend(plans);
        all_conflicts.extend(conflicts);
    }
    all_plans.sort_by_key(|p| std::cmp::Reverse(p.last_activity_ts));
    Ok(ListPlansResponse {
        plans: all_plans,
        conflicts: all_conflicts,
    })
}

fn plans_index_parts(
    snapshot: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<(Vec<PlanRow>, Vec<PlanConflict>)> {
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&snapshot.root);
    let mut plans: Vec<PlanRow> = Vec::with_capacity(snapshot.plans.len());
    for plan in snapshot.plans.values() {
        let worktree_status =
            status_reader.compute(&snapshot.root, &plan.plan_path, &plan.body_hash)?;
        if !plan.is_visible(worktree_status) {
            continue;
        }
        let plan_phase = current_posture(plan, snapshot);
        let gate = crate::projection::latest_reviewable_commit_gate_for(plan);
        let w = waiting_on(plan.is_frozen(), worktree_status, gate);
        let plan_id = basename
            .as_ref()
            .map(|b| crate::lifecycle::PlanId::new(b.clone(), plan.id.clone()).to_string());
        let lifecycle = plan.lifecycle();
        plans.push(PlanRow {
            repo: snapshot.root.to_string_lossy().to_string(),
            plan_id,
            slug: plan.id.as_str().to_string(),
            lifecycle,
            current_path: plan.plan_path.to_string_lossy().to_string(),
            phase: plan_phase,
            plan_worktree_status: worktree_status,
            waiting_on: build_waiting_on(&w),
            archived_cycles: plan.archived_cycles.iter().map(build_archived).collect(),
            last_activity_ts: crate::projection::last_activity_ts_for(plan),
        });
    }
    plans.sort_by_key(|p| std::cmp::Reverse(p.last_activity_ts));
    let conflicts: Vec<PlanConflict> = snapshot
        .plan_conflicts
        .iter()
        .map(|(key, paths)| {
            let plan_id = basename
                .as_ref()
                .map(|b| crate::lifecycle::PlanId::new(b.clone(), key.clone()).to_string());
            PlanConflict {
                plan_id,
                slug: key.as_str().to_string(),
                paths: paths
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect(),
            }
        })
        .collect();
    Ok((plans, conflicts))
}

/// `GET /api/plan/{repo}/{stem_md}` — rich plan detail. Returns
/// `Ok(None)` when the plan is hidden (active + plan file missing
/// from the working tree); the HTTP handler maps that to a 404.
pub fn plan_page(bundle: &RepoState) -> std::io::Result<Option<PlanDetailResponse>> {
    plan_page_with_reader(bundle, &crate::mcp_response::DiskPlanStatusReader)
}

pub fn plan_page_with_reader(
    bundle: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Option<PlanDetailResponse>> {
    let plan = bundle
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&bundle.root);
    let worktree_status = status_reader.compute(&bundle.root, &plan.plan_path, &plan.body_hash)?;
    if !plan.is_visible(worktree_status) {
        return Ok(None);
    }
    let plan_phase = current_posture(plan, bundle);
    let plan_gate = plan_gate_for(plan, bundle);
    let impl_gate = impl_gate_for(plan, bundle);
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan);
    let w = waiting_on(plan.is_frozen(), worktree_status, gate);
    let review_target_phase = posture_to_review_target_phase(plan_phase);

    let plan_revisions: Vec<String> = all_plan_revisions(plan, bundle)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let implementation_commits: Vec<String> = all_implementation_commits(plan, bundle)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();

    let review_target_sha = crate::projection::latest_reviewable_commit_for(plan);
    let review_target = review_target_sha.as_ref().map(|sha| ReviewTarget {
        commit_sha: sha.as_str().to_string(),
        phase: review_target_phase,
    });
    let latest_plan_revision = plan_revisions.last().map(|sha| CommitRef {
        commit_sha: sha.clone(),
    });
    let latest_implementation_revision = implementation_commits.last().map(|sha| CommitRef {
        commit_sha: sha.clone(),
    });

    let timeline = build_timeline(plan);
    let pr_hint = if matches!(plan_phase, Posture::Implementing) {
        Some(build_pr_hint(plan, &implementation_commits))
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

    let commits = build_commits_array_rich(plan);
    let latest_relevant_commit =
        crate::projection::latest_reviewable_commit_for(plan).map(|s| s.as_str().to_string());

    let lifecycle = plan.lifecycle();
    let archived_cycles = plan.archived_cycles.iter().map(build_archived).collect();

    Ok(Some(PlanDetailResponse {
        repo: bundle.root.to_string_lossy().to_string(),
        plan_id,
        slug: plan.id.as_str().to_string(),
        lifecycle,
        current_path: plan.plan_path.to_string_lossy().to_string(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        waiting_on: build_waiting_on(&w),
        expected_action: expected_action(w.reason),
        review_target,
        review_gate: build_review_gate(plan_gate, impl_gate, plan_phase),
        latest_plan_revision,
        latest_implementation_revision,
        plan_revisions,
        implementation_commits,
        commits,
        latest_relevant_commit,
        plan_body_html,
        plan_body_truncated,
        timeline,
        pr_hint,
        archived_cycles,
    }))
}

// ============================================================
// Composing helpers
// ============================================================

fn build_waiting_on(w: &crate::repo_state::WaitingOn) -> WaitingOn {
    WaitingOn {
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

fn posture_to_review_target_phase(p: Posture) -> ReviewTargetPhase {
    match p {
        Posture::Planning => ReviewTargetPhase::Plan,
        Posture::Implementing => ReviewTargetPhase::Impl,
    }
}

/// Per-commit `commits[]` array (UI flavor, full feedback bodies).
fn build_commits_array_rich(plan: &Plan) -> Vec<CommitRowDetail> {
    let mut out = Vec::new();
    for event in &plan.timeline {
        if !event.kind.is_reviewable() {
            continue;
        }
        let Some(gate) = event.gate.as_ref() else {
            continue;
        };
        let feedback: Vec<Feedback> = gate
            .feedback
            .iter()
            .map(|(author, fb)| build_rich_feedback(author, fb))
            .collect();
        out.push(CommitRowDetail {
            sha: event.sha.as_str().to_string(),
            kind: event.kind,
            gate: Some(build_commit_gate(gate)),
            feedback,
        });
    }
    out
}

fn build_commit_gate(g: &CommitGate) -> trinity_core::dto::CommitGate {
    trinity_core::dto::CommitGate {
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
            .map(|(author, fb)| (author.as_str().to_string(), build_rich_feedback(author, fb)))
            .collect(),
    }
}

fn build_rich_feedback(author: &AgentLabel, fb: &crate::repo_state::Feedback) -> Feedback {
    Feedback {
        author: author.as_str().to_string(),
        verdict: fb.verdict,
        body_raw: fb.body.clone(),
        body_html: render_feedback_body(&fb.body, fb.verdict),
        path: fb.path.to_string_lossy().to_string(),
        created_at: fb.created_at,
    }
}

fn build_timeline(session: &Plan) -> Vec<TimelineEvent> {
    let mut out = Vec::with_capacity(session.timeline.len() * 2);
    for event in &session.timeline {
        let sha = event.sha.as_str().to_string();
        let subject = event.subject.clone();
        let plan_touch = if event.sha == session.plan_intro {
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
            // Reviews can only be attached to reviewable events; the
            // match below makes a future non-reviewable-with-gate
            // combination a compile error.
            let phase = match event.kind {
                CommitKind::PlanOnly | CommitKind::Mixed => ReviewTargetPhase::Plan,
                CommitKind::CodeOnly => ReviewTargetPhase::Impl,
                CommitKind::MultiPlan | CommitKind::Finalize | CommitKind::Unattributed => {
                    debug_assert!(
                        false,
                        "non-reviewable kind carries a gate: {:?}",
                        event.kind
                    );
                    continue;
                }
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

fn build_pr_hint(session: &Plan, impl_commits: &[String]) -> PrHint {
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
        implementation_commits: impl_commits.to_vec(),
        options,
        suggested_message: suggested,
    }
}

fn build_review_gate(
    plan_gate: Option<&CommitGate>,
    impl_gate: Option<&CommitGate>,
    session_phase: Posture,
) -> Option<ReviewGate> {
    let phase = posture_to_review_target_phase(session_phase);
    let gate = match session_phase {
        Posture::Planning => plan_gate,
        Posture::Implementing => impl_gate,
    }?;
    Some(ReviewGate {
        state: gate.state.into(),
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

// ============================================================
// Markdown / feedback rendering
// ============================================================

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

/// Look up feedback entries (rich form) targeting `sha`. Reads from
/// the targeted `PlanTimelineEvent`'s gate. Used by the
/// `/api/plan/.../revision/{sha}` and `/api/plan/.../commit/{sha}`
/// route handlers. Returns empty if the SHA isn't on this plan's
/// timeline or the event is non-reviewable (no gate).
pub fn feedback_for_target(session: &Plan, sha: &CommitSha) -> Vec<Feedback> {
    let Some(event) = session.event_for(sha) else {
        return Vec::new();
    };
    let Some(gate) = event.gate.as_ref() else {
        return Vec::new();
    };
    gate.feedback
        .iter()
        .map(|(author, fb)| build_rich_feedback(author, fb))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{PlanKey, content_hash};
    use std::path::Path;
    use trinity_core::vocab::PlanWorktreeStatus;

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
        let resp =
            plans_index_with_reader(&snap, &StaticStatusReader(PlanWorktreeStatus::Clean)).unwrap();
        assert!(resp.plans.is_empty());
        assert!(resp.conflicts.is_empty());
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
            body: String::new(),
            body_hash: content_hash(""),
            plan_intro: CommitSha::parse("dead").unwrap(),
            plan_intro_parent: Some(CommitSha::parse("ca11").unwrap()),
            last_activity_ts: 0,
            timeline: Vec::new(),
            archived_cycles: Vec::new(),
        };
        let hint = build_pr_hint(&session, &[]);
        assert_eq!(hint.plan_intro_parent.as_deref(), Some("ca11"));
        assert!(hint.options.iter().all(|o| o.command.contains("ca11")));
    }
}
