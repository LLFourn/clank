//! The stash transaction, shared by push and pop: build the whole
//! result in the object database, check it against the working tree,
//! then publish by compare-and-swap. Nothing a ref, the index or the
//! working tree can see changes until every check has passed, and a
//! publish that cannot finish is moved back.
//!
//! Push and pop own their own history rewrite rather than using
//! `cli::rewrite`, which keeps a commit by its full tree and publishes
//! with a hard reset: right for purge and finish, wrong for setting a
//! plan aside under someone else's commits and uncommitted work.

use std::path::Path;

use crate::git_plumbing::{self as plumbing, ExpectedRef, Moved, Replayed};

/// What a transaction read before building, and reads again before it
/// publishes: other agents and editors do not take clank's locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    /// The ref HEAD is attached to, or `None` when it is detached.
    pub(crate) binding: Option<String>,
    pub(crate) head: String,
    pub(crate) index: Option<String>,
    /// The tree of the working tree's tracked state.
    pub(crate) tracked: String,
}

impl Capture {
    pub(crate) fn read(repo: &Path) -> anyhow::Result<Self> {
        let binding = crate::git_io::head_binding_at(repo)?;
        let head = crate::git_io::rev_parse_head(repo)?
            .ok_or_else(|| anyhow::anyhow!("this repository has no commit"))?
            .as_str()
            .to_string();
        let index = crate::git_io::index_digest_at(repo)?;
        let tracked = plumbing::tracked_tree(repo)?;
        Ok(Self {
            binding,
            head,
            index,
            tracked,
        })
    }

    fn on(&self) -> String {
        match &self.binding {
            Some(name) => format!("`{}`", name.trim_start_matches("refs/heads/")),
            None => "a detached HEAD".into(),
        }
    }
}

/// What reading the inputs again found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fence {
    Same,
    /// Staged or tracked work changed: build again with it.
    Newer,
    /// What HEAD is, or where it points, changed: the operator moved.
    Moved(String),
}

pub(crate) fn fence(repo: &Path, captured: &Capture) -> anyhow::Result<Fence> {
    let now = Capture::read(repo)?;
    if now.binding != captured.binding {
        return Ok(Fence::Moved(format!(
            "HEAD moved from {} to {} while stashing",
            captured.on(),
            now.on()
        )));
    }
    if now.head != captured.head {
        return Ok(Fence::Moved(format!(
            "{} moved from {} to {} while stashing",
            captured.on(),
            short(&captured.head),
            short(&now.head)
        )));
    }
    if now.index != captured.index || now.tracked != captured.tracked {
        return Ok(Fence::Newer);
    }
    Ok(Fence::Same)
}

pub(crate) fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// The automatic commit of the captured tracked state, on HEAD and on
/// no ref; `None` when that state is HEAD's own tree.
pub(crate) fn snapshot(
    repo: &Path,
    captured: &Capture,
    message: &str,
) -> anyhow::Result<Option<String>> {
    if plumbing::commit_tree_oid(repo, &captured.head)? == captured.tracked {
        return Ok(None);
    }
    plumbing::commit_new(repo, &captured.tracked, Some(&captured.head), message).map(Some)
}

/// One commit replayed onto the new history.
#[derive(Debug, Clone)]
pub(crate) struct Replay {
    pub(crate) original: String,
    pub(crate) new: String,
    /// A merge, now one ordinary commit carrying its first-parent change.
    pub(crate) linearized: bool,
}

pub(crate) enum Built {
    /// The new tip, or `None` when nothing was replayed onto no base.
    Tip {
        tip: Option<String>,
        replayed: Vec<Replay>,
    },
    Conflict {
        sha: String,
        paths: Vec<String>,
    },
}

