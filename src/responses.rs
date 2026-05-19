//! The daemon's sole `model → api` projection module.
//!
//! Every HTTP route and every MCP tool that returns a response
//! shape comes through here. The wire crate (`trinity_core::api`)
//! owns the type definitions; this module owns the per-endpoint
//! field selection. After `wasm-markdown-rendering.md`, that
//! selection is essentially identity — the wire ships raw
//! markdown and the wasm frontend renders at display time, so
//! `Feedback` and `CommitGate` projections are clones.
//!
//! Disk reads go through the [`PlanStatusReader`] trait so tests
//! can drop in a fake.

use std::path::Path;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanId, RepoBasename, content_hash};
use crate::projection::{
    all_implementation_commits, all_plan_revisions, current_posture, impl_gate_for, plan_gate_for,
    plan_worktree_status, waiting_on,
};
use crate::repo_state::{Plan, PlanWorktreeStatus, RepoState};
use crate::review_state::CommitGate;
use trinity_core::api::{
    CommitRef, CommitRow, Feedback, ListPlansResponse, PlanConflict, PlanDetailResponse, PlanRow,
    PrHint, PrHintOption, ReviewGate, ReviewTarget, TimelineEvent, WorkContextResponse,
};
use trinity_core::vocab::{CommitKind, PlanTouchKind, Posture, ReviewTargetPhase};

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
// work_context (MCP)
// ============================================================

pub fn work_context_response(
    snapshot: &RepoState,
    author_label: &AgentLabel,
) -> std::io::Result<Option<WorkContextResponse>> {
    work_context_response_with_reader(snapshot, author_label, &DiskPlanStatusReader)
}

pub fn work_context_response_with_reader(
    snapshot: &RepoState,
    author_label: &AgentLabel,
    status_reader: &impl PlanStatusReader,
) -> std::io::Result<Option<WorkContextResponse>> {
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
    Ok(Some(work_context_response_from_snapshot(
        snapshot,
        worktree_status,
        author_label,
    )))
}

pub fn work_context_response_from_snapshot(
    state: &RepoState,
    worktree_status: PlanWorktreeStatus,
    author_label: &AgentLabel,
) -> WorkContextResponse {
    let plan = state
        .plans
        .values()
        .next()
        .expect("snapshot_session invariant: exactly one plan");
    let plan_phase = current_posture(plan, state);
    let gate = crate::projection::latest_reviewable_commit_gate_for(plan);
    let w = waiting_on(plan.is_frozen(), worktree_status, gate);
    let plan_id = plan_id_string(&state.root, &plan.id).expect("active plan must have a plan_id");
    let review_target_sha = crate::projection::latest_reviewable_commit_for(plan);
    let review_target_event = review_target_sha
        .as_ref()
        .and_then(|sha| plan.event_for(sha));
    let review_target_kind = review_target_event.map(|e| e.kind());
    let requesters: Vec<AgentLabel> = review_target_event
        .and_then(|e| e.gate())
        .map(|g| g.requesters.clone())
        .unwrap_or_default();
    let work = build_work_payload(WorkPayloadInputs {
        plan_id: &plan_id,
        repo_root: &state.root,
        plan_key: &plan.id,
        plan_path: &plan.plan_path,
        waiting: &w,
        review_target_sha: review_target_sha.as_ref(),
        review_target_kind,
        requesters: &requesters,
        author: author_label,
    });

    WorkContextResponse {
        work,
        current_path: plan.plan_path.clone(),
        lifecycle: plan.lifecycle(),
        phase: plan_phase,
        plan_worktree_status: worktree_status,
        waiting_on: w,
    }
}

