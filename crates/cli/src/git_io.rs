//! Git introspection for the filesystem-truth model. Narrowly-scoped
//! reads only — never writes. Functions here use `gix` (gitoxide)
//! programmatically; the subprocess + text-parse era is gone.

use std::path::{Path, PathBuf};

use crate::disk_format::parse_feedback_path;
use crate::disk_snapshot::{
    CommitChanges, CommitEvent, CommitSnapshot, FeedbackBlob, PlanTouch, PlanTouchKind,
};
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
    let parent_tree_ref = parent_tree_owned.as_ref();
    // Enable rename tracking with the git default 50% similarity
    // (matches legacy `-M` flag). diff_tree_to_tree with `None`
    // for options uses the repo's configured defaults, which may
    // have rewrites=None — so build the Options explicitly.
    let opts = gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
    let raw_changes = repo
        .diff_tree_to_tree(parent_tree_ref, Some(&this_tree), opts)
        .map_err(|e| GitIoError::NonZero {
            context,
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

/// Gather a `CommitSnapshot` for `repo_root`. IO half of the
/// commit-derived rebuild; `disk_snapshot::derive_base_state`
/// consumes the result as a feedback-blind fold.
///
/// Composes the migrated primitives:
/// 1. `rev_parse_head` (empty repo / unborn HEAD → empty snapshot).
/// 2. `first_parent_commits` for the oldest-first first-parent walk.
/// 3. `diff_tree_changes` per commit for the structured changes.
///
/// Working-tree feedback is gathered separately via
/// [`collect_feedback_files`] and applied by
/// `disk_snapshot::attach_live_feedback`.
pub fn snapshot(repo_root: &Path) -> Result<CommitSnapshot, GitIoError> {
    let head = rev_parse_head(repo_root)?;
    let Some(head) = head else {
        return Ok(CommitSnapshot::default());
    };

    let metas = first_parent_commits(repo_root)?;
    let mut history: Vec<CommitEvent> = Vec::with_capacity(metas.len());
    for meta in metas {
        let changes = diff_tree_changes(repo_root, &meta.sha)?;
        history.push(CommitEvent {
            commit: meta.sha,
            author_ts: meta.author_ts,
            subject: meta.subject,
            changes,
        });
    }

    Ok(CommitSnapshot {
        head: Some(head),
        history,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
