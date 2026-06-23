//! Git introspection for the filesystem-truth model. Narrowly-scoped
//! reads only — never writes. Functions here use `gix` (gitoxide)
//! programmatically; the subprocess + text-parse era is gone.

use std::path::{Path, PathBuf};

use crate::disk_format::parse_feedback_path;
use crate::disk_snapshot::{CommitChanges, CommitEvent, FeedbackBlob, PlanTouch, PlanTouchKind};
use crate::lifecycle::{CommitSha, PlanKey};

#[derive(Debug, thiserror::Error)]
pub enum GitIoError {
    #[error("git {context}: exit {code:?}: {stderr}")]
    NonZero {
        context: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("git output parse failure ({context}): {detail}")]
    Parse { context: String, detail: String },
}

fn parse_sha(context: &str, s: &str) -> Result<CommitSha, GitIoError> {
    CommitSha::parse(s).map_err(|e| GitIoError::Parse {
        context: context.to_string(),
        detail: e.to_string(),
    })
}

fn nonzero(context: impl Into<String>, e: impl std::fmt::Display) -> GitIoError {
    GitIoError::NonZero {
        context: context.into(),
        code: None,
        stderr: e.to_string(),
    }
}

/// An opened repository handle. Wraps `gix` so callers can reuse one
/// open ODB across several reads WITHOUT naming `gix` themselves —
/// this module is the git-access boundary, and the handle is how a fold
/// / status build / log render opens the ODB ONCE and threads it through
/// every read, instead of re-opening (and re-reading pack indexes) per
/// call.
pub struct Repo(gix::Repository);

// Counts `open` calls, for the open-once fitness tests. Thread-local so
// parallel tests don't interfere (each `#[tokio::test]` is current-thread,
// so a fold's `open`s land on the test's thread).
#[cfg(test)]
thread_local! {
    pub static OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Open `repo` for reuse across reads. See [`Repo`]. Sets a warm object
/// cache: the fold walk + gated diffs revisit parent trees/commits, so
/// caching makes those re-reads near-free across the handle's lifetime.
pub fn open(repo: &Path) -> Result<Repo, GitIoError> {
    #[cfg(test)]
    OPEN_COUNT.with(|c| c.set(c.get() + 1));
    let mut r =
        gix::open(repo).map_err(|e| nonzero(format!("gix open `{}`", repo.display()), e))?;
    r.object_cache_size_if_unset(16 * 1024 * 1024);
    Ok(Repo(r))
}

impl Repo {
    /// HEAD's commit sha (`None` on unborn HEAD) — the [`rev_parse_head`]
    /// read for callers that already hold a handle.
    pub fn head_sha(&self) -> Result<Option<CommitSha>, GitIoError> {
        match self
            .0
            .head()
            .ok()
            .and_then(|head| head.id())
            .map(|id| id.detach())
        {
            None => Ok(None),
            Some(oid) => Ok(Some(parse_sha("head_id", &oid.to_string())?)),
        }
    }
}

/// Worktree dirt summary: +/− line counts vs HEAD (staged and
/// unstaged together) and the untracked-file count. Untracked lines
/// are NOT folded into the +/− numbers — a diff against HEAD doesn't
/// see them, and pretending otherwise lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyStats {
    pub insertions: u64,
    pub deletions: u64,
    pub untracked: u64,
}

/// `None` = clean. Opens its own handle; prefer [`working_tree_dirty`]
/// when you already hold a [`Repo`].
pub fn working_tree_dirty_at(repo: &Path) -> Result<Option<DirtyStats>, GitIoError> {
    open(repo)?.working_tree_dirty()
}

/// `true` iff the working tree + index are clean — no tracked changes
/// and no untracked files (the `git status --porcelain` empty test the
/// `clank unfinish` / `shelve` / `open` / `purge` guards used).
pub fn working_tree_clean(repo: &Path) -> Result<bool, GitIoError> {
    Ok(working_tree_status(repo)?.is_clean())
}

/// The structured working-tree status, so callers can apply their own
/// policy (e.g. `rewrite` ignores untracked `.clank/` scratch). Replaces
/// parsing `git status --porcelain` lines. `changed` is the tracked
/// paths differing from HEAD (staged + unstaged), sorted; `untracked`
/// is the untracked file paths.
pub struct WorkingTreeStatus {
    pub changed: Vec<String>,
    pub untracked: Vec<String>,
}

impl WorkingTreeStatus {
    pub fn is_clean(&self) -> bool {
        self.changed.is_empty() && self.untracked.is_empty()
    }
}

/// Structured `git status` via gix. Opens its own handle.
pub fn working_tree_status(repo: &Path) -> Result<WorkingTreeStatus, GitIoError> {
    let h = open(repo)?;
    let w = status_walk(&h.0)?;
    Ok(WorkingTreeStatus {
        changed: w.changed.into_iter().collect(),
        untracked: w.untracked,
    })
}

/// `None` = clean. Computed in-process via gix — no `git` subprocess.
/// One handle drives a single working-tree status walk plus the
/// per-path line diffs (no per-read ODB re-open).
///
/// We never write the index back (gix only does so on an explicit
/// `Outcome::write_changes()`), so this can't trigger the self-wake
/// the old `--no-optional-locks` shell-out guarded against: a probe
/// that refreshed `.git/index` would fire the very watcher that drove
/// the probe.
impl Repo {
    pub fn working_tree_dirty(&self) -> Result<Option<DirtyStats>, GitIoError> {
        let git = &self.0;

        let workdir = git
            .workdir()
            .ok_or_else(|| nonzero("working_tree_dirty", "bare repo has no working tree"))?
            .to_path_buf();

        let walk = status_walk(git)?;
        if walk.changed.is_empty() && walk.untracked.is_empty() {
            return Ok(None);
        }

        // Line counts mirror `git diff HEAD --shortstat` (display-only; not
        // a gate input). On an unborn HEAD there's no tree to diff against —
        // degrade to 0/0, matching the old shell-out where `git diff HEAD`
        // errored (the untracked count still tells the story). gix's diff
        // algorithm may drift by a line from git's on some changes; that's
        // accepted for a display figure.
        let (insertions, deletions) = match git.head_commit().ok().and_then(|c| c.tree().ok()) {
            Some(head_tree) => count_dirty_lines(git, &head_tree, &workdir, &walk.changed),
            None => (0, 0),
        };

        Ok(Some(DirtyStats {
            insertions,
            deletions,
            untracked: walk.untracked.len() as u64,
        }))
    }
}

