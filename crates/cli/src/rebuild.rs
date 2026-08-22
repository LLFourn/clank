//! The fold drivers. Every rebuild resumes [`RepoState`] from the
//! deepest usable checkpoint (`state_cache`) and applies the missing
//! commits via `git_io::commit_events_between`. Only the DELIBERATE
//! rebuilds (cold start, HEAD change — [`rebuild_with_diagnostics`])
//! persist spaced checkpoints (`clank_core::checkpoint` policy) and
//! prune, feeding the cache. The range/render query ([`rebuild_from`])
//! is READ-ONLY: it benefits from the cache but never writes or prunes
//! it, so a status render cannot churn the cache.
//!
//! Feedback files are NOT folded into state; consumers project them
//! on demand via `git_io::collect_feedback_files`.

use std::path::Path;

use crate::disk_snapshot::apply_commit;
use crate::git_io::{self, GitIoError};
use crate::lifecycle::CommitSha;
use crate::repo_state::RepoState;
use crate::state_cache;
use clank_core::repo_state::LogEvent;

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("git io: {0}")]
    Git(#[from] GitIoError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    Use,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildDiagnostics {
    pub cache_hit: bool,
}

pub async fn rebuild_repo(repo_root: &Path) -> Result<RepoState, RebuildError> {
    rebuild_repo_sync_with_policy(repo_root, CachePolicy::Use)
}

/// Deepest checkpoint ON `target`'s first-parent chain, at or
/// before `target`. Resolved by walking the chain from `target`
/// against the candidate set — the first chain member IS the
/// deepest usable base (and `target` itself when checkpointed).
/// Graph ancestry is deliberately not consulted: a side-branch
/// checkpoint that merged in is a graph ancestor but not a valid
/// resume point for a first-parent fold (codex 6b1c549).
fn find_base_checkpoint(
    repo_root: &Path,
    git: &git_io::Repo,
    target: &CommitSha,
) -> Option<(state_cache::CheckpointRef, RepoState)> {
    let mut checkpoints = state_cache::list_checkpoints(repo_root);
    // A corrupt hit is deleted by try_load; retry against the
    // remaining candidates rather than giving up the whole lookup.
    loop {
        let candidates: std::collections::HashSet<CommitSha> =
            checkpoints.iter().map(|c| c.sha.clone()).collect();
        let hit = match git.first_parent_chain_find(target, &candidates) {
            Ok(Some(sha)) => sha,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    repo = %repo_root.display(),
                    target = %target,
                    error = ?e,
                    "checkpoint chain lookup failed",
                );
                return None;
            }
        };
        // Several depths can share a sha only via filename games;
        // take the deepest listed.
        let pos = checkpoints.iter().position(|c| c.sha == hit)?;
        let cp = checkpoints.remove(pos);
        match state_cache::try_load(repo_root, &cp) {
            Ok(Some(state)) => return Some((cp, state)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    repo = %repo_root.display(),
                    checkpoint = %cp.sha,
                    error = ?e,
                    "checkpoint load failed; trying next candidate",
                );
            }
        }
        checkpoints.retain(|c| c.sha != cp.sha);
    }
}

/// A checkpoint a fold PROPOSES persisting: the folded [`RepoState`]
/// at `depth`. [`fold_events`] returns these; the caller writes them
/// ([`persist_checkpoints`]) or drops them.
struct PendingCheckpoint {
    depth: u64,
    state: RepoState,
}

/// What a fold produced: the log events, plus the policy-spaced
/// checkpoints it PROPOSES persisting.
struct FoldOutput {
    log_events: Vec<LogEvent>,
    checkpoints: Vec<PendingCheckpoint>,
}