/// Inputs to [`build_work_payload`]. Carries everything the
/// projection needs without forcing the caller to hold a `Plan`
/// reference across lock boundaries.
///
/// `wait_for_work` constructs this from its `Candidate`
/// snapshot; `work_context` constructs it from the per-plan
/// `Plan` it has in hand. Both routes funnel into the same
/// builder so the work-prefix on both surfaces stays identical
/// by construction.
pub struct WorkPayloadInputs<'a> {
    pub plan_id: &'a str,
    pub repo_root: &'a Path,
    pub plan_key: &'a crate::lifecycle::PlanKey,
    pub plan_path: &'a str,
    pub waiting: &'a trinity_core::api::WaitingOn,
    pub review_target_sha: Option<&'a crate::lifecycle::CommitSha>,
    pub review_target_kind: Option<CommitKind>,
    /// Authors of REQUEST_CHANGES feedback at the review target —
    /// used to build `rc_paths` for `AddressChanges`. Empty for
    /// non-AddressChanges reasons.
    pub requesters: &'a [AgentLabel],
    pub author: &'a AgentLabel,
}

/// Project the per-plan state into a `WorkPayload`. **Sole**
/// constructor — `wait_for_work` and `work_context` both call
/// this so the work-prefix on both surfaces stays identical by
/// construction. If you find yourself building a `WorkPayload`
/// elsewhere, route through this instead.
pub fn build_work_payload(inputs: WorkPayloadInputs<'_>) -> trinity_core::api::WorkPayload {
    use trinity_core::api::ExpectedAction as A;
    use trinity_core::vocab::WaitingReason as R;
    let plan_side = matches!(
        inputs.review_target_kind,
        Some(CommitKind::PlanOnly | CommitKind::Mixed)
    );
    let feedback_path_for = |sha: &str, author: &str| -> String {
        format!(
            ".trinity/feedback/{session}/{sha}/{author}.md",
            session = inputs.plan_key.as_str(),
            sha = sha,
            author = author,
        )
    };
    let action = match inputs.waiting.reason {
        R::CommitNeedsReview => {
            let sha = inputs
                .review_target_sha
                .expect("CommitNeedsReview implies a review target")
                .as_str()
                .to_string();
            A::WriteFeedback {
                path: feedback_path_for(&sha, inputs.author.as_str()),
                target_sha: sha,
                plan_file: trinity_core::api::PlanFile {
                    path: inputs.plan_path.to_string(),
                    content: None,
                },
            }
        }
        R::AddressCommitChanges => {
            let sha = inputs
                .review_target_sha
                .expect("AddressCommitChanges implies a review target")
                .as_str()
                .to_string();
            let reviews = inputs
                .requesters
                .iter()
                .map(|author| trinity_core::api::CurrentReview {
                    path: feedback_path_for(&sha, author.as_str()),
                    author: author.clone(),
                    verdict: trinity_core::vocab::Verdict::RequestChanges,
                    content: None,
                })
                .collect();
            A::AddressChanges {
                target_sha: sha,
                reviews,
                plan_path: if plan_side {
                    Some(inputs.plan_path.to_string())
                } else {
                    None
                },
            }
        }
        R::CommitPlanRevision => A::CommitPlanRevision {
            plan_path: inputs.plan_path.to_string(),
        },
        R::ReadyToStartImplementation => A::StartImplementation {
            previous_commit: inputs
                .review_target_sha
                .expect("ReadyToStartImplementation implies a previous commit")
                .as_str()
                .to_string(),
            plan_path: inputs.plan_path.to_string(),
        },
        R::SessionFinished => A::SessionFinished,
    };
    trinity_core::api::WorkPayload {
        plan_id: inputs.plan_id.to_string(),
        repo: inputs.repo_root.to_string_lossy().into_owned(),
        action,
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
        waiting_on: w,
        review_target,
        review_gate,
        latest_plan_revision,
        latest_implementation_revision,
        plan_revisions,
        implementation_commits,
        commits,
        latest_relevant_commit,
        plan_body: plan.body.clone(),
        timeline,
        pr_hint,
        archived_cycles: plan.archived_cycles.clone(),
    }))
}

// ============================================================
// Per-endpoint top-level response builders
// ============================================================
//
// These wrap shape-construction for HTTP-only endpoints. Each
// handler does the IO (git show, snapshot lookup, 404 checks) and
// hands the inputs to the matching builder here, so every wire
// shape lives in this one module.