/// Every dirty path — tracked changes plus untracked files — sorted.
/// Opens its own handle. For guards that show WHAT is dirty (e.g.
/// `clank unfinish`'s clean-worktree check).
pub fn working_tree_dirty_paths(repo: &Path) -> Result<Vec<String>, GitIoError> {
    let s = working_tree_status(repo)?;
    let mut paths = s.changed;
    paths.extend(s.untracked);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The raw working-tree status: tracked paths differing from HEAD
/// (staged + unstaged together) and untracked file paths.
struct WtStatus {
    changed: std::collections::BTreeSet<String>,
    untracked: Vec<String>,
}

/// One status walk yields HEAD↔index (staged) and index↔worktree
/// (unstaged + untracked) changes. `git diff HEAD` is HEAD vs the
/// files on disk, so callers diff each changed TRACKED path's HEAD
/// blob against its worktree content (staged + unstaged together).
/// Untracked paths are kept separate — `git diff HEAD` ignores them.
fn status_walk(git: &gix::Repository) -> Result<WtStatus, GitIoError> {
    use gix::bstr::ByteSlice;
    let mut changed: std::collections::BTreeSet<String> = Default::default();
    let mut untracked: Vec<String> = Vec::new();

    let patterns: Vec<gix::bstr::BString> = Vec::new();
    let iter = git
        .status(gix::progress::Discard)
        .map_err(|e| nonzero("git status", e))?
        .into_iter(patterns)
        .map_err(|e| nonzero("git status iter", e))?;
    for item in iter {
        match item.map_err(|e| nonzero("git status item", e))? {
            gix::status::Item::TreeIndex(change) => {
                changed.insert(change.location().to_str_lossy().into_owned());
            }
            gix::status::Item::IndexWorktree(iw) => {
                use gix::status::index_worktree::iter::Summary;
                match iw.summary() {
                    // NeedsUpdate (stat-only) / ignored — not a real change.
                    None => {}
                    // The dirwalk only surfaces untracked files as `Added`.
                    Some(Summary::Added) => {
                        untracked.push(iw.rela_path().to_str_lossy().into_owned())
                    }
                    Some(_) => {
                        changed.insert(iw.rela_path().to_str_lossy().into_owned());
                    }
                }
            }
        }
    }
    Ok(WtStatus { changed, untracked })
}

/// Sum inserted/deleted lines across `changed` paths, diffing each
/// path's HEAD blob against its current worktree content (the
/// `git diff HEAD` shape). Renames aren't tracked — best-effort, as
/// the dirty line is display-only.
fn count_dirty_lines(
    git: &gix::Repository,
    head_tree: &gix::Tree<'_>,
    workdir: &Path,
    changed: &std::collections::BTreeSet<String>,
) -> (u64, u64) {
    use gix::diff::blob::{Algorithm, InternedInput, diff_with_slider_heuristics};

    let (mut insertions, mut deletions) = (0u64, 0u64);
    for rel in changed {
        let old: Vec<u8> = head_tree
            .lookup_entry_by_path(rel)
            .ok()
            .flatten()
            .and_then(|e| git.find_blob(e.oid()).ok())
            .map(|b| b.data.clone())
            .unwrap_or_default();
        let new: Vec<u8> = std::fs::read(workdir.join(rel)).unwrap_or_default();
        if old == new {
            continue;
        }
        let input = InternedInput::new(old.as_slice(), new.as_slice());
        let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
        insertions += u64::from(diff.count_additions());
        deletions += u64::from(diff.count_removals());
    }
    (insertions, deletions)
}

/// Is `rel_path`'s worktree copy dirty vs its blob at `commit`? The
/// one definition shared by the status-derive path
/// (`FsPlanStateLookup::worktree_status`) and the preview path. The
/// blob is the source of truth: a worktree file that differs is
/// `BodyDirty`; a missing worktree file whose blob exists at `commit`
/// is `PlanFileMissing`; a path absent at `commit` is `Clean`
/// regardless of the worktree (matching `git diff HEAD`, which ignores
/// paths not in the commit). Reuses the [`Repo`] handle.
impl Repo {
    pub fn plan_body_status(
        &self,
        commit: &CommitSha,
        rel_path: &str,
    ) -> clank_core::vocab::PlanWorktreeStatus {
        use clank_core::vocab::PlanWorktreeStatus;
        let worktree = self
            .0
            .workdir()
            .map(|w| w.join(rel_path))
            .and_then(|abs| std::fs::read(abs).ok());
        match (commit_blob_bytes(&self.0, commit, rel_path), worktree) {
            (Some(b), Some(w)) if b == w => PlanWorktreeStatus::Clean,
            (Some(_), Some(_)) => PlanWorktreeStatus::BodyDirty,
            (Some(_), None) => PlanWorktreeStatus::PlanFileMissing,
            (None, _) => PlanWorktreeStatus::Clean,
        }
    }
}

/// `rel_path`'s blob bytes at `commit`, or `None` if the commit/path
/// can't be resolved (absent path, bad oid, unreadable object).
fn commit_blob_bytes(git: &gix::Repository, commit: &CommitSha, rel_path: &str) -> Option<Vec<u8>> {
    let oid = gix::ObjectId::from_hex(commit.as_str().as_bytes()).ok()?;
    let tree = git.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.lookup_entry_by_path(rel_path).ok().flatten()?;
    Some(git.find_blob(entry.oid()).ok()?.data.clone())
}

/// Resolve HEAD to its commit SHA. Returns `Ok(None)` for an empty
/// repo (unborn HEAD) or when the path isn't a git repository.
///
/// gix backend; preserves the legacy shell-out's lenient semantics:
/// reads the HEAD ref's target WITHOUT peeling/validating the
/// pointed-at object exists (matches `git rev-parse HEAD` behavior
/// on a repo with a dangling HEAD ref). Callers handle `None` as
/// "no commits to fold from" / cold cache.
pub fn rev_parse_head(repo: &Path) -> Result<Option<CommitSha>, GitIoError> {
    let oid_opt: Option<gix::ObjectId> = (|| {
        let repo = gix::open(repo).ok()?;
        let head = repo.head().ok()?;
        head.id().map(|id| id.detach())
    })();
    match oid_opt {
        None => Ok(None),
        Some(oid) => Ok(Some(parse_sha("head_id", &oid.to_string())?)),
    }
}

/// Make a gix-returned path absolute. gix yields paths as it
/// discovered them (usually absolute when opened with an absolute
/// repo path); a relative one is resolved against `repo`.
fn absolutize(repo: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo.join(p)
    }
}

/// The PER-WORKTREE git directory — `<repo>/.git` for a main
/// checkout, `<common>/worktrees/<id>` for a linked worktree. Use for
/// paths that are per-worktree (HEAD, index, our `clank-rewrite`
/// scratch). NOT for shared paths like `hooks/` — those live in the
/// common dir; use [`common_dir`]. Replaces hardcoded `<repo>/.git`
/// and `git rev-parse --git-path …` for per-worktree paths.
pub fn git_dir(repo: &Path) -> Result<PathBuf, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "open".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    Ok(absolutize(repo, r.git_dir()))
}

/// The SHARED (common) git directory — `<main>/.git`, identical
/// across all linked worktrees. Use for shared paths: `hooks/`,
/// `config`, `info/`. Replaces `git rev-parse --git-path hooks/…`.
pub fn common_dir(repo: &Path) -> Result<PathBuf, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "open".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    Ok(absolutize(repo, r.common_dir()))
}

/// Discover the repository containing `start` — searching UPWARD,
/// like `git rev-parse --show-toplevel` — and return its working-tree
/// root. `Ok(None)` when `start` isn't inside a (non-bare) git repo.
/// Replaces `git rev-parse --show-toplevel`.
pub fn discover_work_dir(start: &Path) -> Result<Option<PathBuf>, GitIoError> {
    match gix::discover(start) {
        Ok(r) => Ok(r.workdir().map(Path::to_path_buf)),
        // Not inside a repo (or unreadable) — the caller decides
        // whether that's an error or a fallback.
        Err(_) => Ok(None),
    }
}

/// Read a git config key as a boolean, honoring git's bool syntax
/// (`true`/`1`/`yes`/`on` → true; `false`/`0`/`no`/`off` → false).
/// `None` when the key is absent (or unparseable). Replaces
/// `git config --get <key>` + a hand-rolled truthiness check.
pub fn config_bool(repo: &Path, key: &str) -> Result<Option<bool>, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "open".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    Ok(r.config_snapshot().boolean(key))
}

/// Resolve a revspec (`HEAD`, a sha, `refs/heads/<b>`, …) to a commit
/// SHA, or `None` if it can't be resolved. Matches `git rev-parse
/// --verify --quiet <rev>` (which exits non-zero + empty on an
/// unresolvable rev). Open/parse failures also fold to `None` — every
/// caller treats "couldn't resolve" uniformly.
pub fn resolve_commit(repo: &Path, rev: &str) -> Option<CommitSha> {
    let r = gix::open(repo).ok()?;
    let id = r.rev_parse_single(rev).ok()?;
    CommitSha::parse(&id.detach().to_string()).ok()
}

/// The short name of the branch HEAD points to (e.g. `master`), or
/// `None` when HEAD is detached or unborn. Replaces
/// `git symbolic-ref --short HEAD` (which exits non-zero when
/// detached — callers map `None` to their own fallback/error).
pub fn current_branch(repo: &Path) -> Result<Option<String>, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "current_branch".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    match r.head_name() {
        Ok(opt) => Ok(opt.map(|name| name.shorten().to_string())),
        Err(e) => Err(GitIoError::NonZero {
            context: "current_branch".into(),
            code: None,
            stderr: format!("head_name: {e}"),
        }),
    }
}

/// The commit's subject line — gix `summary()`, matching git's `%s`
/// subject folding. Replaces `git log -1 --format=%s <sha>`.
pub fn commit_subject(repo: &Path, sha: &CommitSha) -> Result<String, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "commit_subject".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
        context: "commit_subject".into(),
        detail: format!("oid hex: {e}"),
    })?;
    let commit = r.find_commit(oid).map_err(|e| GitIoError::NonZero {
        context: "commit_subject".into(),
        code: None,
        stderr: format!("find_commit: {e}"),
    })?;
    let msg = commit.message().map_err(|e| GitIoError::NonZero {
        context: "commit_subject".into(),
        code: None,
        stderr: format!("message: {e}"),
    })?;
    Ok(msg.summary().to_string())
}

/// Live HEAD facts for the commit-tag invariant
/// (`commit-tag-fixup-is-first-class-state`): HEAD's subject + the
/// plan files its diff touched + repo adoption, packaged as a
/// [`clank_core::wait::HeadCommit`] for `derive_status`. Returns
/// `None` when the repo has no HEAD (fresh repo). Errors reading the
/// subject/diff degrade to empty (no subject) / no touches — the
/// invariant then simply finds nothing to flag, never a false alarm.
pub fn head_commit(
    repo: &Path,
    state: &crate::repo_state::RepoState,
) -> Option<clank_core::wait::HeadCommit> {
    let head = state.head.as_ref()?;
    let subject = commit_subject(repo, head).unwrap_or_default();
    let from = parent_of(repo, head).ok().flatten();
    let touched = commit_events_between_at(repo, from.as_ref(), head)
        .ok()
        .and_then(|evs| evs.into_iter().last())
        .map(|ev| {
            ev.changes
                .plan_touches
                .into_iter()
                .map(|t| t.plan)
                .collect()
        })
        .unwrap_or_default();
    Some(clank_core::wait::HeadCommit {
        sha: head.clone(),
        subject,
        touched,
        adopted: state.fold.adopted,
    })
}