/// Apply `events` to `state` oldest-first. PURE — touches no disk.
/// Returns the log events AND the policy-spaced checkpoints the fold
/// proposes (one snapshot per `should_checkpoint` depth). The CALLER
/// decides whether to persist them ([`persist_checkpoints`]) or drop
/// them, so every cache write is explicit at a call site rather than a
/// side effect hidden inside the fold.
///
/// `base_depth` is `state`'s depth before the first event; `tip_depth`
/// is the depth of the overall fold target — greater than `base_depth
/// + events.len()` when a later phase continues the same fold, so
/// spacing is computed against the real tip.
fn fold_events(
    state: &mut RepoState,
    events: Vec<crate::disk_snapshot::CommitEvent>,
    base_depth: u64,
    tip_depth: u64,
) -> FoldOutput {
    let mut log_events = Vec::new();
    let mut checkpoints = Vec::new();
    let mut depth = base_depth;
    for event in events {
        let sha = event.commit.clone();
        log_events.extend(apply_commit(state, &event));
        depth += 1;
        state.head = Some(sha);
        if clank_core::checkpoint::should_checkpoint(tip_depth.saturating_sub(depth)) {
            checkpoints.push(PendingCheckpoint {
                depth,
                state: state.clone(),
            });
        }
    }
    FoldOutput {
        log_events,
        checkpoints,
    }
}

/// Write a fold's proposed checkpoints — the explicit counterpart to
/// [`fold_events`]' pure proposal. Only deliberate rebuilds call this;
/// the render path drops the proposals so it can't touch the cache.
/// (`state_cache::write` is itself idempotent: an existing checkpoint
/// file is left in place.)
fn persist_checkpoints(repo_root: &Path, checkpoints: &[PendingCheckpoint]) {
    for cp in checkpoints {
        if let Err(e) = state_cache::write(repo_root, &cp.state, cp.depth) {
            tracing::warn!(
                repo = %repo_root.display(),
                depth = cp.depth,
                error = ?e,
                "checkpoint write failed",
            );
        }
    }
}

/// Thin checkpoints to the spacing policy relative to `tip_depth`,
/// then run the mtime backstop. The backstop is depth/ancestry-
/// blind: besides stale-branch and old-format files it also
/// collapses aged ON-CHAIN checkpoints on an idle repo (>24h
/// untouched), so a far-back resume there re-folds from root once
/// and re-seeds — accepted trade-off (ruthless 0ca555d residual);
/// scope the mtime prune to non-first-parent-ancestors if that
/// ever matters. Deletion is by depth value: in the rare case of
/// two branches checkpointed at the same depth, both go — worth
/// at most a re-fold.
fn prune_checkpoints(repo_root: &Path, tip_depth: u64) {
    let checkpoints = state_cache::list_checkpoints(repo_root);
    let depths: Vec<u64> = checkpoints.iter().map(|c| c.depth).collect();
    let deletions = clank_core::checkpoint::prune_plan(&depths, tip_depth);
    for cp in &checkpoints {
        if deletions.contains(&cp.depth) {
            state_cache::remove(repo_root, cp);
        }
    }
    if let Err(e) = state_cache::prune(repo_root) {
        tracing::warn!(
            repo = %repo_root.display(),
            error = ?e,
            "state cache prune failed",
        );
    }
}

