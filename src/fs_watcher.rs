//! Filesystem-watcher path routing. Pure: takes an absolute path + an
//! event kind, returns a structured `FilesystemSignal` (or `None` for
//! paths Trinity doesn't care about). The async wiring that connects
//! this to `notify::Watcher` lives elsewhere.

use std::path::{Path, PathBuf};

use crate::disk_format::{FeedbackPath, parse_feedback_path, session_id_from_plan_path};
use crate::lifecycle::SessionId;

/// Signals the watcher layer hands to the runner. The runner expands these
/// into `Observation` values (loading file bodies, looking up known
/// sessions, etc.) before applying them through the reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemSignal {
    /// A change to `.git/HEAD` or `.git/logs/HEAD` (in either the worktree
    /// or the linked-worktree gitdir). Triggers a full `RebuildRepo`.
    HeadChanged,

    /// A working-tree write/modify/remove of `.trinity/plans/<id>.md`
    /// (or the done-path counterpart). Used to recompute
    /// `plan_worktree_status` for already-discovered sessions. The runner
    /// looks up `session_id` against current state and drops the signal
    /// if there's no matching session (untracked draft).
    PlanFileChanged {
        session_id: SessionId,
        path: PathBuf,
    },

    /// A write/modify to a feedback file. The body must still be read by
    /// the runner; this signal only encodes the structured path.
    FeedbackWritten { parsed: FeedbackPath },

    /// A removal of a feedback file.
    FeedbackRemoved { parsed: FeedbackPath },
}

/// Kind of filesystem event observed by the underlying watcher. Maps to
/// `notify::EventKind` in the async wiring layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    CreatedOrModified,
    Removed,
}

/// Translate an absolute path + event kind into a structured signal.
/// Returns `None` for paths Trinity doesn't watch (anything outside
/// `<repo>/.trinity/` and `<repo>/.git/HEAD`-family).
pub fn path_to_signal(
    abs_path: &Path,
    repo_root: &Path,
    event_kind: FsEventKind,
) -> Option<FilesystemSignal> {
    let rel = abs_path.strip_prefix(repo_root).ok()?;

    // .git/HEAD or .git/logs/HEAD → HeadChanged (works for both regular
    // and linked-worktree gitdirs since the gitdir-resolution layer
    // hands us the resolved absolute path before stripping).
    if matches_head_file(rel) {
        return Some(FilesystemSignal::HeadChanged);
    }

    // .trinity/plans/<id>.md or .trinity/plans/done/<id>.md
    if let Some(signal) = plan_signal(rel, event_kind) {
        return Some(signal);
    }

    // .trinity/feedback/<session>/<plan|impl>/[<sha>/]<author>.md
    if let Ok(rest) = rel.strip_prefix(".trinity/feedback") {
        let parsed = parse_feedback_path(rest)?;
        return Some(match event_kind {
            FsEventKind::CreatedOrModified => FilesystemSignal::FeedbackWritten { parsed },
            FsEventKind::Removed => FilesystemSignal::FeedbackRemoved { parsed },
        });
    }

    None
}

/// True if `rel` is `.git/HEAD` or `.git/logs/HEAD` (within the worktree).
/// Linked-worktree gitdirs (where `.git` is a file) need the resolved
/// gitdir path to be passed in by the caller; that's handled in the
/// async wiring layer, which strips against the gitdir root and matches
/// `HEAD` or `logs/HEAD` directly.
fn matches_head_file(rel: &Path) -> bool {
    matches!(
        rel.to_str(),
        Some(".git/HEAD") | Some(".git/logs/HEAD") | Some("HEAD") | Some("logs/HEAD")
    )
}