/// The commit's body (`%b`) — everything after the subject and its
/// blank line, or `""` if none. Replaces `git log -1 --format=%b
/// <sha>`.
pub fn commit_body(repo: &Path, sha: &CommitSha) -> Result<String, GitIoError> {
    let r = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: "commit_body".into(),
        code: None,
        stderr: format!("open: {e}"),
    })?;
    let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
        context: "commit_body".into(),
        detail: format!("oid hex: {e}"),
    })?;
    let commit = r.find_commit(oid).map_err(|e| GitIoError::NonZero {
        context: "commit_body".into(),
        code: None,
        stderr: format!("find_commit: {e}"),
    })?;
    let msg = commit.message().map_err(|e| GitIoError::NonZero {
        context: "commit_body".into(),
        code: None,
        stderr: format!("message: {e}"),
    })?;
    Ok(msg.body().map(|b| b.to_string()).unwrap_or_default())
}

/// All commits reachable from `head`, as a `full-sha -> subject` map
/// — matches `git log --format=%H<TAB>%s <head>` (all ancestors, not
/// first-parent). For batch subject lookups (the html log view).
pub fn ancestor_subjects(
    repo: &Path,
    head: &CommitSha,
) -> Result<std::collections::BTreeMap<String, String>, GitIoError> {
    let err = |stage: &str, e: &dyn std::fmt::Display| GitIoError::NonZero {
        context: "ancestor_subjects".into(),
        code: None,
        stderr: format!("{stage}: {e}"),
    };
    let r = gix::open(repo).map_err(|e| err("open", &e))?;
    let tip = gix::ObjectId::from_hex(head.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
        context: "ancestor_subjects".into(),
        detail: format!("oid hex: {e}"),
    })?;
    let walk = r.rev_walk([tip]).all().map_err(|e| err("rev_walk", &e))?;
    let mut out = std::collections::BTreeMap::new();
    for info in walk {
        let info = info.map_err(|e| err("walk iter", &e))?;
        let commit = info.object().map_err(|e| err("info.object", &e))?;
        let subject = commit
            .message()
            .map_err(|e| err("message", &e))?
            .summary()
            .to_string();
        out.insert(info.id.to_string(), subject);
    }
    Ok(out)
}

/// Return the blob content at `rel_path` in the tree of `rev`.
/// Preserves trailing whitespace (newlines matter for hashing).
///
/// Errors if the repo can't be opened, the commit/path isn't
/// found, or the entry isn't a blob.
pub fn show_blob(repo: &Path, rev: &CommitSha, rel_path: &Path) -> Result<String, GitIoError> {
    let rev_str = rev.as_str();
    let path_str = rel_path.to_string_lossy();
    let context = format!("show_blob {rev_str}:{path_str}");
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let oid = gix::ObjectId::from_hex(rev_str.as_bytes()).map_err(|e| GitIoError::Parse {
        context: context.clone(),
        detail: format!("rev oid hex: {e}"),
    })?;
    let commit = repo.find_commit(oid).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("find_commit: {e}"),
    })?;
    let tree = commit.tree().map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("commit.tree: {e}"),
    })?;
    let entry = tree
        .lookup_entry_by_path(path_str.as_ref())
        .map_err(|e| GitIoError::NonZero {
            context: context.clone(),
            code: None,
            stderr: format!("lookup_entry_by_path: {e}"),
        })?
        .ok_or_else(|| GitIoError::NonZero {
            context: context.clone(),
            code: Some(128),
            stderr: format!("path `{path_str}` not in tree at {rev_str}"),
        })?;
    let blob = repo
        .find_blob(entry.oid())
        .map_err(|e| GitIoError::NonZero {
            context,
            code: None,
            stderr: format!("find_blob: {e}"),
        })?;
    Ok(String::from_utf8_lossy(&blob.data).into_owned())
}

/// True iff `ancestor` is reachable from `head` along any parent
/// chain. Used by Phase-2 incremental cache loading to find an
/// ancestor cache to fold-forward from.
///
/// gix backend via `merge_base(a, b) == a` idiom. Disjoint
/// histories (no common ancestor) → `Ok(false)` matching the
/// legacy `git merge-base --is-ancestor` exit-1 case. Other gix
/// errors (object missing, cache failure) propagate.
pub fn is_ancestor(
    repo: &Path,
    ancestor: &CommitSha,
    head: &CommitSha,
) -> Result<bool, GitIoError> {
    let context = format!("is_ancestor {} {}", ancestor.as_str(), head.as_str());
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let ancestor_oid =
        gix::ObjectId::from_hex(ancestor.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
            context: context.clone(),
            detail: format!("ancestor oid hex: {e}"),
        })?;
    let head_oid =
        gix::ObjectId::from_hex(head.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
            context: context.clone(),
            detail: format!("head oid hex: {e}"),
        })?;
    match repo.merge_base(ancestor_oid, head_oid) {
        Ok(id) => Ok(id.detach() == ancestor_oid),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(GitIoError::NonZero {
            context,
            code: None,
            stderr: format!("merge_base: {e}"),
        }),
    }
}

/// Walk first-parent commits and collect `CommitMeta` (sha, author
/// timestamp, subject) oldest-first — matches the legacy
/// `git log --first-parent --reverse --format=%H%x00%at%x00%s`
/// contract.
///
/// gix walks newest-first regardless of `Sorting` choice; this
/// helper does the `.reverse()` at the end so callers consume
/// the timeline chronologically (see sharp edge #8 in the plan).
///
/// `from = Some(oid)` excludes that commit and its ancestors
/// (gix's `with_hidden`, equivalent to `git log from..tip`).
/// `from = None` walks from `tip` all the way back to the root.
fn first_parent_walk(
    repo: &gix::Repository,
    from: Option<gix::ObjectId>,
    tip: gix::ObjectId,
    context: &'static str,
) -> Result<Vec<CommitMeta>, GitIoError> {
    let walk_err = |e: &dyn std::fmt::Display, stage: &str| GitIoError::NonZero {
        context: context.to_string(),
        code: None,
        stderr: format!("{stage}: {e}"),
    };
    let mut builder = repo.rev_walk([tip]).first_parent_only();
    if let Some(from_oid) = from {
        builder = builder.with_hidden([from_oid]);
    }
    let walk = builder.all().map_err(|e| walk_err(&e, "rev_walk"))?;
    let mut out = Vec::new();
    for info in walk {
        let info = info.map_err(|e| walk_err(&e, "walk iter"))?;
        let commit = info.object().map_err(|e| walk_err(&e, "info.object"))?;
        let author = commit.author().map_err(|e| walk_err(&e, "commit.author"))?;
        let author_ts: i64 = author
            .time()
            .map_err(|e| walk_err(&e, "author.time parse"))?
            .seconds;
        let msg = commit
            .message()
            .map_err(|e| walk_err(&e, "commit.message"))?;
        // `summary()` trims trailing whitespace and folds internal
        // newlines — matches the legacy `git log --format=%s` output
        // (which strips the trailing \n that git stores). Raw
        // `msg.title` retains the trailing newline.
        let subject = msg.summary().to_string();
        let sha = parse_sha(context, &info.id.to_string())?;
        out.push(CommitMeta {
            sha,
            author_ts,
            subject,
        });
    }
    // gix walks newest-first; reverse to match the legacy
    // oldest-first contract every caller depends on.
    out.reverse();
    Ok(out)
}

/// First-parent commits between `base` (exclusive) and `tip`
/// (inclusive), oldest-first.
pub fn first_parent_commits_between(
    repo: &Path,
    base: &CommitSha,
    tip: &CommitSha,
) -> Result<Vec<CommitMeta>, GitIoError> {
    const CONTEXT: &str = "first_parent_commits_between";
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: CONTEXT.into(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let base_oid =
        gix::ObjectId::from_hex(base.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
            context: CONTEXT.into(),
            detail: format!("base oid hex: {e}"),
        })?;
    let tip_oid =
        gix::ObjectId::from_hex(tip.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
            context: CONTEXT.into(),
            detail: format!("tip oid hex: {e}"),
        })?;
    first_parent_walk(&repo, Some(base_oid), tip_oid, CONTEXT)
}

/// First parent of the given commit. Returns `Ok(None)` for the
/// root commit (no parent) or when the commit / repo can't be
/// opened (preserves the legacy shell-out's lenient semantics —
/// missing commits map to None, not an error).
pub fn parent_of(repo: &Path, sha: &CommitSha) -> Result<Option<CommitSha>, GitIoError> {
    let parent_opt: Option<gix::ObjectId> = (|| {
        let repo = gix::open(repo).ok()?;
        let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).ok()?;
        let commit = repo.find_commit(oid).ok()?;
        commit.parent_ids().next().map(|id| id.detach())
    })();
    match parent_opt {
        None => Ok(None),
        Some(oid) => Ok(Some(parse_sha("parent_of", &oid.to_string())?)),
    }
}

