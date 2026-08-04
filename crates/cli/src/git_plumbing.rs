//! gix-backed git PLUMBING — object/tree/ref writes, the mutation
//! counterpart to [`crate::git_io`]'s reads (which stay read-only for
//! the filesystem-truth model). The history-rewrite engine (`purge` /
//! `finish --purge`) and `stash` build trees, write commit objects,
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

/// Write THE finalize commit for `stem` as a DANGLING object: `head_sha`'s
/// tree with `.clank/plans/<stem>.md` MOVED to `.clank/finished/<stem>.md`
/// (same blob; an empty blob when the plan file is absent — the empty-marker
/// fallback), parent = `head_sha`, ambient identity, `message`.
///
/// This is the ONE builder for finalize commits (ruthless 3bd7882): the live
/// `finish` refs the returned commit; `--dry` leaves it dangling and previews
/// over it. One implementation, so preview and execution cannot drift.
/// Building from HEAD's tree (not the index) also means unrelated STAGED
/// changes are never swept into the finalize commit.
pub fn write_finalize_commit(
    repo: &Path,
    head_sha: &str,
    stem: &str,
    message: &str,
) -> anyhow::Result<String> {
    let r = open(repo)?;
    let tree_id = r
        .find_commit(parse_oid(head_sha)?)
        .with_context(|| format!("find commit `{head_sha}`"))?
        .tree_id()
        .with_context(|| format!("tree of `{head_sha}`"))?;
    let plan_rel = crate::init_facts::plan_md_rel(stem);
    let finished_rel = crate::init_facts::finished_md_rel(stem);
    let blob = match r
        .find_tree(tree_id)
        .context("find head tree")?
        .lookup_entry_by_path(&plan_rel)
        .with_context(|| format!("lookup `{plan_rel}`"))?
    {
        Some(entry) => entry.oid().to_owned(),
        None => r
            .write_blob([])
            .context("write empty marker blob")?
            .detach(),
    };
    let mut editor = r
        .edit_tree(tree_id)
        .with_context(|| format!("edit tree of `{head_sha}`"))?;
    editor
        .remove(plan_rel.as_str())
        .with_context(|| format!("remove `{plan_rel}`"))?;
    editor
        .upsert(
            finished_rel.as_str(),
            gix::object::tree::EntryKind::Blob,
            blob,
        )
        .with_context(|| format!("add `{finished_rel}`"))?;
    let new_tree = editor.write().context("write finalize-preview tree")?;
    let sig = ambient_committer(&r)?;
    write_commit(
        &r,
        &new_tree.detach().to_string(),
        Some(head_sha),
        sig.clone(),
        sig,
        message.into(),
    )
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

// ── subprocess mutations ──
// gix can't (yet) do these — worktree management, network fetch,
// working-tree checkout/cherry-pick, and the index/commit operations
// the history-rewrite engine drives. They stay `git` subprocesses, but
// live HERE behind the boundary so callers never spawn `git` directly.

use std::process::Command;

/// Run `git -C <repo> <args>`, erroring on non-zero exit. PRIVATE — the
/// shared spawn helper for the typed operations below; callers use the
/// typed fns so they never construct raw git argv (gix-not-git-gate).
fn run(repo: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .with_context(|| format!("spawning git {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!(
            "git {} failed (exit {})",
            args.join(" "),
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// `git reset --hard --quiet <rev>` — re-sync the worktree to `rev`. A
/// worktree-state checkout whose exact gitignore/fileMode/autocrlf
/// semantics must match git's, so it stays a subprocess.
/// Cherry-pick `shas` (in order) into the index/worktree WITHOUT
/// committing (`-n`) — the accumulation primitive behind pick's
/// squash/purge modes. Verified against live git
/// (pick-purge-and-squash study): git natively accepts a multi-commit
/// `-n` sequence AND stacks further `-n` picks onto an already-staged
/// index, and a mid-sequence conflict leaves git's cherry-pick state
/// where `--abort` restores the PRE-SEQUENCE state. Returns false on
/// conflict (state left for the user, same contract as
/// [`cherry_pick`]). Why not gix: gix has no cherry-pick; the 3-way
/// merge orchestration is exactly what the subprocess provides.
pub fn cherry_pick_no_commit(repo: &Path, shas: &[&str]) -> anyhow::Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cherry-pick", "-n"])
        .args(shas)
        .status()
        .context("spawning git cherry-pick -n")?;
    Ok(status.success())
}

/// The tree of the CURRENT index (`git write-tree`). Why not gix:
/// writing a tree from the live on-disk index (with whatever
/// extensions a cherry-pick left in it) is the subprocess's job; gix's
/// index-to-tree write path isn't proven on linked worktrees.
pub fn write_index_tree(repo: &Path) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("write-tree")
        .output()
        .context("spawning git write-tree")?;
    if !out.status.success() {
        anyhow::bail!(
            "git write-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn reset_hard(repo: &Path, rev: &str) -> anyhow::Result<()> {
    run(repo, &["reset", "--hard", "--quiet", rev])
}

/// `git rm --cached -q -- <pathspec>` — drop a path from the index only
/// (used by `purge --amend` to strip Clank artifacts from HEAD's tree).
pub fn remove_cached(repo: &Path, pathspec: &str) -> anyhow::Result<()> {
    run(repo, &["rm", "--cached", "-q", "--", pathspec])
}

/// `git rm --quiet --force -- <pathspec>` — remove a tracked path from
/// both index and worktree.
pub fn remove_path(repo: &Path, pathspec: &str) -> anyhow::Result<()> {
    run(repo, &["rm", "--quiet", "--force", "--", pathspec])
}

/// `git add -- <pathspec>` — stage a path.
pub fn stage(repo: &Path, pathspec: &str) -> anyhow::Result<()> {
    run(repo, &["add", "--", pathspec])
}

/// `git commit --quiet -m <msg>`, optionally `--amend`. Commits the
/// staged index (no pathspec).
pub fn commit(repo: &Path, msg: &str, amend: bool) -> anyhow::Result<()> {
    let mut args = vec!["commit", "--quiet", "-m", msg];
    if amend {
        args.push("--amend");
    }
    run(repo, &args)
}

/// `git commit --quiet -- <pathspec> -m <msg>` — commit exactly the
/// given path (used to promote a queued plan file).
pub fn commit_pathspec(repo: &Path, pathspec: &str, msg: &str) -> anyhow::Result<()> {
    run(repo, &["commit", "--quiet", pathspec, "-m", msg])
}

/// `git commit --amend --no-edit --allow-empty` — re-commit HEAD's
/// (possibly now-empty) tree keeping its message, after an index edit.
pub fn amend_no_edit(repo: &Path) -> anyhow::Result<()> {
    run(repo, &["commit", "--amend", "--no-edit", "--allow-empty"])
}

/// `git clone --quiet <source> <dest>` then `git -C <dest> checkout
/// --quiet -b <name> <base_sha>` — a full LOCAL clone (independent
/// config/remotes; `origin` = the source path) pinned to a branch at
/// an explicit sha. gix has a clone API, but the local-transport
/// clone + fresh-checkout path isn't proven here yet; subprocess
/// like [`worktree_add`].
pub fn clone_local(source: &Path, dest: &Path, name: &str, base_sha: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg(source)
        .arg(dest)
        .output()
        .context("spawning git clone")?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["checkout", "--quiet", "-b", name, base_sha])
        .output()
        .context("spawning git checkout in the clone")?;
    if !out.status.success() {
        // This call created `dest` (the caller cloned into a path it
        // verified absent) — remove it, or the next invocation finds
        // a descriptor-less foreign dir and can never retry.
        let _ = std::fs::remove_dir_all(dest);
        anyhow::bail!(
            "git checkout -b {name} {base_sha} failed in the clone (is the base \
             reachable from the main repo's refs? a detached or unpushed-ref base \
             can't ride a clone; the partial clone was removed): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git worktree add -b <name> <dest> <base>` — gix has no worktree
/// creation.
pub fn worktree_add(source: &Path, name: &str, dest: &Path, base: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["worktree", "add", "-b", name])
        .arg(dest)
        .arg(base)
        .output()
        .context("spawning git worktree add")?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Raw `git worktree list --porcelain` stdout — callers parse the
/// `worktree <path>` lines. gix's linked-worktree enumeration is
/// insufficient, so this stays git.
pub fn worktree_list_porcelain(repo: &Path) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("spawning git worktree list")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git worktree list` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git fetch origin <refspec>`. gix CAN fetch, but only with its
/// network+TLS features (a real dependency/binary-size cost) AND
/// explicit credential-helper wiring; the subprocess inherits the
/// user's git credentials for free. So this is a deliberate cost/auth
/// call, not a gix limitation — reconsiderable if we take on gix net.
pub fn fetch_refspec(source: &Path, refspec: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["fetch", "origin", refspec])
        .output()
        .context("spawning git fetch")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git fetch origin {refspec}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git checkout -b <branch> <sha>` — working-tree checkout stays git.
pub fn checkout_new_branch(repo: &Path, branch: &str, sha: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["checkout", "-b", branch, sha])
        .output()
        .context("spawning git checkout")?;
    if !out.status.success() {
        anyhow::bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git cherry-pick --allow-empty <sha>`. Returns `false` on conflict
/// (git leaves the cherry-pick in progress for the caller to resolve),
/// `true` on success. Working-tree replay stays git.
pub fn cherry_pick(repo: &Path, sha: &str) -> anyhow::Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cherry-pick", "--allow-empty", sha])
        .status()
        .context("spawning git cherry-pick")?;
    Ok(status.success())
}