/// Replay each of `commits`, oldest first, as its change against its
/// first parent, onto `base` (`None`: from nothing, so the first is a
/// root). Each keeps its message and author.
pub(crate) fn replay_onto(
    repo: &Path,
    base: Option<&str>,
    commits: &[String],
) -> anyhow::Result<Built> {
    let mut tip = base.map(str::to_string);
    let mut tree = match &tip {
        Some(sha) => plumbing::commit_tree_oid(repo, sha)?,
        None => plumbing::empty_tree(repo)?,
    };
    let mut replayed = Vec::with_capacity(commits.len());
    for sha in commits {
        match plumbing::replay_change(repo, sha, &tree)? {
            Replayed::Conflicts(paths) => {
                return Ok(Built::Conflict {
                    sha: sha.clone(),
                    paths,
                });
            }
            Replayed::Tree(next) => {
                let new = plumbing::replay_commit(repo, sha, &next, tip.as_deref())?;
                let parents = crate::git_io::commit_parent_count_at(
                    repo,
                    &clank_core::ids::CommitSha::parse(sha)
                        .map_err(|e| anyhow::anyhow!("sha `{sha}`: {e}"))?,
                )?;
                replayed.push(Replay {
                    original: sha.clone(),
                    new: new.clone(),
                    linearized: parents > 1,
                });
                tree = next;
                tip = Some(new);
            }
        }
    }
    Ok(Built::Tip { tip, replayed })
}

/// What stands in the way of moving the working tree from `from_tree`
/// to `tip`'s tree, one line per path; empty when nothing does.
pub(crate) fn preflight(repo: &Path, from_tree: &str, tip: &str) -> anyhow::Result<Vec<String>> {
    let to_tree = plumbing::commit_tree_oid(repo, tip)?;
    let mut lines = in_the_way(repo, from_tree, &to_tree)?;
    if let Moved::Refused(more) = plumbing::move_worktree(repo, from_tree, &to_tree, true)? {
        lines.extend(more);
    }
    Ok(lines)
}

/// What is on disk where `to_tree` adds a path `from_tree` does not
/// have. The checkout's own guard leaves out ignored files, which it
/// would overwrite, so every such entry is checked here: a file or
/// symlink at the path, untracked or ignored content in a directory at
/// it, or a file at one of its parent directories.
fn in_the_way(repo: &Path, from_tree: &str, to_tree: &str) -> anyhow::Result<Vec<String>> {
    let had = crate::git_io::tree_files_at(repo, from_tree)?;
    let will = crate::git_io::tree_files_at(repo, to_tree)?;
    let mut found = std::collections::BTreeSet::new();
    for path in will.difference(&had) {
        match std::fs::symlink_metadata(repo.join(path)) {
            Ok(meta) if meta.is_dir() => {
                for inside in files_under(repo, path) {
                    if !had.contains(&inside) {
                        found.insert(format!(
                            "{inside} is not tracked, and {path} would replace it"
                        ));
                    }
                }
            }
            Ok(_) => {
                found.insert(format!("{path} is not tracked, and would be overwritten"));
            }
            Err(_) => {}
        }
        let mut parent = Path::new(path).parent();
        while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
            let rel = dir.to_string_lossy().into_owned();
            if !had.contains(&rel)
                && std::fs::symlink_metadata(repo.join(dir)).is_ok_and(|m| !m.is_dir())
            {
                found.insert(format!(
                    "{rel} is not tracked, and {path} needs it to be a directory"
                ));
            }
            parent = dir.parent();
        }
    }
    Ok(found.into_iter().collect())
}

/// Every non-directory entry under `rel`, relative to `repo`.
fn files_under(repo: &Path, rel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(repo.join(&dir)) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let child = format!("{dir}/{}", entry.file_name().to_string_lossy());
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(child),
                _ => out.push(child),
            }
        }
    }
    out
}

/// How publishing ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Published {
    Done,
    /// Done, but someone staged into the index meanwhile, and their
    /// index was left as they made it.
    IndexKept,
    /// Nothing is published and nothing was written: the branch never
    /// moved, or was moved back. The lines say what was in the way.
    RolledBack(Vec<String>),
    /// The working tree may be partly written, so it is neither the old
    /// state nor the new, and everything the caller protected must stay
    /// protected. `at_tip`: HEAD is at the new tip; otherwise it is
    /// still at the captured commit.
    Partial {
        lines: Vec<String>,
        at_tip: bool,
    },
}