/// All commits along the first-parent chain from the root up to
/// HEAD, oldest-first. Used for the attribution walk. Each entry
/// carries the author timestamp (unix seconds) and the commit
/// subject (first line) via gix's `rev_walk` with
/// `first_parent_only`.
///
/// We deliberately don't try to bound by an `<intro>..HEAD` range: with
/// multiple sessions each having their own intro, identifying the
/// topologically earliest plan_intro requires a separate query. Clank
/// repos are small enough that walking from the root is cheap and avoids
/// a correctness footgun.
/// List a single plan's strippable `.clank/` paths in the tree
/// at `sha`: `.clank/plans/<stem>.md` (when present) plus, when
/// `include_finalize` is true, every path under
/// `.clank/finished/<stem>/`. Sorted. Used by the single-plan
/// rewrite preview so classification can be tree-based instead of
/// diff-touch based.
pub fn tree_plan_paths(
    repo: &Path,
    sha: &CommitSha,
    stem: &str,
    include_finalize: bool,
) -> Result<Vec<String>, GitIoError> {
    let candidates: Vec<String> = {
        let mut v = vec![format!(".clank/plans/{stem}.md")];
        if include_finalize {
            v.push(format!(".clank/finished/{stem}.md"));
        }
        v
    };
    let paths_opt: Option<Vec<String>> = (|| {
        let repo = gix::open(repo).ok()?;
        let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).ok()?;
        let tree = repo.find_commit(oid).ok()?.tree().ok()?;
        let mut found: Vec<String> = candidates
            .into_iter()
            .filter(|path| {
                tree.lookup_entry_by_path(path)
                    .ok()
                    .flatten()
                    .filter(|e| e.mode().is_blob())
                    .is_some()
            })
            .collect();
        found.sort();
        Some(found)
    })();
    Ok(paths_opt.unwrap_or_default())
}

/// List every blob path under `.clank/` in the tree at `sha`.
/// Returned sorted. Empty when the tree has no `.clank/` paths.
/// Used by the all-plans rewrite preview to compute strip_paths
/// from what's actually IN the tree, not what the commit's diff
/// touched — because every post-intro commit's tree inherits
/// `.clank/` content from its parent even when the commit's diff
/// didn't touch `.clank/`.
pub fn tree_clank_paths(repo: &Path, sha: &CommitSha) -> Result<Vec<String>, GitIoError> {
    let paths_opt: Option<Vec<String>> = (|| {
        let repo = gix::open(repo).ok()?;
        let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).ok()?;
        let tree = repo.find_commit(oid).ok()?.tree().ok()?;
        let mut recorder = gix::traverse::tree::Recorder::default();
        tree.traverse().breadthfirst(&mut recorder).ok()?;
        let mut paths: Vec<String> = recorder
            .records
            .into_iter()
            .filter(|e| e.mode.is_blob() && e.filepath.starts_with(b".clank/"))
            .map(|e| String::from_utf8_lossy(&e.filepath).into_owned())
            .collect();
        paths.sort();
        Some(paths)
    })();
    Ok(paths_opt.unwrap_or_default())
}

/// Number of parents on `sha`. Two or more = merge commit. Errors
/// if the repo or commit can't be opened.
pub fn commit_parent_count(repo: &Path, sha: &CommitSha) -> Result<usize, GitIoError> {
    let context = format!("commit_parent_count {}", sha.as_str());
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
        context: context.clone(),
        detail: format!("oid hex: {e}"),
    })?;
    let commit = repo.find_commit(oid).map_err(|e| GitIoError::NonZero {
        context,
        code: None,
        stderr: format!("find_commit: {e}"),
    })?;
    Ok(commit.parent_ids().count())
}

/// First-parent walk pinned to a specific tip SHA. Unlike
/// `first_parent_commits` (which walks live HEAD), this anchors to
/// the caller's snapshot so the resulting range never disagrees
/// with a value the daemon already projected.
pub fn first_parent_commits_to(
    repo: &Path,
    tip: &CommitSha,
) -> Result<Vec<CommitMeta>, GitIoError> {
    const CONTEXT: &str = "first_parent_commits_to";
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: CONTEXT.into(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let tip_oid =
        gix::ObjectId::from_hex(tip.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
            context: CONTEXT.into(),
            detail: format!("tip oid hex: {e}"),
        })?;
    first_parent_walk(&repo, None, tip_oid, CONTEXT)
}

pub fn first_parent_commits(repo: &Path) -> Result<Vec<CommitMeta>, GitIoError> {
    const CONTEXT: &str = "first_parent_commits";
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: CONTEXT.into(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let head_oid = match repo.head().ok().and_then(|h| h.id()) {
        Some(id) => id.detach(),
        None => return Ok(Vec::new()), // unborn HEAD / no commits → empty
    };
    first_parent_walk(&repo, None, head_oid, CONTEXT)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
}

/// Per-commit decision in the fold walk, from a one-level
/// comparison of parent vs child root trees. The recursive diff
/// runs only on `FullDiff` commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffGate {
    /// Root trees identical: empty commit, no changes at all.
    NoChanges,
    /// Roots differ but the `.clank` entry OID is unchanged (or
    /// absent on both sides): only non-clank paths changed.
    /// Because a tree entry's OID covers everything beneath it,
    /// entry equality proves nothing under `.clank/` changed.
    CodeOnly,
    /// The `.clank` entry differs: a real diff is required.
    FullDiff,
}

fn diff_gate(
    roots_equal: bool,
    parent_clank: Option<gix::ObjectId>,
    child_clank: Option<gix::ObjectId>,
) -> DiffGate {
    if roots_equal {
        DiffGate::NoChanges
    } else if parent_clank == child_clank {
        DiffGate::CodeOnly
    } else {
        DiffGate::FullDiff
    }
}

/// OID of the top-level `.clank` entry of `tree_id`; `None` when
/// absent (pre-clank history).
fn clank_entry_oid(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    context: &str,
) -> Result<Option<gix::ObjectId>, GitIoError> {
    let tree = repo.find_tree(tree_id).map_err(|e| GitIoError::NonZero {
        context: context.to_string(),
        code: None,
        stderr: format!("find_tree: {e}"),
    })?;
    for entry in tree.iter() {
        let entry = entry.map_err(|e| GitIoError::Parse {
            context: context.to_string(),
            detail: format!("tree entry: {e}"),
        })?;
        if entry.filename() == ".clank" {
            return Ok(Some(entry.oid().to_owned()));
        }
    }
    Ok(None)
}

/// Walk `tip`'s first-parent chain (inclusive of `tip`) and return
/// the first sha present in `candidates` — `None` if the chain
/// reaches the root without a hit.
///
/// This is the fold's notion of ancestry: a checkpoint is only a
/// valid resume point if it sits ON the target's first-parent
/// chain. Graph ancestry (`is_ancestor` via merge-base) is NOT
/// sufficient — a checkpoint written on a side branch that later
/// merges in is a graph ancestor, but resuming from it mixes
/// side-branch folded state with the first-parent walk and
/// duplicates the merge's `.clank` changes (codex 6b1c549).
pub fn first_parent_chain_find_at(
    repo_path: &Path,
    tip: &CommitSha,
    candidates: &std::collections::HashSet<CommitSha>,
) -> Result<Option<CommitSha>, GitIoError> {
    open(repo_path)?.first_parent_chain_find(tip, candidates)
}

impl Repo {
    /// Handle-based [`first_parent_chain_find_at`] — the fold reuses its
    /// one handle for the checkpoint-ancestry probe.
    pub fn first_parent_chain_find(
        &self,
        tip: &CommitSha,
        candidates: &std::collections::HashSet<CommitSha>,
    ) -> Result<Option<CommitSha>, GitIoError> {
        const CONTEXT: &str = "first_parent_chain_find";
        if candidates.is_empty() {
            return Ok(None);
        }
        let repo = &self.0;
        let mut cursor =
            gix::ObjectId::from_hex(tip.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
                context: CONTEXT.into(),
                detail: format!("tip oid hex: {e}"),
            })?;
        loop {
            let sha = parse_sha(CONTEXT, &cursor.to_string())?;
            if candidates.contains(&sha) {
                return Ok(Some(sha));
            }
            let commit = repo.find_commit(cursor).map_err(|e| GitIoError::NonZero {
                context: CONTEXT.into(),
                code: None,
                stderr: format!("find_commit: {e}"),
            })?;
            match commit.parent_ids().next() {
                Some(p) => cursor = p.detach(),
                None => return Ok(None),
            }
        }
    }
}

/// First-parent `CommitEvent`s between `base` (exclusive; `None` =
/// repo root) and `tip` (inclusive), oldest-first — the fold's
/// event producer.
///
/// One repository open for the whole walk, carrying each commit's
/// root tree forward (it is the next commit's parent tree). Per
/// commit, the root trees are compared one level deep and the
/// recursive diff runs only when the `.clank` entry OID changed —
/// the same tree-entry short-circuit `git log -- .clank` uses for
/// pathspec limiting. Pre-clank history (no `.clank` entry on
/// either side) is near-free, which subsumes "start the fold where
/// `.clank` was introduced".
///
/// Commits where `.clank` DID change get the full-repo diff, not a
/// `.clank`-subtree diff: renames crossing the `.clank` boundary
/// (plan resurrected from outside, plan file moved out) must keep
/// their rename pairing so `CommitChanges` matches
/// `diff_tree_changes` exactly. Such commits are the rare case.
pub fn commit_events_between_at(
    repo_path: &Path,
    base: Option<&CommitSha>,
    tip: &CommitSha,
) -> Result<Vec<CommitEvent>, GitIoError> {
    open(repo_path)?.commit_events_between(base, tip)
}

