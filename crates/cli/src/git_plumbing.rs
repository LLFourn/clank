//! gix-backed git PLUMBING — object/tree/ref writes, the mutation
//! counterpart to [`crate::git_io`]'s reads (which stay read-only for
//! the filesystem-truth model). The history-rewrite engine (`purge` /
//! `finish --purge`) and `shelve` build trees, write commit objects,
//! and move refs through this typed API instead of scattering `gix::`
//! calls. A read appears here only when it's an inseparable input to a
//! write (e.g. an original commit's author for a replay).

use std::path::Path;

use anyhow::Context as _;

fn open(repo: &Path) -> anyhow::Result<gix::Repository> {
    gix::open(repo).with_context(|| format!("gix open `{}`", repo.display()))
}

fn parse_oid(sha: &str) -> anyhow::Result<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes()).with_context(|| format!("parse sha `{sha}`"))
}

/// The SHA of `<sha>`'s tree — replaces `git rev-parse <sha>^{tree}`.
pub fn commit_tree_oid(repo: &Path, sha: &str) -> anyhow::Result<String> {
    let r = open(repo)?;
    Ok(r.find_commit(parse_oid(sha)?)
        .with_context(|| format!("find commit `{sha}`"))?
        .tree_id()
        .with_context(|| format!("tree of `{sha}`"))?
        .detach()
        .to_string())
}

/// Build a new tree from `<commit_sha>`'s tree with each of
/// `strip_paths` removed — edited IN MEMORY (no scratch index, no
/// `GIT_INDEX_FILE`, so nothing can ENOTDIR on a linked worktree's
/// `.git` file). A path absent from the tree is a no-op.
pub fn strip_tree(repo: &Path, commit_sha: &str, strip_paths: &[String]) -> anyhow::Result<String> {
    let r = open(repo)?;
    let tree_id = r
        .find_commit(parse_oid(commit_sha)?)
        .with_context(|| format!("find commit `{commit_sha}`"))?
        .tree_id()
        .with_context(|| format!("tree of `{commit_sha}`"))?;
    let mut editor = r
        .edit_tree(tree_id)
        .with_context(|| format!("edit tree of `{commit_sha}`"))?;
    for path in strip_paths {
        editor
            .remove(path.as_str())
            .with_context(|| format!("strip `{path}`"))?;
    }
    Ok(editor
        .write()
        .context("write stripped tree")?
        .detach()
        .to_string())
}

/// Write a commit that REPLAYS `<original_sha>`'s identity onto a new
/// `tree_sha` + `parent`: the original AUTHOR is preserved exactly,
/// the COMMITTER is the ambient configured identity + now (a rewrite
/// is a fresh commit event), and the original message is kept verbatim.
pub fn replay_commit(
    repo: &Path,
    original_sha: &str,
    tree_sha: &str,
    parent: Option<&str>,
) -> anyhow::Result<String> {
    let r = open(repo)?;
    let orig = r
        .find_commit(parse_oid(original_sha)?)
        .with_context(|| format!("find commit `{original_sha}`"))?;
    let author = orig
        .author()
        .with_context(|| format!("author of `{original_sha}`"))?
        .to_owned()?;
    let message = orig
        .message_raw()
        .with_context(|| format!("message of `{original_sha}`"))?
        .to_owned();
    let committer = ambient_committer(&r)?;
    write_commit(&r, tree_sha, parent, author, committer, message)
}

/// Write a SQUASH commit: `<source_sha>`'s AUTHOR is preserved, the
/// COMMITTER is the configured identity with its DATE PINNED to the
/// author date (so re-squashing reproduces the same sha —
/// idempotent), and `message` is the supplied summary.
pub fn squash_commit(
    repo: &Path,
    source_sha: &str,
    tree_sha: &str,
    parent: Option<&str>,
    message: &str,
) -> anyhow::Result<String> {
    let r = open(repo)?;
    let source = r
        .find_commit(parse_oid(source_sha)?)
        .with_context(|| format!("find commit `{source_sha}`"))?;
    let author = source
        .author()
        .with_context(|| format!("author of `{source_sha}`"))?
        .to_owned()?;
    // Configured committer, but with its date pinned to the author
    // date (gix::date::Time is Copy, so reusing it leaves `author`
    // intact for the move below).
    let cfg = ambient_committer(&r)?;
    let committer = gix::actor::Signature {
        name: cfg.name,
        email: cfg.email,
        time: author.time,
    };
    write_commit(&r, tree_sha, parent, author, committer, message.into())
}

/// The configured committer identity (config name/email + now), as an
/// owned signature. Errors when no identity is configured.
fn ambient_committer(r: &gix::Repository) -> anyhow::Result<gix::actor::Signature> {
    Ok(r.committer()
        .ok_or_else(|| anyhow::anyhow!("no committer identity (set user.name / user.email)"))?
        .context("committer time")?
        .to_owned()?)
}

fn write_commit(
    r: &gix::Repository,
    tree_sha: &str,
    parent: Option<&str>,
    author: gix::actor::Signature,
    committer: gix::actor::Signature,
    message: gix::bstr::BString,
) -> anyhow::Result<String> {
    let parents = parent.map(parse_oid).transpose()?.into_iter().collect();
    let commit = gix::objs::Commit {
        tree: parse_oid(tree_sha)?,
        parents,
        author,
        committer,
        encoding: None,
        message,
        extra_headers: Vec::new(),
    };
    Ok(r.write_object(&commit)
        .context("write commit")?
        .detach()
        .to_string())
}

/// Precondition for [`update_ref`] — clank-native so callers never
/// touch gix's `PreviousValue`.
pub enum ExpectedRef {
    /// The ref must NOT already exist (atomic create-or-fail).
    CreateOnly,
    /// The ref must currently point at this sha — refuse if it moved.
    Match(String),
    /// No precondition — set the ref unconditionally.
    Any,
}

/// Point `full_name` (e.g. `refs/heads/foo`) at `new_sha` under the
/// `expected` precondition — gix's atomic ref transaction, replacing
/// `git update-ref <ref> <new> <old>`. Errors on a precondition
/// mismatch (ref exists / moved).
pub fn update_ref(
    repo: &Path,
    full_name: &str,
    new_sha: &str,
    expected: ExpectedRef,
) -> anyhow::Result<()> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    let previous = match expected {
        ExpectedRef::CreateOnly => PreviousValue::MustNotExist,
        ExpectedRef::Match(old) => {
            PreviousValue::MustExistAndMatch(gix::refs::Target::Object(parse_oid(&old)?))
        }
        ExpectedRef::Any => PreviousValue::Any,
    };
    let r = open(repo)?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "clank rewrite".into(),
            },
            expected: previous,
            new: gix::refs::Target::Object(parse_oid(new_sha)?),
        },
        name: full_name
            .try_into()
            .with_context(|| format!("ref name `{full_name}`"))?,
        deref: false,
    };
    r.edit_reference(edit)
        .with_context(|| format!("update `{full_name}`"))?;
    Ok(())
}

/// Delete `full_name` — gix ref transaction, replacing `git
/// update-ref -d <ref>`. Errors if the ref doesn't exist.
pub fn delete_ref(repo: &Path, full_name: &str) -> anyhow::Result<()> {
    use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
    let r = open(repo)?;
    let edit = RefEdit {
        change: Change::Delete {
            expected: PreviousValue::Any,
            log: RefLog::AndReference,
        },
        name: full_name
            .try_into()
            .with_context(|| format!("ref name `{full_name}`"))?,
        deref: false,
    };
    r.edit_reference(edit)
        .with_context(|| format!("delete `{full_name}`"))?;
    Ok(())
}
