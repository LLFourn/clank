//! Local preview projection for `trinity finish` / `trinity purge`.
//! Operates against a freshly-folded [`RepoState`] (the new sans-io
//! fold) plus git_io + on-disk feedback files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::disk_format::{FeedbackTarget, parse_verdict};
use crate::git_io::{
    self, GitIoError, commit_parent_count, diff_tree_changes, first_parent_commits_to,
    rev_parse_head, tree_plan_paths, tree_trinity_paths,
};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, RepoBasename, content_hash};
use crate::repo_state::RepoState;
use trinity_core::api::{
    FinalizeBlockReason, FinalizeReadiness, FinishPreviewResponse, PurgeAllPreviewResponse,
    RewriteCommit, RewriteDisposition, RewritePreviewResponse, SealedApproval,
};
use trinity_core::vocab::{CommitGateState, PlanWorktreeStatus, Verdict};

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

/// Single-plan finalize preview. The plan must be active in
/// `state.fold.plans`.
pub async fn build_finish_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
) -> Result<FinishPreviewResponse, PreviewError> {
    let basename = RepoBasename::from_repo_root(repo_root)
        .ok_or_else(|| PreviewError::UnknownRepo(repo_root.display().to_string()))?;
    let plan_id_str = format!("{}/{}.md", basename.as_str(), plan_key.as_str());
    let plan_path = format!(".trinity/plans/{}.md", plan_key.as_str());

    let is_finished = state
        .fold
        .finished_plans
        .iter()
        .any(|f| &f.plan == plan_key);
    let active = state.fold.plans.get(plan_key);
    if !is_finished && active.is_none() {
        return Err(PreviewError::PlanNotFound(plan_id_str));
    }

    let worktree_status = worktree_status(repo_root, &plan_path).await?;

    // Active plans need their file in the worktree to be eligible
    // for finalize. Finished plans are exempt.
    if !is_finished && matches!(worktree_status, PlanWorktreeStatus::PlanFileMissing) {
        return Err(PreviewError::PlanHidden(plan_id_str));
    }

    let latest_reviewable_sha = active.and_then(latest_reviewable_sha);

    let (gate_state, sealed_approvals_candidates) =
        compute_gate(repo_root, state, plan_key, latest_reviewable_sha.as_ref()).await?;

    let readiness = compute_finalize_readiness(
        is_finished,
        latest_reviewable_sha.as_ref(),
        gate_state,
        worktree_status,
    );

    let sealed_approvals = if matches!(readiness, FinalizeReadiness::Ready) {
        sealed_approvals_candidates
    } else {
        Vec::new()
    };

    Ok(FinishPreviewResponse {
        plan_id: plan_id_str,
        plan_path,
        readiness,
        gate_state,
        latest_reviewable_sha,
        plan_worktree_status: worktree_status,
        is_finished,
        sealed_approvals,
    })
}