impl Repo {
    /// Handle-based [`commit_events_between_at`] — the fold reuses one
    /// handle across every range walk (warm object cache included).
    pub fn commit_events_between(
        &self,
        base: Option<&CommitSha>,
        tip: &CommitSha,
    ) -> Result<Vec<CommitEvent>, GitIoError> {
        const CONTEXT: &str = "commit_events_between";
        let walk_err = |e: &dyn std::fmt::Display, stage: &str| GitIoError::NonZero {
            context: CONTEXT.to_string(),
            code: None,
            stderr: format!("{stage}: {e}"),
        };
        let repo = &self.0;

        let tip_oid =
            gix::ObjectId::from_hex(tip.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
                context: CONTEXT.into(),
                detail: format!("tip oid hex: {e}"),
            })?;
        let base_oid = base
            .map(|b| {
                gix::ObjectId::from_hex(b.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
                    context: CONTEXT.into(),
                    detail: format!("base oid hex: {e}"),
                })
            })
            .transpose()?;
        if base_oid == Some(tip_oid) {
            return Ok(Vec::new());
        }

        struct RawCommit {
            oid: gix::ObjectId,
            tree: gix::ObjectId,
            first_parent: Option<gix::ObjectId>,
            sha: CommitSha,
            author_ts: i64,
            subject: String,
        }
        let mut builder = repo.rev_walk([tip_oid]).first_parent_only();
        if let Some(b) = base_oid {
            builder = builder.with_hidden([b]);
        }
        let walk = builder.all().map_err(|e| walk_err(&e, "rev_walk"))?;
        let mut raws = Vec::new();
        for info in walk {
            let info = info.map_err(|e| walk_err(&e, "walk iter"))?;
            let commit = info.object().map_err(|e| walk_err(&e, "info.object"))?;
            let author = commit.author().map_err(|e| walk_err(&e, "commit.author"))?;
            let author_ts = author
                .time()
                .map_err(|e| walk_err(&e, "author.time parse"))?
                .seconds;
            let subject = commit
                .message()
                .map_err(|e| walk_err(&e, "commit.message"))?
                .summary()
                .to_string();
            let tree = commit
                .tree_id()
                .map_err(|e| walk_err(&e, "commit.tree_id"))?
                .detach();
            let first_parent = commit.parent_ids().next().map(|p| p.detach());
            let sha = parse_sha(CONTEXT, &info.id.to_string())?;
            raws.push(RawCommit {
                oid: info.id,
                tree,
                first_parent,
                sha,
                author_ts,
                subject,
            });
        }
        // gix walks newest-first; the fold applies oldest-first.
        raws.reverse();

        let mut out = Vec::with_capacity(raws.len());
        // (commit, root tree) of the previously processed commit — in a
        // first-parent walk that IS the next commit's first parent,
        // except across the hidden-`base` boundary.
        let mut prev: Option<(gix::ObjectId, gix::ObjectId)> = None;
        for raw in raws {
            let parent_tree = match raw.first_parent {
                None => None, // root commit: diff against the empty tree
                Some(p) => match prev {
                    Some((prev_oid, prev_tree)) if prev_oid == p => Some(prev_tree),
                    // Walk start (parent hidden behind `base`): one lookup.
                    _ => Some(
                        repo.find_commit(p)
                            .map_err(|e| walk_err(&e, "parent find_commit"))?
                            .tree_id()
                            .map_err(|e| walk_err(&e, "parent tree_id"))?
                            .detach(),
                    ),
                },
            };
            let roots_equal = parent_tree == Some(raw.tree);
            let gate = if roots_equal {
                DiffGate::NoChanges
            } else {
                let parent_clank = match parent_tree {
                    Some(t) => clank_entry_oid(&repo, t, CONTEXT)?,
                    None => None,
                };
                let child_clank = clank_entry_oid(&repo, raw.tree, CONTEXT)?;
                diff_gate(roots_equal, parent_clank, child_clank)
            };
            let changes = match gate {
                DiffGate::NoChanges => CommitChanges::default(),
                DiffGate::CodeOnly => CommitChanges {
                    has_non_plan_code_changes: true,
                    ..Default::default()
                },
                DiffGate::FullDiff => {
                    let parent = parent_tree
                        .map(|t| {
                            repo.find_tree(t).map_err(|e| GitIoError::NonZero {
                                context: CONTEXT.to_string(),
                                code: None,
                                stderr: format!("parent find_tree: {e}"),
                            })
                        })
                        .transpose()?;
                    let child = repo.find_tree(raw.tree).map_err(|e| GitIoError::NonZero {
                        context: CONTEXT.to_string(),
                        code: None,
                        stderr: format!("find_tree: {e}"),
                    })?;
                    diff_trees_changes(&repo, parent.as_ref(), &child, CONTEXT)?
                }
            };
            prev = Some((raw.oid, raw.tree));
            out.push(CommitEvent {
                commit: raw.sha,
                author_ts: raw.author_ts,
                subject: raw.subject,
                changes,
            });
        }
        Ok(out)
    }
}

/// Structured changes for `sha` against its first parent (or the
/// empty tree for the root commit), translated into a
/// `CommitChanges`.
///
/// gix backend via `repo.diff_tree_to_tree` with
/// `Rewrites::default()` (50% similarity — matches the legacy
/// `-M` flag). Tree-level entries are filtered out via
/// `EntryMode::is_no_tree()`; the legacy parser implicitly did
/// the same by only seeing leaf records from
/// `diff-tree --name-status`.
///
/// First-parent semantics for merge commits fall out from
/// `parent_ids().next()` being the first parent (matches the
/// legacy `-m --first-parent` flag combination).
pub fn diff_tree_changes(repo: &Path, sha: &CommitSha) -> Result<CommitChanges, GitIoError> {
    let context = format!("diff_tree_changes {}", sha.as_str());
    let repo = gix::open(repo).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("gix open: {e}"),
    })?;
    let oid = gix::ObjectId::from_hex(sha.as_str().as_bytes()).map_err(|e| GitIoError::Parse {
        context: context.clone(),
        detail: format!("oid hex: {e}"),
    })?;
    let commit = repo.find_commit(oid).map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("find_commit: {e}"),
    })?;
    let this_tree = commit.tree().map_err(|e| GitIoError::NonZero {
        context: context.clone(),
        code: None,
        stderr: format!("commit.tree: {e}"),
    })?;
    // First-parent semantics fall out: `parent_ids().next()` IS
    // the first parent for merges. Root commit → no parent →
    // diff against the empty tree (matches legacy `--root`).
    let parent_tree_owned = match commit.parent_ids().next() {
        Some(p) => Some(
            repo.find_commit(p.detach())
                .map_err(|e| GitIoError::NonZero {
                    context: context.clone(),
                    code: None,
                    stderr: format!("parent find_commit: {e}"),
                })?
                .tree()
                .map_err(|e| GitIoError::NonZero {
                    context: context.clone(),
                    code: None,
                    stderr: format!("parent tree: {e}"),
                })?,
        ),
        None => None,
    };
    diff_trees_changes(&repo, parent_tree_owned.as_ref(), &this_tree, &context)
}

/// Recursive rename-tracking diff of two trees → `CommitChanges`.
/// Shared by `diff_tree_changes` (per-commit reference shape) and
/// `commit_events_between`'s gated full-diff path — one source of
/// truth for diff semantics.
fn diff_trees_changes(
    repo: &gix::Repository,
    parent_tree: Option<&gix::Tree<'_>>,
    this_tree: &gix::Tree<'_>,
    context: &str,
) -> Result<CommitChanges, GitIoError> {
    // Enable rename tracking with the git default 50% similarity
    // (matches legacy `-M` flag). diff_tree_to_tree with `None`
    // for options uses the repo's configured defaults, which may
    // have rewrites=None — so build the Options explicitly.
    let opts = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let raw_changes = repo
        .diff_tree_to_tree(parent_tree, Some(this_tree), opts)
        .map_err(|e| GitIoError::NonZero {
            context: context.to_string(),
            code: None,
            stderr: format!("diff_tree_to_tree: {e}"),
        })?;
    let records: Vec<DiffRecord> = raw_changes
        .into_iter()
        .filter_map(diff_record_from_gix_change)
        .collect();
    Ok(apply_diff_records(&records))
}

/// Per-change record extracted from gix's `Change` enum. Mirrors
/// the (status_char, old_path, new_path) tuple the legacy
/// `diff-tree --name-status -M` parser produced — so the domain
/// logic in `apply_diff_records` can stay shape-for-shape
/// identical to the legacy `parse_diff_tree` body.
struct DiffRecord {
    /// 'A' add, 'D' delete, 'M' modify, 'R' rename, 'C' copy.
    status_char: char,
    is_rename: bool,
    old_path: Option<String>,
    new_path: String,
}

