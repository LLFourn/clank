//! The daemon's sole `model → api` projection module.
//!
//! Every HTTP route and every MCP tool that returns a response
//! shape comes through here. The wire crate (`trinity_core::api`)
//! owns the type definitions; this module owns the per-endpoint
//! field selection plus the body_html rendering for `Feedback`.
//!
//! Disk reads go through the [`PlanStatusReader`] trait so tests
//! can drop in a fake.

use std::path::Path;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanId, RepoBasename, content_hash};
use crate::projection::{
    all_implementation_commits, all_plan_revisions, current_posture, expected_action,
    impl_gate_for, plan_gate_for, plan_worktree_status, waiting_on,
};
use crate::repo_state::{Plan, PlanWorktreeStatus, RepoState};
use crate::review_state::CommitGate;
use trinity_core::api::{
    CommitRef, CommitRow, Feedback, GetContextResponse, ListPlansResponse, PlanConflict,
    PlanDetailResponse, PlanRow, PrHint, PrHintOption, ReviewGate, ReviewTarget, TimelineEvent,
    WriteFeedback,
};
use trinity_core::vocab::{CommitKind, PlanTouchKind, Posture, ReviewTargetPhase, Verdict};

// ============================================================
// PlanStatusReader trait — disk reads through here so tests fake
// ============================================================

pub trait PlanStatusReader {
    fn compute(
        &self,
        repo_root: &Path,
        plan_path: &str,
        body_hash: &ContentHash,
    ) -> std::io::Result<PlanWorktreeStatus>;
}

pub struct DiskPlanStatusReader;

impl PlanStatusReader for DiskPlanStatusReader {
    fn compute(
        &self,
        repo_root: &Path,
        plan_path: &str,
        body_hash: &ContentHash,
    ) -> std::io::Result<PlanWorktreeStatus> {
        compute_plan_worktree_status_parts(repo_root, plan_path, body_hash)
    }
}

