//! Shared finish / rewrite preview projection. The single source of
//! truth for the typed previews that drive `trinity finish` and
//! `trinity purge` (single-plan and `--all`). HTTP handlers and the
//! operator CLI are both thin adapters over this module: they do not
//! duplicate any of the projection logic below.
//!
//! See `.trinity/plans/cli-local-state-and-status.md` for the
//! filesystem-truth direction this module supports.

use std::path::Path;

use crate::attribution::CommitChanges;
use crate::git_io::{
    self, GitIoError, commit_parent_count, diff_tree_changes, first_parent_commits_to,
    rev_parse_head, tree_plan_paths, tree_trinity_paths,
};
use crate::lifecycle::{CommitSha, PlanKey, RepoBasename};
use crate::repo_state::RepoState;
use trinity_core::api::{
    FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse, PurgeAllPreviewResponse,
    RewriteCommit, RewriteDisposition, RewritePreviewResponse, SealedApproval,
};
use trinity_core::model::PlanTimelineEvent;
use trinity_core::vocab::{CommitGateState, PlanWorktreeStatus};

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("unknown repo basename: {0}")]
    UnknownRepo(String),
    #[error("plan {0} not found")]
    PlanNotFound(String),
    #[error("plan {0} is hidden: plan file missing from working tree")]
    PlanHidden(String),
    #[error("repo {0} has no HEAD")]
    NoHead(String),
    #[error("plan intro {intro} not in first-parent walk from HEAD {head}")]
    IntroNotInWalk { intro: String, head: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git: {0}")]
    Git(#[from] GitIoError),
}

/// Single-plan finalize preview. The plan must already exist in
/// `state.plans`; callers either pass a `single_plan` slice (HTTP) or
/// a freshly-folded full state and the target `plan_key` (CLI).
pub async fn build_finish_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
) -> Result<FinishPreviewResponse, PreviewError> {
    let basename = RepoBasename::from_repo_root(repo_root)
        .ok_or_else(|| PreviewError::UnknownRepo(repo_root.display().to_string()))?;
    let plan_id_str = format!("{}/{}.md", basename.as_str(), plan_key.as_str());

    let plan = state
        .plans
        .get(plan_key)
        .ok_or_else(|| PreviewError::PlanNotFound(plan_id_str.clone()))?;

    let worktree_status = crate::responses::compute_plan_worktree_status_parts(
        &state.root,
        &plan.plan_path,
        &plan.body_hash,
    )?;

    if !plan.is_visible(worktree_status) {
        return Err(PreviewError::PlanHidden(plan_id_str));
    }

    let latest_reviewable_sha = crate::projection::latest_reviewable_commit_for(plan);
    let gate = latest_reviewable_sha
        .as_ref()
        .and_then(|sha| state.gate_for(sha));
    let gate_state = gate.map(|g| g.state).unwrap_or(CommitGateState::Unreviewed);
    let is_finished = plan.is_frozen();

    let sealed_approvals = match gate {
        Some(g) if g.state == CommitGateState::Approved => {
            let target_sha = latest_reviewable_sha
                .as_ref()
                .expect("Approved gate implies a reviewable sha");
            g.feedback
                .iter()
                .filter(|(_, fb)| fb.verdict == trinity_core::Verdict::Approve)
                .map(|(author, fb)| SealedApproval {
                    author: author.clone(),
                    source_path: crate::disk_format::feedback_path_wire(
                        &plan.id,
                        target_sha.as_str(),
                        author,
                    ),
                    body_hash: crate::lifecycle::content_hash(&fb.body),
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let readiness = compute_finalize_readiness(
        is_finished,
        latest_reviewable_sha.as_ref(),
        gate_state,
        worktree_status,
    );

    Ok(FinishPreviewResponse {
        plan_id: plan_id_str,
        plan_path: plan.plan_path.clone(),
        readiness,
        gate_state,
        latest_reviewable_sha,
        plan_worktree_status: worktree_status,
        is_finished,
        sealed_approvals,
    })
}

/// Single-plan rewrite preview. `include_finalize=false` preserves
/// the plan's `.trinity/finished/<stem>/` snapshot (finish audit
/// trail); `true` strips it (used by `purge --include-finalize`-like
/// modes).
pub async fn build_rewrite_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
    include_finalize: bool,
) -> Result<RewritePreviewResponse, PreviewError> {
    let basename = RepoBasename::from_repo_root(repo_root)
        .ok_or_else(|| PreviewError::UnknownRepo(repo_root.display().to_string()))?;
    let plan_id_str = format!("{}/{}.md", basename.as_str(), plan_key.as_str());
    let plan = state
        .plans
        .get(plan_key)
        .ok_or_else(|| PreviewError::PlanNotFound(plan_id_str.clone()))?;

    let worktree_status = crate::responses::compute_plan_worktree_status_parts(
        &state.root,
        &plan.plan_path,
        &plan.body_hash,
    )?;
    if !plan.is_visible(worktree_status) {
        return Err(PreviewError::PlanHidden(plan_id_str));
    }

    let head_sha = state
        .head
        .clone()
        .ok_or_else(|| PreviewError::NoHead(basename.as_str().to_string()))?;
    let intro_sha = Some(plan.plan_intro.clone());

    let metas = first_parent_commits_to(repo_root, &head_sha).await?;
    let start = metas
        .iter()
        .position(|m| m.sha == plan.plan_intro)
        .ok_or_else(|| PreviewError::IntroNotInWalk {
            intro: plan.plan_intro.as_str().to_string(),
            head: head_sha.as_str().to_string(),
        })?;
    let range = &metas[start..];

    // MultiPlan events touched this plan but a cross-plan commit must
    // still be flagged `foreign: true` — `--squash` refuses on it,
    // `--purge` rewrites it.
    let native_shas: std::collections::BTreeSet<CommitSha> = plan
        .timeline
        .iter()
        .filter_map(|e| match e {
            PlanTimelineEvent::PlanOnly { sha, .. }
            | PlanTimelineEvent::CodeOnly { sha, .. }
            | PlanTimelineEvent::Mixed { sha, .. }
            | PlanTimelineEvent::Finalize { sha, .. } => Some(sha.clone()),
            PlanTimelineEvent::MultiPlan { .. } => None,
        })
        .collect();

    let mut per_commit: Vec<(&git_io::CommitMeta, CommitChanges, bool)> =
        Vec::with_capacity(range.len());
    for meta in range {
        let parent_count = commit_parent_count(repo_root, &meta.sha).await?;
        let changes = diff_tree_changes(repo_root, &meta.sha).await?;
        per_commit.push((meta, changes, parent_count > 1));
    }
    let linear = !per_commit.iter().any(|(_, _, is_merge)| *is_merge);

    let mut commits = Vec::with_capacity(per_commit.len());
    for (meta, changes, _) in &per_commit {
        let strippable_in_tree =
            tree_plan_paths(repo_root, &meta.sha, plan_key.as_str(), include_finalize).await?;
        let strip_predicate_for_diff = |p: &str| -> bool {
            p == format!(".trinity/plans/{}.md", plan_key.as_str())
                || (include_finalize
                    && p.starts_with(&format!(".trinity/finished/{}/", plan_key.as_str())))
        };
        let contributes_non_strippable = changes.has_non_plan_code_changes
            || changes
                .trinity_paths_touched
                .iter()
                .any(|p| !strip_predicate_for_diff(p));
        let attributed_to_this_plan = native_shas.contains(&meta.sha);
        let (disposition, strip_paths) =
            classify_from_tree(contributes_non_strippable, &strippable_in_tree);
        commits.push(RewriteCommit {
            sha: meta.sha.clone(),
            subject: meta.subject.clone(),
            disposition,
            foreign: !attributed_to_this_plan,
            strip_paths,
        });
    }

    let head_strip_paths =
        tree_plan_paths(repo_root, &head_sha, plan_key.as_str(), include_finalize).await?;

    Ok(RewritePreviewResponse {
        plan_id: plan_id_str,
        plan_stem: plan_key.clone(),
        intro_sha,
        head_sha,
        linear,
        commits,
        head_strip_paths,
    })
}

/// All-plans rewrite preview. Walks first-parent from HEAD, slices
/// at the earliest `.trinity/` touch (the all-plans "intro"), then
/// classifies each commit in the range using tree-state.
pub async fn build_rewrite_preview_all(
    repo_root: &Path,
    include_finalize: bool,
) -> Result<PurgeAllPreviewResponse, PreviewError> {
    let basename = RepoBasename::from_repo_root(repo_root)
        .ok_or_else(|| PreviewError::UnknownRepo(repo_root.display().to_string()))?;
    let repo_basename = basename.as_str().to_string();

    let head_sha = rev_parse_head(repo_root)
        .await?
        .ok_or_else(|| PreviewError::NoHead(repo_basename.clone()))?;

    let metas = first_parent_commits_to(repo_root, &head_sha).await?;

    let mut intro_pos: Option<usize> = None;
    let mut per_commit: Vec<(git_io::CommitMeta, CommitChanges, bool)> =
        Vec::with_capacity(metas.len());
    let mut plans_seen: std::collections::BTreeSet<PlanKey> = std::collections::BTreeSet::new();
    for (idx, meta) in metas.into_iter().enumerate() {
        let parent_count = commit_parent_count(repo_root, &meta.sha).await?;
        let is_merge = parent_count > 1;
        let changes = diff_tree_changes(repo_root, &meta.sha).await?;
        for touch in &changes.plan_touches {
            plans_seen.insert(touch.session.clone());
        }
        for fc in &changes.finalize_changes {
            plans_seen.insert(fc.plan_key.clone());
        }
        if intro_pos.is_none() && changes.touched_trinity {
            intro_pos = Some(idx);
        }
        per_commit.push((meta, changes, is_merge));
    }

    // Merges BEFORE the intro are outside the rewrite range and
    // must not block linearity.
    let linear = match intro_pos {
        Some(start) => !per_commit[start..].iter().any(|(_, _, is_merge)| *is_merge),
        None => true,
    };

    let (intro_sha, commits) = match intro_pos {
        Some(start) => {
            let intro_sha = Some(per_commit[start].0.sha.clone());
            let mut commits = Vec::with_capacity(per_commit.len() - start);
            for (meta, changes, _) in &per_commit[start..] {
                let tree_trinity = tree_trinity_paths(repo_root, &meta.sha).await?;
                let strippable_in_tree: Vec<String> = tree_trinity
                    .into_iter()
                    .filter(|p| include_finalize || !p.starts_with(".trinity/finished/"))
                    .collect();
                let contributes_non_strippable = changes.has_non_plan_code_changes
                    || (!include_finalize
                        && changes
                            .trinity_paths_touched
                            .iter()
                            .any(|p| p.starts_with(".trinity/finished/")));
                let (disposition, strip_paths) =
                    classify_from_tree(contributes_non_strippable, &strippable_in_tree);
                commits.push(RewriteCommit {
                    sha: meta.sha.clone(),
                    subject: meta.subject.clone(),
                    disposition,
                    foreign: false,
                    strip_paths,
                });
            }
            (intro_sha, commits)
        }
        None => (None, Vec::new()),
    };

    let head_strip_paths: Vec<String> = tree_trinity_paths(repo_root, &head_sha)
        .await?
        .into_iter()
        .filter(|p| include_finalize || !p.starts_with(".trinity/finished/"))
        .collect();

    Ok(PurgeAllPreviewResponse {
        repo: repo_basename,
        head_sha,
        linear,
        intro_sha,
        plans_touched: plans_seen.into_iter().collect(),
        commits,
        head_strip_paths,
    })
}

/// Unified tree-based classifier. Two independent signals:
///
/// 1. `contributes_non_strippable`: does THIS commit's diff add or
///    modify any path the rewrite wants to preserve? Non-Trinity
///    code always counts; unstrippable Trinity paths added by this
///    commit count too. Inherited content from earlier commits does
///    NOT count.
///
/// 2. `strippable_in_tree`: paths under the strip-predicate that
///    exist in the commit's resulting tree (from `git ls-tree`).
///
/// Disposition matrix:
/// - `!contributes_non_strippable` → Drop.
/// - contributes + strippable in tree → Rewrite (strip those).
/// - contributes + no strippable in tree → KeepVerbatim.
pub fn classify_from_tree(
    contributes_non_strippable: bool,
    strippable_in_tree: &[String],
) -> (RewriteDisposition, Vec<String>) {
    if !contributes_non_strippable {
        return (RewriteDisposition::Drop, Vec::new());
    }
    if strippable_in_tree.is_empty() {
        (RewriteDisposition::KeepVerbatim, Vec::new())
    } else {
        (RewriteDisposition::Rewrite, strippable_in_tree.to_vec())
    }
}

/// Project the typed finalize decision. Single source of truth for
/// "can the CLI proceed?" — both HTTP and CLI dispatch on this.
pub fn compute_finalize_readiness(
    is_finished: bool,
    latest_reviewable_sha: Option<&CommitSha>,
    gate_state: CommitGateState,
    worktree_status: PlanWorktreeStatus,
) -> FinalizeReadiness {
    if is_finished {
        return FinalizeReadiness::AlreadyFinished;
    }
    let mut reasons = Vec::new();
    if latest_reviewable_sha.is_none() {
        reasons.push(FinalizeBlockReason::NoReviewableCommit);
    }
    if gate_state != CommitGateState::Approved {
        reasons.push(FinalizeBlockReason::GateNotApproved { state: gate_state });
    }
    match worktree_status {
        PlanWorktreeStatus::PlanFileMissing => {
            reasons.push(FinalizeBlockReason::PlanFileMissing);
        }
        PlanWorktreeStatus::BodyDirty => {
            reasons.push(FinalizeBlockReason::PlanFileDirty);
        }
        PlanWorktreeStatus::Clean => {}
    }
    if reasons.is_empty() {
        FinalizeReadiness::Ready
    } else {
        FinalizeReadiness::Blocked { reasons }
    }
}