fn diff_record_from_gix_change(ch: gix::object::tree::diff::ChangeDetached) -> Option<DiffRecord> {
    use gix::object::tree::diff::ChangeDetached as Ch;
    match ch {
        Ch::Addition {
            location,
            entry_mode,
            ..
        } if entry_mode.is_no_tree() => Some(DiffRecord {
            status_char: 'A',
            is_rename: false,
            old_path: None,
            new_path: String::from_utf8_lossy(&location).into_owned(),
        }),
        Ch::Deletion {
            location,
            entry_mode,
            ..
        } if entry_mode.is_no_tree() => Some(DiffRecord {
            status_char: 'D',
            is_rename: false,
            old_path: None,
            new_path: String::from_utf8_lossy(&location).into_owned(),
        }),
        Ch::Modification {
            location,
            entry_mode,
            ..
        } if entry_mode.is_no_tree() => Some(DiffRecord {
            status_char: 'M',
            is_rename: false,
            old_path: None,
            new_path: String::from_utf8_lossy(&location).into_owned(),
        }),
        Ch::Rewrite {
            location,
            source_location,
            copy,
            entry_mode,
            ..
        } if entry_mode.is_no_tree() => Some(DiffRecord {
            status_char: if copy { 'C' } else { 'R' },
            is_rename: true,
            old_path: Some(String::from_utf8_lossy(&source_location).into_owned()),
            new_path: String::from_utf8_lossy(&location).into_owned(),
        }),
        // Trees only: ignore. The legacy `diff-tree
        // --name-status` parser handled every leaf shape it
        // saw — blobs, symlinks, and submodule commits all
        // appear as leaf records. gix's Change enum surfaces
        // tree-level diffs too, so we filter explicitly via
        // `is_no_tree()` rather than the narrower `is_blob()`
        // (which would silently drop symlinks/submodules under
        // .clank/ from `touched_clank` and `clank_paths_touched`).
        // Codex caught this on a6b7725.
        _ => None,
    }
}

/// Build a `CommitChanges` from per-change records. This is the
/// domain logic that was the body of `parse_diff_tree`; extracted
/// so the gix-backed `diff_tree_changes` and the parser-removal
/// share one source of truth.
fn apply_diff_records(records: &[DiffRecord]) -> CommitChanges {
    let mut plan_touches: Vec<PlanTouch> = Vec::new();
    let mut has_non_plan_code_changes = false;
    let mut clank_paths: Vec<String> = Vec::new();
    let mut clank_paths_touched: Vec<String> = Vec::new();
    let mut touched_clank = false;
    let mut plans_finished_added: std::collections::BTreeSet<PlanKey> = Default::default();

    for record in records {
        let DiffRecord {
            status_char,
            is_rename,
            old_path,
            new_path,
        } = record;
        let status_char = *status_char;
        let is_rename = *is_rename;
        let old_path: Option<&str> = old_path.as_deref();
        let new_path: &str = new_path.as_str();

        let new_rel = PathBuf::from(new_path);
        let old_rel = old_path.map(PathBuf::from);
        // Track every `.clank/`-prefixed DESTINATION path the
        // commit added/modified/renamed into existence. Source of
        // truth for the all-plans purge endpoint's strip_paths.
        // Skip pure deletions: their "new path" is absent from the
        // resulting tree, so there's nothing to strip there.
        let is_pure_delete = status_char == 'D' && !is_rename;
        if new_rel.starts_with(".clank") && !is_pure_delete {
            clank_paths.push(new_path.to_string());
        }
        // Track whether the commit touched ANY `.clank/` path on
        // either side (including pure deletes and renames out of
        // `.clank/`). The all-plans classifier uses this to make
        // a delete-only Clank commit `Drop` instead of
        // `KeepVerbatim`.
        let new_in_clank = new_rel.starts_with(".clank");
        let old_in_clank = old_rel.as_ref().is_some_and(|p| p.starts_with(".clank"));
        if new_in_clank || old_in_clank {
            touched_clank = true;
        }
        // Bidirectional path list: every `.clank/`-prefixed path
        // this commit's diff touched on either side. Captures
        // destinations of adds/modifies/renames AND sources of
        // deletes/renames-out. Used by the contribution check so a
        // commit that deletes a preserved path (e.g. removing
        // another plan's file under a single-plan purge) isn't
        // silently dropped.
        if new_in_clank {
            clank_paths_touched.push(new_path.to_string());
        }
        if old_in_clank && let Some(old) = old_path {
            clank_paths_touched.push(old.to_string());
        }
        let new_is_plan = is_plan_path(&new_rel);
        let old_is_plan = old_path
            .map(|p| is_plan_path(&PathBuf::from(p)))
            .unwrap_or(false);

        if new_is_plan || old_is_plan {
            // Resolve the plan key on each side. With nested-path
            // rejection (Phase 5 of event-log-and-finished), every
            // `.clank/plans/X.md` path uniquely identifies stem X,
            // so old and new keys differ iff the rename crosses
            // stems.
            let new_key = if new_is_plan {
                PlanKey::from_path(&new_rel)
            } else {
                None
            };
            let old_key = old_path.and_then(|p| PlanKey::from_path(&PathBuf::from(p)));

            let is_deletion = status_char == 'D' && !is_rename;

            if let (true, Some(old_k), Some(new_k)) = (is_rename, &old_key, &new_key)
                && old_k != new_k
            {
                // Cross-stem rename `git mv .clank/plans/foo.md
                // .clank/plans/bar.md`. Model as delete-old +
                // intro-new.
                plan_touches.push(PlanTouch {
                    plan: old_k.clone(),
                    kind: PlanTouchKind::Revision,
                    new_path: None,
                });
                plan_touches.push(PlanTouch {
                    plan: new_k.clone(),
                    kind: PlanTouchKind::Intro,
                    new_path: Some(new_rel.clone()),
                });
            } else if is_rename && old_is_plan && !new_is_plan {
                if let Some(old_k) = old_key {
                    if is_finished_path(&new_rel) {
                        if let Some(fk) = plan_key_from_finished_path(&new_rel) {
                            plans_finished_added.insert(fk);
                        }
                    }
                    plan_touches.push(PlanTouch {
                        plan: old_k,
                        kind: PlanTouchKind::Revision,
                        new_path: None,
                    });
                }
            } else if is_rename && !old_is_plan && new_is_plan {
                // Rename INTO `.clank/plans/<key>.md` from somewhere
                // else (e.g. resurrecting a plan from done/). Model
                // as Intro on the new key.
                if let Some(new_k) = new_key {
                    plan_touches.push(PlanTouch {
                        plan: new_k,
                        kind: PlanTouchKind::Intro,
                        new_path: Some(new_rel.clone()),
                    });
                }
            } else {
                let plan_key = match new_key.or(old_key) {
                    Some(id) => id,
                    None => continue,
                };
                let kind = match status_char {
                    'A' => PlanTouchKind::Intro,
                    _ => PlanTouchKind::Revision,
                };
                let new_path_for_touch = if is_deletion {
                    None
                } else {
                    Some(new_rel.clone())
                };
                plan_touches.push(PlanTouch {
                    plan: plan_key,
                    kind,
                    new_path: new_path_for_touch,
                });
            }
        } else if is_finished_path(&new_rel) && (status_char == 'A' || is_rename) {
            if let Some(key) = plan_key_from_finished_path(&new_rel) {
                plans_finished_added.insert(key);
            }
        } else if !new_rel.starts_with(".clank") {
            has_non_plan_code_changes = true;
        }
    }

    // Any new file in finished/ is a finish. Upgrade existing
    // delete touches, or add new Finish touches.
    for key in &plans_finished_added {
        let upgraded = plan_touches.iter_mut().any(|t| {
            if &t.plan == key && t.new_path.is_none() {
                t.kind = PlanTouchKind::Finish;
                true
            } else {
                false
            }
        });
        if !upgraded {
            plan_touches.push(PlanTouch {
                plan: key.clone(),
                kind: PlanTouchKind::Finish,
                new_path: None,
            });
        }
    }

    clank_paths.sort();
    clank_paths.dedup();
    clank_paths_touched.sort();
    clank_paths_touched.dedup();
    CommitChanges {
        plan_touches,
        has_non_plan_code_changes,
        clank_paths,
        touched_clank,
        clank_paths_touched,
    }
}

/// Walk `<repo>/.clank/agents/` and return every well-formed
/// feedback file with its body and mtime. Public for the live
/// overlay path in `rebuild_repo` and any caller that wants the
/// raw feedback set.
///
/// `parse_feedback_path` takes paths RELATIVE TO
/// `<repo>/.clank/`, so we strip that prefix before parsing
/// (the parser sees `agents/<author>/feedback/<plan-or-_>/
/// <ref>.md`).
pub fn collect_feedback_files(repo_root: &Path) -> Result<Vec<FeedbackBlob>, GitIoError> {
    let clank_root = repo_root.join(".clank");
    let agents_root = clank_root.join("agents");
    if !agents_root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    // Depth from `agents/`: <author>/feedback/<ref>.md = 3 segments.
    walk_files(&agents_root, 3, &mut paths).map_err(|e| GitIoError::Parse {
        context: "walk agents dir".into(),
        detail: format!("{e}"),
    })?;
    let mut out = Vec::with_capacity(paths.len());
    for abs in paths {
        let Ok(rel) = abs.strip_prefix(&clank_root) else {
            continue;
        };
        let Some(parsed) = parse_feedback_path(rel) else {
            continue;
        };
        let body = std::fs::read_to_string(&abs).map_err(|e| GitIoError::Parse {
            context: "read feedback file".into(),
            detail: format!("{}: {e}", abs.display()),
        })?;
        let created_at = file_mtime_unix_secs(&abs);
        out.push(FeedbackBlob {
            abs_path: abs,
            parsed,
            body,
            created_at,
        });
    }
    Ok(out)
}