/// Under git's HEAD lock, held throughout so HEAD cannot be switched,
/// attached or detached meanwhile: check that HEAD still names what was
/// captured, move the captured branch (or the detached HEAD) from the
/// captured commit to `tip`, move the working tree from `from_tree` to
/// `tip`'s tree, then install the index. A working tree that refuses the
/// move before writing anything puts the branch back. An error is
/// returned only while nothing has moved.
pub(crate) fn publish(
    repo: &Path,
    captured: &Capture,
    from_tree: &str,
    tip: &str,
) -> anyhow::Result<Published> {
    let to_tree = plumbing::commit_tree_oid(repo, tip)?;
    let blocked = in_the_way(repo, from_tree, &to_tree)?;
    if !blocked.is_empty() {
        return Ok(Published::RolledBack(blocked));
    }
    let mut head = match plumbing::HeadLock::take(repo) {
        Ok(head) => head,
        Err(e) => return Ok(Published::RolledBack(vec![format!("{e:#}")])),
    };
    if !head.names(captured.binding.as_deref(), &captured.head)? {
        return Ok(Published::RolledBack(vec![format!(
            "HEAD is no longer {} at {}",
            captured.on(),
            short(&captured.head)
        )]));
    }
    match &captured.binding {
        Some(branch) => {
            if let Err(e) =
                plumbing::update_ref(repo, branch, tip, ExpectedRef::Match(captured.head.clone()))
            {
                return Ok(Published::RolledBack(vec![format!(
                    "{} did not move to {}: {e:#}",
                    captured.on(),
                    short(tip)
                )]));
            }
        }
        None => head.point_at(tip),
    }
    // From here every failure is `Partial`, never an error: the branch
    // may have moved and the working tree may be written, so nothing the
    // caller protected may be released on it.
    let partial = |mut lines: Vec<String>, head: plumbing::HeadLock| {
        let at_tip = match head.commit() {
            Ok(()) => true,
            Err(e) => {
                lines.push(format!("HEAD could not be moved: {e:#}"));
                captured.binding.is_some()
            }
        };
        Published::Partial { lines, at_tip }
    };
    match plumbing::move_worktree(repo, from_tree, &to_tree, false) {
        Ok(Moved::Done(index)) => {
            if let Err(e) = head.commit() {
                return Ok(Published::Partial {
                    lines: vec![format!("HEAD could not be moved: {e:#}")],
                    at_tip: captured.binding.is_some(),
                });
            }
            Ok(
                match plumbing::install_index(repo, index, captured.index.as_deref()) {
                    Ok(true) => Published::Done,
                    Ok(false) => Published::IndexKept,
                    Err(e) => Published::Partial {
                        lines: vec![format!("the index could not be written: {e:#}")],
                        at_tip: true,
                    },
                },
            )
        }
        Ok(Moved::Refused(lines)) => match &captured.binding {
            Some(branch) => match plumbing::update_ref(
                repo,
                branch,
                &captured.head,
                ExpectedRef::Match(tip.into()),
            ) {
                Ok(()) => Ok(Published::RolledBack(lines)),
                Err(e) => {
                    let mut lines = lines;
                    lines.push(format!(
                        "{} could not be put back at {}: {e:#}",
                        captured.on(),
                        short(&captured.head)
                    ));
                    Ok(Published::Partial {
                        lines,
                        at_tip: true,
                    })
                }
            },
            None => Ok(Published::RolledBack(lines)),
        },
        Ok(Moved::Partial(lines)) => Ok(partial(lines, head)),
        Ok(Moved::Clear) => unreachable!("a real move is never a dry run"),
        Err(e) => Ok(partial(vec![format!("{e:#}")], head)),
    }
}

/// The untracked files outside `.clank/` a publish leaves where they are.
pub(crate) fn untracked_left(repo: &Path) -> Vec<String> {
    crate::git_io::working_tree_status(repo)
        .map(|s| {
            s.untracked
                .into_iter()
                .filter(|p| !p.starts_with(".clank/") && p != ".clank")
                .collect()
        })
        .unwrap_or_default()
}