/// Rebuild a range `(from, to]` collecting log events. `from` is
/// exclusive (`None` = repo root), `to` is inclusive. Resumes from
/// the deepest checkpoint at-or-before `from` to stay cheap.
///
/// READ-ONLY: this is the log-window render path, so it READS the
/// checkpoint cache to resume but never writes or prunes it — only a
/// deliberate [`rebuild_with_diagnostics`] mutates the cache. A render
/// that wrote the cache is what pegged the status TUI (a write the
/// trailing prune then deleted, fsync'd every frame); keeping the
/// render incapable of touching the cache makes that class of bug
/// structurally impossible.
pub async fn rebuild_from(
    repo_root: &Path,
    from: Option<&CommitSha>,
    to: &CommitSha,
) -> Result<(RepoState, Vec<LogEvent>), RebuildError> {
    // One handle for both range walks (and the checkpoint probe).
    let git = git_io::open(repo_root)?;
    let base = from.and_then(|f| find_base_checkpoint(repo_root, &git, f));
    let (mut state, base_depth) = match base {
        Some((cp, s)) => (s, cp.depth),
        None => (RepoState::empty(repo_root.to_path_buf()), 0),
    };

    // Two phases: a SILENT fold from the resume base up through `from`
    // to rebuild context (events discarded), then a COLLECTING fold of
    // `(from, to]`. fold_events proposes checkpoints; this render path
    // DROPS them — only a deliberate rebuild writes the cache.
    let silent_target = from.cloned();
    let phase1 = match silent_target.as_ref() {
        Some(target) => git.commit_events_between(state.head.as_ref(), target)?,
        None => Vec::new(),
    };
    let phase2_base = silent_target.clone().or_else(|| state.head.clone());
    let phase2 = git.commit_events_between(phase2_base.as_ref(), to)?;
    let mid_depth = base_depth + phase1.len() as u64;
    let tip_depth = mid_depth + phase2.len() as u64;

    // Phase 1 (silent): build context up to `from`; events AND proposed
    // checkpoints discarded.
    let _ = fold_events(&mut state, phase1, base_depth, tip_depth);
    if let Some(target) = silent_target {
        state.head = Some(target);
    }

    // Phase 2 (collecting): keep the log events, drop the checkpoints —
    // the render never writes or prunes the cache.
    let FoldOutput { log_events, .. } = fold_events(&mut state, phase2, mid_depth, tip_depth);
    state.head = Some(to.clone());

    Ok((state, log_events))
}