/// File mtime as unix seconds. Returns 0 with a `tracing::warn` if the
/// metadata read or unix-epoch conversion fails — the UI treats 0 as
/// "no chronological hint" rather than erroring the whole request.
/// Shared by both the rebuild path (initial `collect_feedback_files`)
/// and the watcher-driven `runtime::upsert_*` callsites.
pub(crate) fn file_mtime_unix_secs(path: &Path) -> i64 {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = ?err,
                    "feedback mtime before unix epoch; falling back to 0"
                );
                0
            }
        },
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = ?err,
                "feedback mtime read failed; falling back to 0"
            );
            0
        }
    }
}

fn walk_files(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    fn walk(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if depth > max {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(&path, depth + 1, max, out)?;
            } else if ft.is_file() {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, 1, max_depth, out)
}

/// True iff `rel` is exactly `.clank/finished/<name>.md` (flat, no subdirs).
fn is_finished_path(rel: &Path) -> bool {
    let mut comps = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some(".clank") {
        return false;
    }
    if comps.next() != Some("finished") {
        return false;
    }
    let third = match comps.next() {
        Some(s) => s,
        None => return false,
    };
    comps.next().is_none() && third.ends_with(".md")
}

/// Extract the `PlanKey` from a `.clank/finished/<stem>.md` path.
fn plan_key_from_finished_path(rel: &Path) -> Option<PlanKey> {
    let mut comps = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some(".clank") {
        return None;
    }
    if comps.next() != Some("finished") {
        return None;
    }
    let name = comps.next()?;
    if comps.next().is_some() {
        return None;
    }
    let stem = name.strip_suffix(".md")?;
    PlanKey::parse(stem).ok()
}

fn is_plan_path(rel: &Path) -> bool {
    // `.clank/plans/<name>.md` (no nested subdirs).
    let mut comps = rel.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some(".clank") {
        return false;
    }
    if comps.next() != Some("plans") {
        return false;
    }
    let third = match comps.next() {
        Some(s) => s,
        None => return false,
    };
    comps.next().is_none() && third.ends_with(".md")
}

/// The `origin` remote's fetch URL, or `None` if unset. gix-backed
/// (config), replacing `git remote get-url origin`.
pub fn origin_url(repo: &Path) -> Option<String> {
    let r = gix::open(repo).ok()?;
    let remote = r.find_remote("origin").ok()?;
    let url = remote.url(gix::remote::Direction::Fetch)?;
    Some(url.to_bstring().to_string())
}

/// Blob content of `rel` at `rev` (e.g. `"HEAD"`, `"HEAD~"`). gix-backed
/// (resolve + [`show_blob`]), replacing `git show <rev>:<rel>`. Errors
/// if the rev or path can't be resolved.
pub fn blob_at_rev(repo: &Path, rev: &str, rel: &str) -> Result<String, GitIoError> {
    let sha = resolve_commit(repo, rev)
        .ok_or_else(|| nonzero(format!("resolve `{rev}`"), "no such revision"))?;
    show_blob(repo, &sha, Path::new(rel))
}

// ── subprocess reads (gix can't reproduce git's exact OUTPUT yet) ──
// These stay `git` subprocesses behind the boundary: each depends on
// git's precise textual output (a unified-diff patch, `--name-status`
// with git's rename semantics, `check-ignore -v`'s matching rule, or
// the `%ai` date format) that gix doesn't reproduce cheaply. Converting
// them to gix is future work, invisible to callers behind these fns.

fn read_git_stdout(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitIoError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| nonzero(format!("git {}", args.join(" ")), e))?;
    if !out.status.success() {
        return Err(nonzero(
            format!("git {}", args.join(" ")),
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(out.stdout)
}

/// `git diff-tree --no-commit-id --name-status [-M] -r HEAD` → the
/// non-empty `<status>\t<path>[\t<path2>]` lines. `detect_renames`
/// adds `-M`. Used by `purge --amend` and `unfinish`'s finish-shape
/// guards, which depend on git's exact name-status + rename semantics.
pub fn diff_tree_name_status(repo: &Path, detect_renames: bool) -> Result<Vec<String>, GitIoError> {
    let mut args = vec!["diff-tree", "--no-commit-id", "--name-status"];
    if detect_renames {
        args.push("-M");
    }
    args.extend(["-r", "HEAD"]);
    let stdout = read_git_stdout(repo, &args)?;
    Ok(String::from_utf8_lossy(&stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// `git check-ignore -v <rel>`: `Some(rule)` (the first `-v` line — the
/// matching gitignore source) if git ignores `rel`, else `None`. The
/// path needn't exist; git matches patterns. `None` on spawn failure.
pub fn check_ignore(repo: &Path, rel: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-v"])
        .arg(repo.join(rel))
        .output()
        .ok()?;
    if out.status.code() != Some(0) {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim_end()
            .to_string(),
    )
}

/// `git diff <range>` patch text (range is `a..b` or a single rev).
/// git's exact unified-diff format is the contract (it's written to a
/// patch file), so this stays git.
pub fn diff_range_patch(repo: &Path, range: &str) -> Result<Vec<u8>, GitIoError> {
    read_git_stdout(repo, &["diff", range])
}

/// `git show --format=fuller --patch <sha>` — a commit's patch with a
/// fuller header (for stacked-diff synthesis).
pub fn commit_show_patch(repo: &Path, sha: &str) -> Result<Vec<u8>, GitIoError> {
    read_git_stdout(repo, &["show", "--format=fuller", "--patch", sha])
}

/// `git show --no-color --pretty=format: <sha>` — just a commit's diff
/// (empty pretty header), for the HTML renderer's unified-diff parser.
pub fn commit_diff_text(repo: &Path, sha: &str) -> Result<Vec<u8>, GitIoError> {
    read_git_stdout(repo, &["show", "--no-color", "--pretty=format:", sha])
}

/// `(author "Name <email>", date "%ai", body)` for `sha`, all empty on
/// failure. Kept on git for the `%ai` date format `clank log` displays.
pub fn commit_meta(repo: &Path, sha: &CommitSha) -> (String, String, String) {
    let Ok(stdout) = read_git_stdout(
        repo,
        &["log", "-1", "--format=%an <%ae>%n%ai%n%B", sha.as_str()],
    ) else {
        return (String::new(), String::new(), String::new());
    };
    let text = String::from_utf8_lossy(&stdout);
    let mut lines = text.lines();
    let author = lines.next().unwrap_or("").to_string();
    let date = lines.next().unwrap_or("").to_string();
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    (author, date, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> gix::ObjectId {
        gix::ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    #[test]
    fn diff_gate_matrix() {
        use DiffGate::*;
        // (roots_equal, parent_clank, child_clank) → gate
        let cases = [
            // empty commit: no work at all
            (true, None, None, NoChanges),
            (true, Some(oid(1)), Some(oid(1)), NoChanges),
            // pre-clank history: absent == absent → code only
            (false, None, None, CodeOnly),
            // post-clank, commit doesn't touch .clank
            (false, Some(oid(1)), Some(oid(1)), CodeOnly),
            // .clank introduced / removed / modified → real diff
            (false, None, Some(oid(1)), FullDiff),
            (false, Some(oid(1)), None, FullDiff),
            (false, Some(oid(1)), Some(oid(2)), FullDiff),
        ];
        for (roots_equal, parent, child, want) in cases {
            assert_eq!(
                diff_gate(roots_equal, parent, child),
                want,
                "roots_equal={roots_equal} parent={parent:?} child={child:?}"
            );
        }
    }

    #[test]
    fn git_dir_is_per_worktree_common_dir_is_shared() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        let main = tempfile::tempdir().unwrap();
        let m = main.path();
        git(m, &["init", "--quiet", "--initial-branch=main"]);
        git(m, &["config", "user.email", "t@t"]);
        git(m, &["config", "user.name", "t"]);
        std::fs::write(m.join("f.txt"), "x").unwrap();
        git(m, &["add", "-A"]);
        git(m, &["commit", "--quiet", "-m", "base"]);

        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join("wt");
        git(
            m,
            &[
                "worktree",
                "add",
                "--quiet",
                wt.to_str().unwrap(),
                "-b",
                "feat",
            ],
        );
        assert!(
            wt.join(".git").is_file(),
            "linked worktree `.git` is a file"
        );

        // common_dir is SHARED — identical from the main checkout and
        // the linked worktree.
        let common_m = common_dir(m).unwrap().canonicalize().unwrap();
        let common_wt = common_dir(&wt).unwrap().canonicalize().unwrap();
        assert_eq!(common_m, common_wt, "common dir is shared");

        // git_dir is PER-WORKTREE — differs, and the worktree's lives
        // under the common dir's `worktrees/`. (This is why hooks must
        // use common_dir but `clank-rewrite` uses git_dir.)
        let gd_m = git_dir(m).unwrap().canonicalize().unwrap();
        let gd_wt = git_dir(&wt).unwrap().canonicalize().unwrap();
        assert_ne!(gd_m, gd_wt, "per-worktree gitdir differs");
        assert!(
            gd_wt.starts_with(common_wt.join("worktrees")),
            "worktree gitdir under common/worktrees; got {gd_wt:?}"
        );
        assert!(git_dir(&wt).unwrap().is_absolute());
    }

    #[test]
    fn discover_work_dir_searches_upward_and_none_outside() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        // Upward search: from a nested subdir → the repo's work root
        // (matches `git rev-parse --show-toplevel`).
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let found = discover_work_dir(&sub).unwrap().unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            root.canonicalize().unwrap(),
            "discover from a subdir returns the toplevel"
        );
        // Outside any repo → None (caller bails / falls back).
        let outside = tempfile::tempdir().unwrap();
        assert!(discover_work_dir(outside.path()).unwrap().is_none());
    }

    #[test]
    fn config_bool_honors_git_bool_syntax() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet"]);
        let key = "branch.foo.protect";
        // Absent → None.
        assert_eq!(config_bool(r, key).unwrap(), None);
        // git's truthy/falsy spellings (the reason to use gix's parser
        // over a hand-rolled match).
        for truthy in ["true", "1", "yes", "on"] {
            git(r, &["config", key, truthy]);
            assert_eq!(config_bool(r, key).unwrap(), Some(true), "{truthy}");
        }
        for falsy in ["false", "0", "no", "off"] {
            git(r, &["config", key, falsy]);
            assert_eq!(config_bool(r, key).unwrap(), Some(false), "{falsy}");
        }
    }

    #[test]
    fn commit_subject_and_body_match_git() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("f"), "x").unwrap();
        git(r, &["add", "-A"]);
        // `-m subject -m body` → subject + body separated by a blank.
        git(
            r,
            &[
                "commit",
                "--quiet",
                "-m",
                "the subject",
                "-m",
                "line 1\nline 2",
            ],
        );

        let head = rev_parse_head(r).unwrap().unwrap();
        assert_eq!(commit_subject(r, &head).unwrap(), "the subject");
        assert_eq!(commit_body(r, &head).unwrap().trim(), "line 1\nline 2");
    }

    #[test]
    fn current_branch_reads_head_and_none_when_detached() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet", "--initial-branch=main"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("f"), "x").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "c"]);

        assert_eq!(current_branch(r).unwrap().as_deref(), Some("main"));

        // Detached HEAD → None (the old `symbolic-ref` exited non-zero).
        let head = rev_parse_head(r).unwrap().unwrap();
        git(r, &["checkout", "--quiet", head.as_str()]);
        assert_eq!(current_branch(r).unwrap(), None);
    }

    #[test]
    fn resolve_commit_resolves_revs_and_none_for_unknown() {
        use std::process::Command;
        fn git(dir: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet", "--initial-branch=main"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("f"), "x").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "c"]);

        let head = rev_parse_head(r).unwrap().unwrap();
        // HEAD, a branch ref, and a full sha all resolve to the commit.
        assert_eq!(resolve_commit(r, "HEAD").as_ref(), Some(&head));
        assert_eq!(resolve_commit(r, "refs/heads/main").as_ref(), Some(&head));
        assert_eq!(resolve_commit(r, head.as_str()).as_ref(), Some(&head));
        // Unresolvable revs → None (matches `--verify --quiet` exit 1).
        assert_eq!(resolve_commit(r, "refs/heads/nope"), None);
        assert_eq!(resolve_commit(r, "not-a-real-rev"), None);
    }

    mod walker_equivalence {
        //! The walker must produce byte-identical `CommitEvent`s to
        //! the legacy per-commit producer (`first_parent_commits_to`
        //! plus `diff_tree_changes`) — the legacy pair is kept as
        //! the reference implementation for exactly this test.
        use super::super::*;
        use std::path::Path;
        use std::process::Command;

        fn git(repo: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
                .output()
                .expect("git spawns");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn write(repo: &Path, rel: &str, body: &str) {
            let abs = repo.join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, body).unwrap();
        }

        fn commit(repo: &Path, msg: &str) {
            git(repo, &["add", "-A"]);
            git(repo, &["commit", "--quiet", "-m", msg]);
        }

        /// Synthetic history exercising every gate path and the
        /// boundary-crossing rename semantics the full-repo diff
        /// preserves: pre-clank code commits, clank intro, plan
        /// add/modify, mixed commit, rename within .clank
        /// (plans → finished), rename OUT of .clank, rename INTO
        /// .clank, empty commit, merge (first-parent).
        fn build_fixture() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            let r = dir.path();
            git(r, &["init", "--quiet", "-b", "main"]);
            git(r, &["config", "user.email", "t@t"]);
            git(r, &["config", "user.name", "t"]);

            // pre-clank era (root + one more code commit)
            write(r, "src/lib.rs", "// v1\n");
            commit(r, "code: root");
            write(r, "src/lib.rs", "// v2\n");
            commit(r, "code: change");
            // clank intro
            write(r, ".clank/config.json", "{}\n");
            commit(r, "clank: init");
            // plan intro
            write(r, ".clank/plans/alpha.md", "# alpha\n");
            commit(r, "[alpha] intro");
            // mixed plan + code commit
            write(r, ".clank/plans/alpha.md", "# alpha v2\n");
            write(r, "src/lib.rs", "// v3\n");
            commit(r, "[alpha] step 1");
            // code-only commit in the clank era
            write(r, "src/other.rs", "// other\n");
            commit(r, "code: unrelated");
            // empty commit
            git(r, &["commit", "--quiet", "--allow-empty", "-m", "empty"]);
            // rename within .clank: finalize (plans → finished)
            std::fs::create_dir_all(r.join(".clank/finished")).unwrap();
            git(
                r,
                &["mv", ".clank/plans/alpha.md", ".clank/finished/alpha.md"],
            );
            commit(r, "[alpha] finish");
            // rename OUT of .clank
            write(r, ".clank/plans/beta.md", "# beta\nsome body text here\n");
            commit(r, "[beta] intro");
            std::fs::create_dir_all(r.join("docs")).unwrap();
            git(r, &["mv", ".clank/plans/beta.md", "docs/beta.md"]);
            commit(r, "[beta] moved out");
            // rename INTO .clank
            git(r, &["mv", "docs/beta.md", ".clank/plans/beta.md"]);
            commit(r, "[beta] resurrected");
            // merge commit (first-parent semantics)
            git(r, &["checkout", "--quiet", "-b", "side", "HEAD~2"]);
            write(r, "src/side.rs", "// side\n");
            commit(r, "code: side branch");
            git(r, &["checkout", "--quiet", "main"]);
            git(
                r,
                &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
            );
            dir
        }

        fn legacy_events(repo: &Path, tip: &CommitSha) -> Vec<CommitEvent> {
            first_parent_commits_to(repo, tip)
                .unwrap()
                .into_iter()
                .map(|meta| CommitEvent {
                    changes: diff_tree_changes(repo, &meta.sha).unwrap(),
                    commit: meta.sha,
                    author_ts: meta.author_ts,
                    subject: meta.subject,
                })
                .collect()
        }

        fn head(repo: &Path) -> CommitSha {
            rev_parse_head(repo).unwrap().unwrap()
        }

        #[test]
        fn walker_matches_legacy_producer_from_root() {
            let dir = build_fixture();
            let tip = head(dir.path());
            let walked = commit_events_between_at(dir.path(), None, &tip).unwrap();
            let legacy = legacy_events(dir.path(), &tip);
            assert_eq!(walked.len(), legacy.len(), "same commit count");
            for (w, l) in walked.iter().zip(&legacy) {
                assert_eq!(w, l, "diverged at {} ({})", l.commit.as_str(), l.subject);
            }
        }

        #[test]
        fn walker_matches_legacy_producer_from_mid_range_base() {
            // Base mid-history: the walk's oldest commit must diff
            // against its REAL first parent (behind the hidden
            // base), not the empty tree.
            let dir = build_fixture();
            let tip = head(dir.path());
            let all = legacy_events(dir.path(), &tip);
            for start in [1, all.len() / 2, all.len() - 1] {
                let base = all[start - 1].commit.clone();
                let walked = commit_events_between_at(dir.path(), Some(&base), &tip).unwrap();
                assert_eq!(
                    walked,
                    all[start..],
                    "range fold from base at index {start}"
                );
            }
        }

        #[test]
        fn walker_base_equals_tip_is_empty() {
            let dir = build_fixture();
            let tip = head(dir.path());
            assert_eq!(
                commit_events_between_at(dir.path(), Some(&tip), &tip).unwrap(),
                Vec::new()
            );
        }
    }

    #[test]
    fn is_plan_path_active() {
        assert!(is_plan_path(&PathBuf::from(".clank/plans/foo.md")));
    }

    #[test]
    fn is_plan_path_rejects_done_subdir() {
        assert!(!is_plan_path(&PathBuf::from(".clank/plans/done/foo.md")));
    }

    #[test]
    fn is_not_plan_path_feedback() {
        assert!(!is_plan_path(&PathBuf::from(
            ".clank/feedback/foo/plan/alice.md"
        )));
    }

    #[test]
    fn is_not_plan_path_source() {
        assert!(!is_plan_path(&PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn is_not_plan_path_too_deep() {
        assert!(!is_plan_path(&PathBuf::from(".clank/plans/sub/foo.md")));
    }

    // The 10 `parse_diff_tree_*` scenario tests that used to live
    // here have been converted to integration tests building real
    // git repos; see `crates/cli/tests/diff_tree_changes_scenarios.rs`.
    // Same scenario names, same assertions — only the input shape
    // changes (real commits + diff_tree_changes call instead of
    // synthetic stdout + parse_diff_tree call).
}