/// `GET /api/plan/<id>/revision/<sha>` — wire shape construction
/// after the handler has resolved the body, the plan-revision
/// list position, and the per-target feedback.
pub fn build_plan_revision_response(
    snapshot: &RepoState,
    plan: &Plan,
    plan_id_str: String,
    commit_sha: &CommitSha,
    body: String,
    plan_revisions: &[CommitSha],
    pos: usize,
) -> trinity_core::api::PlanRevisionResponse {
    let previous_sha = pos
        .checked_sub(1)
        .and_then(|j| plan_revisions.get(j))
        .map(|c| c.as_str().to_string());
    let next_sha = plan_revisions.get(pos + 1).map(|c| c.as_str().to_string());
    let feedback = feedback_for_target(plan, commit_sha);
    trinity_core::api::PlanRevisionResponse {
        repo: snapshot.root.to_string_lossy().to_string(),
        plan_id: plan_id_str,
        slug: plan.id.as_str().to_string(),
        commit_sha: commit_sha.as_str().to_string(),
        body,
        plan_intro: plan.plan_intro.as_str().to_string(),
        plan_intro_parent: plan
            .plan_intro_parent
            .as_ref()
            .map(|s| s.as_str().to_string()),
        previous_sha,
        next_sha,
        feedback,
    }
}

/// Inputs to [`build_commit_detail_response`]. Groups the
/// handler's IO results so the builder signature stays narrow
/// even as the wire shape composes more daemon-side resolutions.
pub struct CommitDetailInputs<'a> {
    pub snapshot: &'a RepoState,
    pub plan: &'a Plan,
    pub plan_id_str: String,
    pub commit_sha: &'a CommitSha,
    pub event: &'a trinity_core::model::PlanTimelineEvent,
    pub subject: String,
    pub message_body: String,
    pub diff_files: Vec<trinity_core::api::FileDiff>,
    /// `(filename, body)` pairs from the freeze commit's
    /// `.trinity/finished/<stem>/` tree. Empty for non-Finalize
    /// events.
    pub finalize_files: Vec<(String, String)>,
}

/// `GET /api/plan/<id>/commit/<sha>` — wire shape construction
/// after the handler has resolved the commit message, structured
/// diff, and (for Finalize) the approval snapshot files.
///
/// Panics on `CommitKind::Unattributed` — the handler MUST reject
/// that case before calling this builder. The daemon never emits
/// a `CommitDetailResponse` for an unattributed commit.
pub fn build_commit_detail_response(
    inputs: CommitDetailInputs<'_>,
) -> trinity_core::api::CommitDetailResponse {
    use trinity_core::api::{CommitDetail, CommitDetailResponse, FinalizeApproval};
    use trinity_core::vocab::CommitKind;
    let CommitDetailInputs {
        snapshot,
        plan,
        plan_id_str,
        commit_sha,
        event,
        subject,
        message_body,
        diff_files,
        finalize_files,
    } = inputs;
    let detail = match event.kind() {
        CommitKind::Finalize => {
            let snapshot_entries = finalize_files
                .into_iter()
                .map(|(filename, body)| {
                    let author = filename
                        .strip_suffix(".md")
                        .unwrap_or(&filename)
                        .to_string();
                    FinalizeApproval {
                        author,
                        filename,
                        body,
                    }
                })
                .collect();
            CommitDetail::Finalize {
                snapshot: snapshot_entries,
            }
        }
        CommitKind::PlanOnly => CommitDetail::PlanOnly {
            feedback: feedback_for_target(plan, commit_sha),
        },
        CommitKind::CodeOnly => CommitDetail::CodeOnly {
            feedback: feedback_for_target(plan, commit_sha),
        },
        CommitKind::Mixed => CommitDetail::Mixed {
            feedback: feedback_for_target(plan, commit_sha),
        },
        CommitKind::MultiPlan => CommitDetail::MultiPlan {},
        CommitKind::Unattributed => unreachable!(
            "build_commit_detail_response called with Unattributed kind; \
             the handler must reject this before reaching the builder"
        ),
    };
    CommitDetailResponse {
        repo: snapshot.root.to_string_lossy().to_string(),
        plan_id: plan_id_str,
        slug: plan.id.as_str().to_string(),
        commit_sha: commit_sha.as_str().to_string(),
        subject,
        message_body,
        diff_files,
        detail,
    }
}