/// Single-plan rewrite preview. `include_finalize=true` strips the
/// `.trinity/finished/<stem>/` snapshot too (purge mode).
pub async fn build_rewrite_preview(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
    include_finalize: bool,
) -> Result<RewritePreviewResponse, PreviewError> {
    let basename = RepoBasename::from_repo_root(repo_root)
        .ok_or_else(|| PreviewError::UnknownRepo(repo_root.display().to_string()))?;
    let plan_id_str = format!("{}/{}.md", basename.as_str(), plan_key.as_str());

    // Active or finished plan? For finished plans, we also need the
    // finalize SHA so native_shas can be computed by replaying the
    // [intro, finalized_at] interval through the sans-io fold (the
    // fold itself drops finished plans' per-commit timeline).
    let (intro_sha, finalized_at) = if let Some(ps) = state.fold.plans.get(plan_key) {
        (ps.commits.first().map(|e| e.sha.clone()), None)
    } else if let Some(fp) = state
        .fold
        .finished_plans
        .iter()
        .find(|f| &f.plan == plan_key)
    {
        (Some(fp.intro.clone()), Some(fp.finalized_at.clone()))
    } else {
        return Err(PreviewError::PlanNotFound(plan_id_str));
    };

    let head_sha = state
        .head
        .clone()
        .ok_or_else(|| PreviewError::NoHead(basename.as_str().to_string()))?;

    let metas = first_parent_commits_to(repo_root, &head_sha).await?;
    let start = match intro_sha.as_ref() {
        Some(intro) => metas.iter().position(|m| &m.sha == intro).ok_or_else(|| {
            PreviewError::IntroNotInWalk {
                intro: intro.as_str().to_string(),
                head: head_sha.as_str().to_string(),
            }
        })?,
        None => {
            return Ok(RewritePreviewResponse {
                plan_id: plan_id_str,
                plan_stem: plan_key.clone(),
                intro_sha: None,
                head_sha,
                linear: true,
                commits: Vec::new(),
                head_strip_paths: Vec::new(),
            });
        }
    };
    let range = &metas[start..];

    // Plan-attributed SHAs. For active plans, read straight from the
    // fold's per-plan timeline. For finished plans, re-derive by
    // folding the [intro, finalized_at] interval — `RepoState`
    // doesn't retain finished-plan timelines (Acceptance: finished
    // plans keep only `{ plan, intro, finalized_at }`).
    let native_shas: BTreeSet<CommitSha> = if let Some(ps) = state.fold.plans.get(plan_key) {
        ps.commits.iter().map(|e| e.sha.clone()).collect()
    } else if let Some(end) = finalized_at.as_ref() {
        re_fold_finished_plan_natives(repo_root, plan_key, end).await?
    } else {
        BTreeSet::new()
    };

    let mut per_commit = Vec::with_capacity(range.len());
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
        let attributed = native_shas.contains(&meta.sha);
        let (disposition, strip_paths) =
            classify_from_tree(contributes_non_strippable, &strippable_in_tree);
        commits.push(RewriteCommit {
            sha: meta.sha.clone(),
            subject: meta.subject.clone(),
            disposition,
            foreign: !attributed,
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

/// All-plans rewrite preview. Walks first-parent from HEAD, finds
/// the earliest `.trinity/` touch, classifies the range.
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
    let mut per_commit = Vec::with_capacity(metas.len());
    let mut plans_seen: BTreeSet<PlanKey> = BTreeSet::new();
    for (idx, meta) in metas.into_iter().enumerate() {
        let parent_count = commit_parent_count(repo_root, &meta.sha).await?;
        let is_merge = parent_count > 1;
        let changes = diff_tree_changes(repo_root, &meta.sha).await?;
        for touch in &changes.plan_touches {
            plans_seen.insert(touch.plan.clone());
        }
        for fc in &changes.finalize_changes {
            plans_seen.insert(fc.plan_key.clone());
        }
        if intro_pos.is_none() && changes.touched_trinity {
            intro_pos = Some(idx);
        }
        per_commit.push((meta, changes, is_merge));
    }

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

/// Re-derive the per-plan native-SHA set for a FINISHED plan.
///
/// The canonical `RepoState` only stores `{ plan, intro,
/// finalized_at }` for finished plans (no timeline). Reconstructing
/// the timeline requires the SAME repo-scoped classifier context
/// the original fold saw — `known_plans` and `active_plan_hint` at
/// each commit can be established by OTHER plans the classifier
/// already knew before this plan's intro (e.g. a `[other,this]`
/// prefix needs `other` in `known_plans` for the attribution to
/// hold). So we cold-fold the first-parent chain from ROOT up to
/// but excluding `finalized_at`. At that boundary the plan is
/// still active in `state.fold.plans` with its current cycle's
/// timeline (prior cycles, if any, were already dropped by their
/// own finalize). The finalize SHA itself is added back at the end.
///
/// Phase-2 cache loading is opportunistic: try the closest cached
/// ancestor of `finalized_at` and fold-forward from there; fall
/// back to a cold walk from root on miss.
async fn re_fold_finished_plan_natives(
    repo_root: &Path,
    plan_key: &PlanKey,
    finalized_at: &CommitSha,
) -> Result<BTreeSet<CommitSha>, PreviewError> {
    use crate::disk_snapshot::{CommitEvent, apply_commit, enrich_with_newly_finished};

    let metas = first_parent_commits_to(repo_root, finalized_at).await?;
    let final_idx = metas
        .iter()
        .position(|m| &m.sha == finalized_at)
        .ok_or_else(|| PreviewError::IntroNotInWalk {
            intro: finalized_at.as_str().to_string(),
            head: finalized_at.as_str().to_string(),
        })?;

    let (mut scratch, start_idx) = pick_cache_anchor(repo_root, &metas, final_idx).await?;

    for meta in &metas[start_idx..final_idx] {
        let changes = diff_tree_changes(repo_root, &meta.sha).await?;
        let raw = CommitEvent {
            commit: meta.sha.clone(),
            author_ts: meta.author_ts,
            subject: meta.subject.clone(),
            changes,
            newly_finished: BTreeSet::new(),
        };
        let enriched = enrich_with_newly_finished(repo_root, &raw).await?;
        apply_commit(&mut scratch, &enriched);
    }
    let mut native: BTreeSet<CommitSha> = scratch
        .fold
        .plans
        .get(plan_key)
        .map(|ps| ps.commits.iter().map(|e| e.sha.clone()).collect())
        .unwrap_or_default();
    native.insert(finalized_at.clone());
    Ok(native)
}

/// Most-recent cached `RepoState` whose head is an ancestor of
/// `finalized_at` and falls before it in the walk. Returns
/// `(scratch_state, start_idx)` — the index in `metas` from which
/// to fold forward. Falls back to `(RepoState::empty, 0)` on miss.
async fn pick_cache_anchor(
    repo_root: &Path,
    metas: &[crate::git_io::CommitMeta],
    final_idx: usize,
) -> Result<(RepoState, usize), PreviewError> {
    let ancestor_positions: std::collections::BTreeMap<CommitSha, usize> = metas[..final_idx]
        .iter()
        .enumerate()
        .map(|(i, m)| (m.sha.clone(), i))
        .collect();
    for cached in crate::state_cache::list_cached_heads(repo_root) {
        let Some(&idx) = ancestor_positions.get(&cached) else {
            continue;
        };
        if let Ok(Some(state)) = crate::state_cache::try_load(repo_root, &cached) {
            return Ok((state, idx + 1));
        }
    }
    Ok((RepoState::empty(repo_root.to_path_buf()), 0))
}

/// Tree-based classifier shared with the rewrite engine.
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

/// Compute finalize readiness from the four observable signals.
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

/// The plan's latest reviewable commit SHA (last touched_plan ||
/// touched_code event). `None` when the plan has no commits yet.
fn latest_reviewable_sha(ps: &trinity_core::repo_state::PlanState) -> Option<CommitSha> {
    ps.commits
        .iter()
        .rev()
        .find(|e| e.touched_plan || e.touched_code)
        .map(|e| e.sha.clone())
}

/// Worktree status: compare the file's worktree content (if any) to
/// HEAD's blob via `git show HEAD:.trinity/plans/<plan>.md`.
async fn worktree_status(
    repo_root: &Path,
    plan_path: &str,
) -> Result<PlanWorktreeStatus, PreviewError> {
    let abs = repo_root.join(plan_path);
    let worktree_body = match std::fs::read_to_string(&abs) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let head = git_io::rev_parse_head(repo_root).await?;
    let head_body = match head {
        Some(ref h) => match git_io::show_blob(repo_root, h, Path::new(plan_path)).await {
            Ok(b) => Some(b),
            Err(_) => None,
        },
        None => None,
    };
    Ok(match (head_body.as_deref(), worktree_body.as_deref()) {
        (Some(h), Some(w)) if h == w => PlanWorktreeStatus::Clean,
        (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
        (Some(_), None) => PlanWorktreeStatus::PlanFileMissing,
        (None, _) => PlanWorktreeStatus::Clean,
    })
}

/// Build the per-plan gate from feedback files. Walks the plan's
/// reviewable timeline accumulating participants, then evaluates the
/// target SHA's verdicts.
async fn compute_gate(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
    target_sha: Option<&CommitSha>,
) -> Result<(CommitGateState, Vec<SealedApproval>), PreviewError> {
    let Some(target) = target_sha else {
        return Ok((CommitGateState::Unreviewed, Vec::new()));
    };
    let Some(ps) = state.fold.plans.get(plan_key) else {
        return Ok((CommitGateState::Unreviewed, Vec::new()));
    };

    // Cumulative participants across all reviewable commits up to
    // and including the target.
    let mut participants: Vec<AgentLabel> = Vec::new();
    let mut target_feedback: BTreeMap<AgentLabel, (Verdict, String, std::path::PathBuf)> =
        BTreeMap::new();

    for ev in &ps.commits {
        if !(ev.touched_plan || ev.touched_code) {
            continue;
        }
        let dir = repo_root
            .join(".trinity/feedback")
            .join(plan_key.as_str())
            .join(ev.sha.as_str());
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if &ev.sha == target {
                    break;
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(author) = AgentLabel::parse(stem) else {
                continue;
            };
            let body = match std::fs::read_to_string(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let verdict = parse_verdict(&body);
            if !participants.contains(&author) {
                participants.push(author.clone());
            }
            if &ev.sha == target {
                target_feedback.insert(author, (verdict, body, path));
            }
        }
        if &ev.sha == target {
            break;
        }
    }

    // Classify per-participant against the target SHA's verdicts.
    let mut approvers: Vec<AgentLabel> = Vec::new();
    let mut requesters: Vec<AgentLabel> = Vec::new();
    let mut ambiguous: Vec<AgentLabel> = Vec::new();
    let mut missing: Vec<AgentLabel> = Vec::new();
    for p in &participants {
        match target_feedback.get(p) {
            Some((Verdict::Approve, _, _)) => approvers.push(p.clone()),
            Some((Verdict::RequestChanges, _, _)) => requesters.push(p.clone()),
            Some((Verdict::Unmarked, _, _)) => ambiguous.push(p.clone()),
            None => missing.push(p.clone()),
        }
    }

    let state_enum = if !requesters.is_empty() || !ambiguous.is_empty() {
        CommitGateState::ChangesRequested
    } else if !approvers.is_empty() && missing.is_empty() {
        CommitGateState::Approved
    } else {
        CommitGateState::Unreviewed
    };

    let sealed_approvals = if matches!(state_enum, CommitGateState::Approved) {
        target_feedback
            .iter()
            .filter(|(_, (v, _, _))| matches!(v, Verdict::Approve))
            .filter_map(|(author, (_, body, path))| {
                let rel = path
                    .strip_prefix(repo_root)
                    .ok()?
                    .to_string_lossy()
                    .to_string();
                Some(SealedApproval {
                    author: author.clone(),
                    source_path: rel,
                    body_hash: content_hash(body),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok((state_enum, sealed_approvals))
}

// Drop unused — keep the FeedbackTarget import live so the parser
// stays in the module graph.
#[allow(dead_code)]
fn _feedback_target_marker(_: FeedbackTarget) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::{CachePolicy, rebuild_repo_with_policy};
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
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
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

    /// Regression for codex on 31df65e: finished-plan rewrite
    /// preview must reflect the historical classifier context of
    /// the repo, not an isolated target-plan scratch fold. If plan
    /// A is active when plan B is introduced and a commit titled
    /// `[a,b] shared` lands while both are active, B's native_shas
    /// must include the shared commit — the cold-from-root re-fold
    /// keeps A in known_plans at the shared commit so the prefix
    /// resolves and B's per-cycle timeline picks the commit up.
    #[tokio::test]
    async fn finished_plan_rewrite_preview_uses_repo_history() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/a.md", "# a\n");
        commit(dir.path(), "[a] intro");
        write_file(dir.path(), ".trinity/plans/b.md", "# b\n");
        commit(dir.path(), "[b] intro");
        write_file(dir.path(), "src.rs", "fn main() {}\n");
        write_file(dir.path(), ".trinity/plans/a.md", "# a v2\n");
        write_file(dir.path(), ".trinity/plans/b.md", "# b v2\n");
        commit(dir.path(), "[a,b] shared work");
        // Approve b and finalize.
        write_file(
            dir.path(),
            ".trinity/finished/b/alice.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize b");

        let state = rebuild_repo_with_policy(dir.path(), CachePolicy::Bypass)
            .await
            .unwrap();
        let key = PlanKey::parse("b").unwrap();
        let preview = build_rewrite_preview(dir.path(), &state, &key, true)
            .await
            .unwrap();

        // The `[a,b] shared work` commit must NOT be foreign to b.
        let shared = preview
            .commits
            .iter()
            .find(|c| c.subject == "[a,b] shared work")
            .expect("shared commit in rewrite range");
        assert!(
            !shared.foreign,
            "shared `[a,b]` commit must be attributed to b too (historical classifier context)",
        );
    }
}