fn plan_signal(rel: &Path, event_kind: FsEventKind) -> Option<FilesystemSignal> {
    let plans_rel = rel.strip_prefix(".trinity/plans").ok()?;
    // plans_rel is either `<id>.md` or `done/<id>.md`.
    let (under_done, name_seg) = {
        let mut comps = plans_rel.components();
        let first = comps.next()?;
        let first_str = first.as_os_str().to_str()?;
        if first_str == "done" {
            let second = comps.next()?;
            if comps.next().is_some() {
                return None; // too deep
            }
            (true, second.as_os_str().to_str()?.to_string())
        } else {
            if comps.next().is_some() {
                return None; // too deep
            }
            (false, first_str.to_string())
        }
    };
    let plan_rel = if under_done {
        Path::new(".trinity/plans/done").join(&name_seg)
    } else {
        Path::new(".trinity/plans").join(&name_seg)
    };
    let session_id = session_id_from_plan_path(&plan_rel)?;
    // Both create/modify and remove map to PlanFileChanged. The runner's
    // plan-worktree-status recompute correctly handles either case.
    let _ = event_kind;
    Some(FilesystemSignal::PlanFileChanged {
        session_id,
        path: plan_rel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        PathBuf::from("/r")
    }

    #[test]
    fn git_head_in_worktree() {
        let sig = path_to_signal(
            &repo().join(".git/HEAD"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, Some(FilesystemSignal::HeadChanged));
    }

    #[test]
    fn git_logs_head_in_worktree() {
        let sig = path_to_signal(
            &repo().join(".git/logs/HEAD"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, Some(FilesystemSignal::HeadChanged));
    }

    #[test]
    fn linked_worktree_gitdir_head() {
        // Caller strips the gitdir root and passes us a path relative to it.
        let gitdir = PathBuf::from("/g/.git/worktrees/wt1");
        let abs = gitdir.join("HEAD");
        let sig = path_to_signal(&abs, &gitdir, FsEventKind::CreatedOrModified);
        assert_eq!(sig, Some(FilesystemSignal::HeadChanged));
    }

    #[test]
    fn active_plan_file_created() {
        let sig = path_to_signal(
            &repo().join(".trinity/plans/foo.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        match sig {
            Some(FilesystemSignal::PlanFileChanged { session_id, path }) => {
                assert_eq!(session_id.as_str(), "foo");
                assert_eq!(path, PathBuf::from(".trinity/plans/foo.md"));
            }
            other => panic!("expected PlanFileChanged, got {other:?}"),
        }
    }

    #[test]
    fn done_plan_file_emits_signal_with_done_path() {
        let sig = path_to_signal(
            &repo().join(".trinity/plans/done/foo.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        match sig {
            Some(FilesystemSignal::PlanFileChanged { session_id, path }) => {
                assert_eq!(session_id.as_str(), "foo");
                assert_eq!(path, PathBuf::from(".trinity/plans/done/foo.md"));
            }
            other => panic!("expected PlanFileChanged, got {other:?}"),
        }
    }

    #[test]
    fn plan_file_removed_still_emits_plan_changed() {
        // Removing the active path is one of the worktree-state transitions
        // (e.g. `mv` to done/ without committing → triggers DoneMovePending
        // on next status recompute).
        let sig = path_to_signal(
            &repo().join(".trinity/plans/foo.md"),
            &repo(),
            FsEventKind::Removed,
        );
        assert!(matches!(
            sig,
            Some(FilesystemSignal::PlanFileChanged { .. })
        ));
    }

    #[test]
    fn plan_subdir_not_a_plan_file() {
        let sig = path_to_signal(
            &repo().join(".trinity/plans/nested/foo.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, None);
    }

    #[test]
    fn plan_non_md_file_not_signaled() {
        let sig = path_to_signal(
            &repo().join(".trinity/plans/foo.txt"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, None);
    }

    #[test]
    fn feedback_canonical_sha_path() {
        let sig = path_to_signal(
            &repo().join(".trinity/feedback/foo/plan/abc1234/alice.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        match sig {
            Some(FilesystemSignal::FeedbackWritten { parsed }) => {
                assert_eq!(parsed.session_id.as_str(), "foo");
                assert_eq!(parsed.author.as_str(), "alice");
                assert_eq!(parsed.target_sha.unwrap().as_str(), "abc1234");
            }
            other => panic!("expected FeedbackWritten, got {other:?}"),
        }
    }

    #[test]
    fn feedback_flat_path() {
        let sig = path_to_signal(
            &repo().join(".trinity/feedback/foo/plan/alice.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        match sig {
            Some(FilesystemSignal::FeedbackWritten { parsed }) => {
                assert!(parsed.target_sha.is_none());
            }
            other => panic!("expected FeedbackWritten, got {other:?}"),
        }
    }

    #[test]
    fn feedback_removed() {
        let sig = path_to_signal(
            &repo().join(".trinity/feedback/foo/impl/def5678/bob.md"),
            &repo(),
            FsEventKind::Removed,
        );
        assert!(matches!(
            sig,
            Some(FilesystemSignal::FeedbackRemoved { .. })
        ));
    }

    #[test]
    fn non_trinity_path_is_ignored() {
        let sig = path_to_signal(
            &repo().join("src/lib.rs"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, None);
    }

    #[test]
    fn cache_dir_is_ignored() {
        let sig = path_to_signal(
            &repo().join(".trinity/cache/foo.bin"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, None);
    }

    #[test]
    fn outside_repo_root_is_ignored() {
        let sig = path_to_signal(
            &PathBuf::from("/elsewhere/.trinity/plans/foo.md"),
            &repo(),
            FsEventKind::CreatedOrModified,
        );
        assert_eq!(sig, None);
    }
}