/// `GET /api/plan/<id>/diff/<from>/<to>` — wire shape construction
/// for the plan-file diff between two revisions.
pub fn build_diff_response(
    from_sha: &CommitSha,
    to_sha: &CommitSha,
    from_path: &std::path::Path,
    to_path: &std::path::Path,
    diff_files: Vec<trinity_core::api::FileDiff>,
) -> trinity_core::api::DiffResponse {
    trinity_core::api::DiffResponse {
        from: from_sha.as_str().to_string(),
        to: to_sha.as_str().to_string(),
        from_path: from_path.to_string_lossy().to_string(),
        to_path: to_path.to_string_lossy().to_string(),
        diff_files,
    }
}

/// `GET /api/repos` — sort and shape the watched-repo list.
pub fn build_repo_list_response(
    trinity: &crate::repo_state::Trinity,
) -> trinity_core::api::RepoListResponse {
    let mut rows: Vec<(i64, trinity_core::api::RepoRow)> =
        Vec::with_capacity(trinity.repo_basenames.len());
    for (basename, root) in &trinity.repo_basenames {
        let Some(repo_state) = trinity.repos.get(root) else {
            continue;
        };
        let mut last_ts: i64 = 0;
        for plan in repo_state.plans.values() {
            last_ts = last_ts.max(crate::projection::last_activity_ts_for(plan));
        }
        rows.push((
            last_ts,
            trinity_core::api::RepoRow {
                basename: basename.as_str().to_string(),
                root: root.to_string_lossy().to_string(),
                plan_count: repo_state.plans.len(),
                last_activity_ts: last_ts,
            },
        ));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    let repos = rows.into_iter().map(|(_, v)| v).collect();
    trinity_core::api::RepoListResponse { repos }
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
        let Some(gate) = event.gate() else {
            continue;
        };
        let sha = event.sha().as_str().to_string();
        let feedback: Vec<Feedback> = gate.feedback.values().cloned().collect();
        let gate = gate.clone();
        let row = match event.kind() {
            CommitKind::PlanOnly => CommitRow::PlanOnly {
                sha,
                gate,
                feedback,
            },
            CommitKind::CodeOnly => CommitRow::CodeOnly {
                sha,
                gate,
                feedback,
            },
            CommitKind::Mixed => CommitRow::Mixed {
                sha,
                gate,
                feedback,
            },
            CommitKind::MultiPlan | CommitKind::Finalize | CommitKind::Unattributed => continue,
        };
        out.push(row);
    }
    out
}

fn build_timeline(plan: &Plan) -> Vec<TimelineEvent> {
    let mut out = Vec::with_capacity(plan.timeline.len() * 2);
    for event in &plan.timeline {
        let sha = event.sha().as_str().to_string();
        let subject = event.subject().to_string();
        let plan_touch = if event.sha() == &plan.plan_intro {
            PlanTouchKind::Intro
        } else {
            PlanTouchKind::Revision
        };
        let row = match event.kind() {
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
        if let Some(gate) = event.gate() {
            // `gate()` returns Some only for reviewable variants
            // (PlanOnly, CodeOnly, Mixed) — the invariant is now
            // structural.
            let phase = match event.kind() {
                CommitKind::PlanOnly | CommitKind::Mixed => ReviewTargetPhase::Plan,
                CommitKind::CodeOnly => ReviewTargetPhase::Impl,
                CommitKind::MultiPlan | CommitKind::Finalize | CommitKind::Unattributed => {
                    unreachable!("event.gate() is Some only for reviewable kinds")
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

/// Look up feedback entries (rich form) targeting `sha`. Reads from
/// the targeted `PlanTimelineEvent`'s gate. Used by the
/// `/api/plan/.../revision/{sha}` and `/api/plan/.../commit/{sha}`
/// route handlers.
pub fn feedback_for_target(plan: &Plan, sha: &CommitSha) -> Vec<Feedback> {
    let Some(event) = plan.event_for(sha) else {
        return Vec::new();
    };
    let Some(gate) = event.gate() else {
        return Vec::new();
    };
    gate.feedback.values().cloned().collect()
}