pub async fn rebuild_repo_with_policy(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<RepoState, RebuildError> {
    rebuild_repo_sync_with_policy(repo_root, policy)
}

/// Synchronous rebuild for config-mutation cores which are also called from
/// the synchronous status TUI. The fold is entirely synchronous; the async
/// entry points remain as compatibility wrappers for existing command paths.
pub fn rebuild_repo_sync_with_policy(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<RepoState, RebuildError> {
    let (state, _diag) = rebuild_with_diagnostics_sync(repo_root, policy)?;
    Ok(state)
}

pub async fn rebuild_with_diagnostics(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<(RepoState, RebuildDiagnostics), RebuildError> {
    rebuild_with_diagnostics_sync(repo_root, policy)
}

fn rebuild_with_diagnostics_sync(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<(RepoState, RebuildDiagnostics), RebuildError> {
    // One handle for the whole fold: HEAD read, checkpoint-ancestry
    // probe, and every range walk reuse it (no per-call re-open). A
    // non-repo path degrades to empty state, matching the old lenient
    // `rev_parse_head` → `None`.
    let Ok(git) = git_io::open(repo_root) else {
        return Ok((
            RepoState::empty(repo_root.to_path_buf()),
            RebuildDiagnostics { cache_hit: false },
        ));
    };
    let Some(h) = git.head_sha()? else {
        // Unborn HEAD / no commits: nothing to fold.
        return Ok((
            RepoState::empty(repo_root.to_path_buf()),
            RebuildDiagnostics { cache_hit: false },
        ));
    };

    if policy == CachePolicy::Use
        && let Some((cp, mut state)) = find_base_checkpoint(repo_root, &git, &h)
    {
        if cp.sha == h {
            return Ok((state, RebuildDiagnostics { cache_hit: true }));
        }
        // Fold-forward from the checkpoint to HEAD. We are in the
        // CachePolicy::Use branch, so persist the proposed checkpoints
        // and prune — this is the deliberate rebuild that owns the cache.
        match git.commit_events_between(Some(&cp.sha), &h) {
            Ok(events) => {
                let tip_depth = cp.depth + events.len() as u64;
                let folded = fold_events(&mut state, events, cp.depth, tip_depth);
                persist_checkpoints(repo_root, &folded.checkpoints);
                state.head = Some(h.clone());
                prune_checkpoints(repo_root, tip_depth);
                return Ok((state, RebuildDiagnostics { cache_hit: true }));
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo_root.display(),
                    from = %cp.sha,
                    to = %h,
                    error = ?e,
                    "incremental fold-forward failed; falling back to cold fold",
                );
            }
        }
    }

    // Cold fold from the repo root.
    let events = git.commit_events_between(None, &h)?;
    let tip_depth = events.len() as u64;
    let mut state = RepoState::empty(repo_root.to_path_buf());
    let folded = fold_events(&mut state, events, 0, tip_depth);
    state.head = Some(h);
    // CachePolicy::Bypass folds without touching the cache.
    if policy == CachePolicy::Use {
        persist_checkpoints(repo_root, &folded.checkpoints);
        prune_checkpoints(repo_root, tip_depth);
    }

    Ok((state, RebuildDiagnostics { cache_hit: false }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::PlanKey;
    use std::path::Path;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
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

    #[tokio::test]
    async fn empty_repo_yields_empty_state() {
        let dir = init_repo();
        let state = rebuild_repo(dir.path()).await.unwrap();
        assert!(state.head.is_none());
        assert!(state.fold.plans.is_empty());
    }

    #[tokio::test]
    async fn single_plan_intro_appears_in_fold() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(state.fold.plans.contains_key(&key));
        assert!(state.fold.plans[&key].commits[0].touched_plan);
    }

    #[tokio::test]
    async fn finalize_moves_plan_to_finished() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        // Move the plan file to finished/ to trigger Finish detection.
        write_file(dir.path(), ".clank/finished/foo.md", "# foo\n");
        run_git(dir.path(), &["rm", "--quiet", ".clank/plans/foo.md"]);
        commit(dir.path(), "[foo] finish");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(!state.fold.plans.contains_key(&key));
        assert_eq!(state.fold.finished_plans.len(), 1);
    }

    #[tokio::test]
    async fn warm_cache_skips_the_fold() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");

        let (_cold, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(!diag.cache_hit);

        let (_warm, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(diag.cache_hit);

        let (_bypass, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Bypass)
            .await
            .unwrap();
        assert!(!diag.cache_hit);
    }

    /// `(inode, mtime_s, mtime_ns, len)` of every cache file, keyed by
    /// name. The inode is the decisive churn signal: every `write`
    /// temp+rename swaps it, so a rewrite shows up even within one
    /// wall-clock second (when mtime wouldn't move).
    #[cfg(unix)]
    fn cache_fingerprint(cache: &Path) -> std::collections::BTreeMap<String, (u64, i64, i64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let mut m = std::collections::BTreeMap::new();
        for e in std::fs::read_dir(cache).into_iter().flatten().flatten() {
            let md = e.metadata().unwrap();
            m.insert(
                e.file_name().to_string_lossy().into_owned(),
                (md.ino(), md.mtime(), md.mtime_nsec(), md.len()),
            );
        }
        m
    }

    #[cfg(unix)]
    fn assert_no_churn(
        before: &std::collections::BTreeMap<String, (u64, i64, i64, u64)>,
        after: &std::collections::BTreeMap<String, (u64, i64, i64, u64)>,
    ) {
        let churned: Vec<String> = before
            .keys()
            .filter(|k| after.get(*k) != before.get(*k))
            .cloned()
            .collect();
        assert!(
            churned.is_empty(),
            "stable-head folds churned {} cache file(s): {churned:?}",
            churned.len()
        );
    }

    /// Fix 1 (render reads, fold writes): `rebuild_from` is the
    /// log-window RENDER path the status snapshot calls on EVERY frame.
    /// It must never write OR prune the checkpoint cache — only a
    /// deliberate `rebuild_with_diagnostics` mutates it. A render that
    /// wrote the cache is what pegged the status TUI: it wrote a
    /// checkpoint the trailing prune then deleted, fsync'd every frame
    /// (the watcher saw the `.clank/cache` write and re-woke). Making
    /// the render incapable of touching the cache makes that class of
    /// bug structurally impossible.
    #[cfg(unix)]
    #[tokio::test]
    async fn rebuild_from_never_mutates_cache() {
        let dir = init_repo();
        // More than the log window so `from = HEAD~30` resolves.
        for i in 0..40 {
            write_file(dir.path(), ".clank/plans/foo.md", &format!("# foo v{i}\n"));
            commit(dir.path(), &format!("[foo] step {i}"));
        }
        let head = crate::git_io::rev_parse_head(dir.path())
            .unwrap()
            .expect("head");
        let from = crate::git_io::resolve_commit(dir.path(), "HEAD~30");

        let cache = dir.path().join(".clank/cache/repo-state");
        // Cold cache: a render-path fold writes NOTHING.
        rebuild_from(dir.path(), from.as_ref(), &head)
            .await
            .unwrap();
        assert!(
            cache_fingerprint(&cache).is_empty(),
            "rebuild_from wrote a checkpoint — the render path must be read-only"
        );

        // A deliberate rebuild OWNS the cache; prime it.
        rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        let before = cache_fingerprint(&cache);
        assert!(
            !before.is_empty(),
            "the deliberate rebuild primed the cache"
        );

        // Repeated renders against the primed cache neither rewrite nor
        // prune any file.
        for _ in 0..5 {
            rebuild_from(dir.path(), from.as_ref(), &head)
                .await
                .unwrap();
        }
        let after = cache_fingerprint(&cache);
        assert_no_churn(&before, &after);
        assert_eq!(
            before.len(),
            after.len(),
            "rebuild_from added or deleted a checkpoint — it must not prune"
        );
    }

    /// The same guarantee through the FULL render path: building a
    /// `StatusSnapshot` (which runs `rebuild_repo_with_policy` AND
    /// `recent_log_rows`/`rebuild_from`) repeatedly at a stable head
    /// must not rewrite any cache file. This is the closest in-process
    /// proxy for "a status --tui render leaves the cache alone."
    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_status_snapshots_at_stable_head_do_not_churn_cache() {
        let dir = init_repo();
        for i in 0..40 {
            write_file(dir.path(), ".clank/plans/foo.md", &format!("# foo v{i}\n"));
            commit(dir.path(), &format!("[foo] step {i}"));
        }
        let cache = dir.path().join(".clank/cache/repo-state");
        let build = || {
            crate::cli::status::StatusSnapshot::build_async(
                dir.path(),
                "repo",
                None,
                CachePolicy::Use,
                None,
                false,
            )
        };
        build().await.unwrap();
        let before = cache_fingerprint(&cache);
        assert!(!before.is_empty(), "first snapshot primed the cache");

        for _ in 0..5 {
            build().await.unwrap();
        }
        assert_no_churn(&before, &cache_fingerprint(&cache));
    }

    #[tokio::test]
    async fn ancestor_cache_fold_forwards() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        let _ = rebuild_repo(dir.path()).await.unwrap();
        write_file(dir.path(), "src.rs", "fn main() {}\n");
        commit(dir.path(), "[foo] implement");
        let (state, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(diag.cache_hit);
        let key = PlanKey::parse("foo").unwrap();
        assert_eq!(state.fold.plans[&key].commits.len(), 2);
    }

    /// `n` code commits on top of whatever exists.
    fn many_commits(repo: &Path, n: usize) {
        for i in 0..n {
            write_file(repo, "src/churn.rs", &format!("// rev {i}\n"));
            commit(repo, &format!("code: churn {i}"));
        }
    }

    /// First-parent shas oldest-first (index + 1 == depth).
    fn shas_by_depth(repo: &Path) -> Vec<CommitSha> {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--first-parent", "--reverse", "HEAD"])
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| CommitSha::parse(l.trim()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn cold_fold_writes_spaced_checkpoints_and_any_point_resumes_nearby() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        many_commits(dir.path(), 39);

        let _ = rebuild_repo(dir.path()).await.unwrap();

        let checkpoints = state_cache::list_checkpoints(dir.path());
        let depths: Vec<u64> = checkpoints.iter().map(|c| c.depth).collect();
        assert_eq!(depths.first(), Some(&40), "tip checkpointed");
        assert!(
            checkpoints.len() <= 14,
            "O(log n) checkpoints for 40 commits: {depths:?}"
        );

        // The property the TUI's range fold depends on: a recent
        // point finds a base within roughly its own distance from
        // the tip — never the repo root. (Targets below the
        // shallowest checkpoint fall back to a root fold bounded
        // by their own depth, and that fold self-heals the gap —
        // covered by rebuild_from_resumes_from_checkpoints…)
        let shas = shas_by_depth(dir.path());
        for k in [1u64, 3, 5, 10] {
            let target_depth = 40 - k;
            let target = &shas[(target_depth - 1) as usize];
            let git = git_io::open(dir.path()).unwrap();
            let (cp, _) = find_base_checkpoint(dir.path(), &git, target)
                .unwrap_or_else(|| panic!("base for HEAD~{k}"));
            let gap = target_depth - cp.depth;
            assert!(
                gap <= k.max(2),
                "HEAD~{k}: base at depth {} leaves gap {gap}",
                cp.depth
            );
        }
    }

    #[tokio::test]
    async fn checkpoint_contents_equal_a_cold_fold_to_that_sha() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        many_commits(dir.path(), 20);
        let _ = rebuild_repo(dir.path()).await.unwrap();

        // Pick a mid-history checkpoint and verify its payload
        // matches folding from the root to that sha with no cache.
        let cp = state_cache::list_checkpoints(dir.path())
            .into_iter()
            .find(|c| c.depth < 21)
            .expect("a mid-history checkpoint");
        let loaded = state_cache::try_load(dir.path(), &cp).unwrap().unwrap();
        let (cold, _) = rebuild_from(dir.path(), None, &cp.sha).await.unwrap();
        assert_eq!(loaded.fold, cold.fold, "checkpoint at depth {}", cp.depth);
    }

    #[tokio::test]
    async fn rebuild_from_resumes_from_checkpoints_read_only() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        many_commits(dir.path(), 29);
        let shas = shas_by_depth(dir.path());
        let from = shas[26].clone(); // HEAD~3
        let head = shas[29].clone();

        // Render-path fold on an empty cache: correct events, but it
        // seeds NO checkpoints (Fix 1 — only a deliberate rebuild owns
        // the cache).
        let (state_cold, events_cold) = rebuild_from(dir.path(), Some(&from), &head).await.unwrap();
        assert_eq!(events_cold.len(), 3);
        assert!(
            state_cache::list_checkpoints(dir.path()).is_empty(),
            "the render path must not seed checkpoints"
        );

        // A deliberate rebuild seeds the cache; the render then RESUMES
        // from a nearby checkpoint…
        rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        let git = git_io::open(dir.path()).unwrap();
        let (cp, _) = find_base_checkpoint(dir.path(), &git, &from).expect("base exists now");
        assert!(cp.depth >= 24, "dense-near-tip base, got {}", cp.depth);
        // …and produces the identical result it folded cold.
        let (state_warm, events_warm) = rebuild_from(dir.path(), Some(&from), &head).await.unwrap();
        assert_eq!(state_cold.fold, state_warm.fold);
        assert_eq!(events_cold, events_warm);
    }

    #[tokio::test]
    async fn side_branch_checkpoint_is_not_a_resume_base() {
        // codex 6b1c549: a checkpoint written on a side branch is a
        // GRAPH ancestor of main after a no-ff merge, but not a
        // first-parent ancestor. Resuming from it would fold the
        // side branch's .clank changes twice (once in the loaded
        // state, again via the merge commit's diff).
        let dir = init_repo();
        let repo = dir.path();
        write_file(repo, ".clank/plans/foo.md", "# foo\n");
        commit(repo, "[foo] intro");

        // Side branch revises the plan; checkpoint written THERE
        // (the only checkpoints in existence).
        run_git(repo, &["checkout", "--quiet", "-b", "side"]);
        write_file(repo, ".clank/plans/foo.md", "# foo v2\n");
        commit(repo, "[foo] side revision");
        let _ = rebuild_repo(repo).await.unwrap();
        let side_tip = state_cache::list_checkpoints(repo)
            .first()
            .unwrap()
            .sha
            .clone();

        // Merge into main as a non-first-parent.
        run_git(repo, &["checkout", "--quiet", "main"]);
        run_git(
            repo,
            &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
        );
        let head = git_io::rev_parse_head(repo).unwrap().unwrap();

        // The side checkpoint must not be offered as a base…
        assert!(
            git_io::is_ancestor(repo, &side_tip, &head).unwrap(),
            "precondition: side tip IS a graph ancestor (the trap)"
        );
        let git = git_io::open(repo).unwrap();
        assert!(
            find_base_checkpoint(repo, &git, &head).is_none()
                || find_base_checkpoint(repo, &git, &head).unwrap().0.sha != side_tip,
            "side-branch checkpoint offered as resume base"
        );

        // …and the cached rebuild must equal a cache-blind fold:
        // exactly one revision entry for foo (the merge), no
        // duplicate from the side sha.
        let (cached, _) = rebuild_with_diagnostics(repo, CachePolicy::Use)
            .await
            .unwrap();
        let (blind, _) = rebuild_with_diagnostics(repo, CachePolicy::Bypass)
            .await
            .unwrap();
        assert_eq!(
            cached.fold, blind.fold,
            "side checkpoint corrupted the fold"
        );
        let key = PlanKey::parse("foo").unwrap();
        assert_eq!(
            cached.fold.plans[&key].commits.len(),
            2,
            "intro + merge revision only — no duplicated side commit"
        );
    }

    #[tokio::test]
    async fn tip_advance_rebalances_checkpoint_density() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        many_commits(dir.path(), 29);
        let _ = rebuild_repo(dir.path()).await.unwrap();

        many_commits(dir.path(), 30);
        let _ = rebuild_repo(dir.path()).await.unwrap();

        let depths: Vec<u64> = state_cache::list_checkpoints(dir.path())
            .iter()
            .map(|c| c.depth)
            .collect();
        assert_eq!(depths.first(), Some(&60), "new tip checkpointed");
        assert!(
            depths.len() <= 16,
            "old dense cluster thinned, not accumulated: {depths:?}"
        );
        // The once-dense cluster behind depth 30 collapses to
        // one-per-bucket relative to the new tip.
        let old_cluster = depths.iter().filter(|&&d| (25..=30).contains(&d)).count();
        assert!(old_cluster <= 2, "stale density remains: {depths:?}");
    }

    /// The fitness function for thread-gix-repo-handle: a fold opens the
    /// ODB EXACTLY once (HEAD read + checkpoint probe + range walks all
    /// reuse one handle). An exact count, not a loose bound — a stray
    /// re-open fails loudly. `#[tokio::test]` is current-thread, so the
    /// fold's `git_io::open` calls land on this test's thread, where the
    /// thread-local counter observes them.
    #[tokio::test]
    async fn fold_opens_the_odb_once() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        many_commits(dir.path(), 8);

        // Cold fold (cache bypassed): HEAD read + cold range walk, one open.
        git_io::OPEN_COUNT.with(|c| c.set(0));
        let _ = rebuild_with_diagnostics(dir.path(), CachePolicy::Bypass)
            .await
            .unwrap();
        assert_eq!(
            git_io::OPEN_COUNT.with(|c| c.get()),
            1,
            "cold fold must open the ODB exactly once"
        );

        // Prime the cache, add commits, then a warm fold-forward: HEAD
        // read + checkpoint-ancestry probe + fold-forward walk, still one.
        let _ = rebuild_repo(dir.path()).await.unwrap();
        many_commits(dir.path(), 4);
        git_io::OPEN_COUNT.with(|c| c.set(0));
        let _ = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert_eq!(
            git_io::OPEN_COUNT.with(|c| c.get()),
            1,
            "warm fold-forward must open the ODB exactly once"
        );
    }
}