/// Compare the working-tree plan file to HEAD's blob hash.
pub fn compute_plan_worktree_status_parts(
    repo_root: &Path,
    plan_path: &str,
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
// list_plans / /api/plans
// ============================================================

pub fn list_plans_response(state: &RepoState) -> std::io::Result<ListPlansResponse> {
    list_plans_response_with_reader(state, &DiskPlanStatusReader)
}

pub fn list_plans_response_with_reader(
    state: &RepoState,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<ListPlansResponse> {
    let (plans, conflicts) = plans_index_parts(state, status_reader)?;
    Ok(ListPlansResponse { plans, conflicts })
}

/// Aggregated `{ plans, conflicts }` across multiple snapshots,
/// sorted by `last_activity_ts` desc. Used by HTTP `/api/plans`.
pub fn plans_index_across(snapshots: &[RepoState]) -> std::io::Result<ListPlansResponse> {
    plans_index_across_with_reader(snapshots, &DiskPlanStatusReader)
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
    let mut plans: Vec<PlanRow> = Vec::with_capacity(snapshot.plans.len());
    for plan in snapshot.plans.values() {
        let worktree_status =
            status_reader.compute(&snapshot.root, &plan.plan_path, &plan.body_hash)?;
        if !plan.is_visible(worktree_status) {
            continue;
        }
        plans.push(build_plan_row(
            &snapshot.root,
            plan,
            worktree_status,
            snapshot,
        ));
    }
    plans.sort_by_key(|p| std::cmp::Reverse(p.last_activity_ts));
    let conflicts: Vec<PlanConflict> = snapshot
        .plan_conflicts
        .iter()
        .map(|(key, paths)| PlanConflict {
            plan_id: plan_id_string(&snapshot.root, key),
            slug: key.as_str().to_string(),
            paths: paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
        })
        .collect();
    Ok((plans, conflicts))
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
        lifecycle,
        current_path: plan.plan_path.clone(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        waiting_on: w,
        archived_cycles: plan.archived_cycles.clone(),
        last_activity_ts: crate::projection::last_activity_ts_for(plan),
    }
}

// ============================================================
// get_context (MCP)
// ============================================================

pub fn get_context_response(
    snapshot: &RepoState,
    author_label: &AgentLabel,
) -> std::io::Result<Option<GetContextResponse>> {
    get_context_response_with_reader(snapshot, author_label, &DiskPlanStatusReader)
}

pub fn get_context_response_with_reader(
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
    let plan = state
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let plan_phase = current_posture(plan, state);
    let plan_gate = plan_gate_for(plan, state);
    let impl_gate = impl_gate_for(plan, state);
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan);
    let w = waiting_on(plan.is_frozen(), worktree_status, gate);
    let review_target_phase = posture_to_review_target_phase(plan_phase);

    let review_target_sha = crate::projection::latest_reviewable_commit_for(plan);
    let review_target = review_target_sha.as_ref().map(|sha| ReviewTarget {
        commit_sha: sha.as_str().to_string(),
        phase: review_target_phase,
    });
    let write_feedback = review_target_sha.as_ref().map(|sha| WriteFeedback {
        phase: review_target_phase,
        target_sha: sha.as_str().to_string(),
        path: format!(
            ".trinity/feedback/{session}/{sha}/{author}.md",
            session = plan.id.as_str(),
            sha = sha.as_str(),
            author = author_label.as_str(),
        ),
    });
    let plan_revisions: Vec<String> = all_plan_revisions(plan, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let implementation_commits: Vec<String> = all_implementation_commits(plan, state)
        .into_iter()
        .map(|s| s.as_str().to_string())
        .collect();
    let lifecycle = plan.lifecycle();
    let pr_hint = if matches!(plan_phase, Posture::Implementing) {
        Some(build_pr_hint(
            plan,
            &all_implementation_commits(plan, state)
                .into_iter()
                .map(|s| s.as_str().to_string())
                .collect::<Vec<_>>(),
        ))
    } else {
        None
    };

    let commits = build_commits_array(plan);
    let latest_relevant_commit = review_target_sha
        .as_ref()
        .map(|sha| sha.as_str().to_string());
    let timeline = build_timeline(plan);
    let review_gate = build_review_gate(plan_gate, impl_gate, plan_phase);

    GetContextResponse {
        repo: state.root.to_string_lossy().to_string(),
        plan_id: plan_id_string(&state.root, &plan.id),
        slug: plan.id.as_str().to_string(),
        lifecycle,
        current_path: plan.plan_path.clone(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        expected_action: expected_action(w.reason),
        waiting_on: w,
        review_target,
        write_feedback,
        review_gate,
        latest_plan_revision: plan_revisions.last().map(|s: &String| CommitRef {
            commit_sha: s.clone(),
        }),
        latest_implementation_revision: implementation_commits.last().map(|s: &String| CommitRef {
            commit_sha: s.clone(),
        }),
        plan_revisions,
        implementation_commits,
        commits,
        latest_relevant_commit,
        timeline,
        pr_hint,
        archived_cycles: plan.archived_cycles.clone(),
    }
}

// ============================================================
// /api/plan/{repo}/{stem_md}
// ============================================================

pub fn plan_page(bundle: &RepoState) -> std::io::Result<Option<PlanDetailResponse>> {
    plan_page_with_reader(bundle, &DiskPlanStatusReader)
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

    let plan_id = plan_id_string(&bundle.root, &plan.id);
    let plan_body_html = render_markdown(&plan.body);
    let plan_body_truncated = plan.body.chars().count() > 4000;
    let commits = build_commits_array(plan);
    let latest_relevant_commit = review_target_sha
        .as_ref()
        .map(|sha| sha.as_str().to_string());
    let review_gate = build_review_gate(plan_gate, impl_gate, plan_phase);

    Ok(Some(PlanDetailResponse {
        repo: bundle.root.to_string_lossy().to_string(),
        plan_id,
        slug: plan.id.as_str().to_string(),
        lifecycle: plan.lifecycle(),
        current_path: plan.plan_path.clone(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        expected_action: expected_action(w.reason),
        waiting_on: w,
        review_target,
        review_gate,
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
        archived_cycles: plan.archived_cycles.clone(),
    }))
}

// ============================================================
// Shared sub-builders
// ============================================================

fn plan_id_string(repo_root: &Path, plan_key: &crate::lifecycle::PlanKey) -> Option<String> {
    RepoBasename::from_repo_root(repo_root).map(|b| PlanId::new(b, plan_key.clone()).to_string())
}

fn posture_to_review_target_phase(p: Posture) -> ReviewTargetPhase {
    match p {
        Posture::Planning => ReviewTargetPhase::Plan,
        Posture::Implementing => ReviewTargetPhase::Impl,
    }
}

/// Per-commit `commits[]` array. Both MCP and HTTP get the same
/// shape with full feedback bodies — the wire collapse plan moved
/// MCP from a `{author, verdict}` summary to the full shape.
fn build_commits_array(plan: &Plan) -> Vec<CommitRow> {
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
            .map(|(author, fb)| build_feedback(author, fb))
            .collect();
        out.push(CommitRow {
            sha: event.sha.as_str().to_string(),
            kind: event.kind,
            gate: Some(build_commit_gate(gate)),
            feedback,
        });
    }
    out
}

/// model::CommitGate → api::CommitGate. AgentLabel is serde-
/// transparent so the wire form of participants/etc. is unchanged
/// from the prior `Vec<String>`; the only real translation here is
/// the per-author Feedback projection (raw markdown → rendered).
fn build_commit_gate(g: &CommitGate) -> trinity_core::api::CommitGate {
    trinity_core::api::CommitGate {
        state: g.state,
        participants: g.participants.clone(),
        approvers: g.approvers.clone(),
        requesters: g.requesters.clone(),
        ambiguous: g.ambiguous.clone(),
        missing: g.missing.clone(),
        feedback: g
            .feedback
            .iter()
            .map(|(author, fb)| (author.clone(), build_feedback(author, fb)))
            .collect(),
    }
}

fn build_feedback(author: &AgentLabel, fb: &crate::repo_state::Feedback) -> Feedback {
    Feedback {
        author: author.as_str().to_string(),
        verdict: fb.verdict,
        body_raw: fb.body.clone(),
        body_html: render_feedback_body(&fb.body, fb.verdict),
        path: fb.path.clone(),
        created_at: fb.created_at,
    }
}

fn build_timeline(plan: &Plan) -> Vec<TimelineEvent> {
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
            // Reviews can only attach to reviewable events; an
            // unreviewable kind here is a fold-invariant violation.
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

fn build_pr_hint(plan: &Plan, impl_commits: &[String]) -> PrHint {
    let plan_intro = plan.plan_intro.as_str().to_string();
    let plan_intro_parent = plan
        .plan_intro_parent
        .as_ref()
        .map(|s| s.as_str().to_string());
    let base_for_squash = plan_intro_parent
        .clone()
        .unwrap_or_else(|| plan_intro.clone());
    let plan_path = plan.plan_path.clone();
    let suggested = format!("Implement {}", plan.id.as_str());
    let options = vec![
        PrHintOption {
            kind: trinity_core::PrHintOptionKind::KeepPlanInPr,
            base: base_for_squash.clone(),
            command: format!("git reset --soft {base_for_squash} && git commit -m '{suggested}'"),
        },
        PrHintOption {
            kind: trinity_core::PrHintOptionKind::ExcludePlanFromPr,
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
    plan_phase: Posture,
) -> Option<ReviewGate> {
    let phase = posture_to_review_target_phase(plan_phase);
    let gate = match plan_phase {
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

/// Consume any trailing horizontal whitespace (spaces / tabs) on
/// the marker line, then up to one CR/LF run. Matches the leniency
/// of `disk_format::parse_verdict` which trims each candidate line
/// before matching the marker.
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
/// route handlers.
pub fn feedback_for_target(plan: &Plan, sha: &CommitSha) -> Vec<Feedback> {
    let Some(event) = plan.event_for(sha) else {
        return Vec::new();
    };
    let Some(gate) = event.gate.as_ref() else {
        return Vec::new();
    };
    gate.feedback
        .iter()
        .map(|(author, fb)| build_feedback(author, fb))
        .collect()
}
