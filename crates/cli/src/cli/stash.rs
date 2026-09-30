//! `clank stash` — set an in-flight plan's commits aside (bare, or
//! `push`) and restore them later (`pop`), with `show` / `drop` /
//! `list`. The verbs mirror `git stash`, bare-is-push included
//! (rename-shelve-to-stash, stash-does-what-you-mean; formerly
//! `clank shelve`/`unshelve`, kept as hidden aliases for a release).
//!
//! Push order is the data-safety invariant: the protective ref
//! (`refs/clank/stash/<plan>`) is written BEFORE the rewrite drops
//! anything, so the plan's commits stay reachable and GC-protected
//! even if every later step fails. Pop cherry-picks the recorded shas
//! back; reviews RESET by design (new shas, new context — the fold
//! derives Unreviewed and reviewers re-review; nothing migrates).
//!
//! STORAGE — read both, write new: records land at
//! `.clank/stash/<stem>.json` + `refs/clank/stash/<stem>`; legacy
//! `.clank/shelved/` records (+ their `refs/clank/shelved/` refs)
//! stay readable for a release. THE REF-PATH CONTRACT (codex d633f87):
//! every reader resolves an item through the merged scan and uses the
//! RECORD'S OWN `git_ref` — never a stem-constructed path.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use crate::lifecycle::PlanKey;
use crate::rebuild::CachePolicy;
use clank_core::api::{RewriteCommit, RewriteDisposition};
use clank_core::ids::CommitSha;

use super::{
    ShelveArgs, ShelveCmd, StashArgs, StashCmd, StashPushArgs, StashShowArgs, UnshelveArgs,
};

/// On-disk stash record — `.clank/stash/<plan>.json` for new pushes,
/// `.clank/shelved/<plan>.json` for legacy ones (read-both). The wire
/// shape is unchanged from the shelve era, so legacy records parse.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StashRecord {
    /// The plan's attributed commit shas, oldest first — the
    /// cherry-pick order for pop.
    pub shas: Vec<String>,
    /// The protective ref keeping those commits reachable.
    pub git_ref: String,
    /// Optional "stashed waiting on this plan" dependency; powers
    /// the `clank status` nudge once that plan finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

/// Exclusive and BLOCKING over a repo's stash directory.
///
/// A stash is TWO objects — a protective ref and a record file — and
/// every verb here mutates both. Without one lock over the pair, the
/// window between them is a stash somebody else can be halfway
/// through: a drop that had verified its record could delete the ref a
/// concurrent push had just replaced, and then remove that push's
/// record (codex on 18c3fd1).
///
/// Blocking, not `try`: these are two small file operations and a ref
/// edit, and a verb that gave up because another was mid-write would
/// be the same bug with an error message. flock dies with the process,
/// so a killed holder frees it with no cleanup protocol.
///
/// `scan_stash` reads only `*.json`, so this file is invisible to it.
pub struct StashLock {
    _file: std::fs::File,
}

pub fn lock_stash(repo: &Path) -> anyhow::Result<StashLock> {
    use std::os::fd::AsRawFd;
    let dir = stash_dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating `{}`", dir.display()))?;
    let path = dir.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening `{}`", path.display()))?;
    loop {
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(StashLock { _file: file });
        }
        let err = std::io::Error::last_os_error();
        // A signal arriving during the wait is a reason to keep
        // waiting, not a failure to lock.
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err).context("locking the stash");
        }
    }
}

fn stash_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/stash")
}

/// Legacy location from the shelve era — read-only for one release.
fn legacy_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/shelved")
}

/// Where NEW records are written.
fn state_path(repo: &Path, stem: &str) -> PathBuf {
    stash_dir(repo).join(format!("{stem}.json"))
}

/// New pushes protect on the stash ref namespace; readers NEVER
/// construct this — they use the record's own `git_ref`.
fn ref_name(stem: &str) -> String {
    format!("refs/clank/stash/{stem}")
}

/// The record file for `stem`, wherever it lives (new first, then
/// legacy), or `None`.
fn find_record_path(repo: &Path, stem: &str) -> Option<PathBuf> {
    let new = state_path(repo, stem);
    if new.is_file() {
        return Some(new);
    }
    let legacy = legacy_dir(repo).join(format!("{stem}.json"));
    legacy.is_file().then_some(legacy)
}

fn record_revision(meta: &std::fs::Metadata, body: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}:{}",
        meta.dev(),
        meta.ino(),
        meta.ctime(),
        meta.ctime_nsec(),
        blake3::hash(body.as_bytes())
    )
}

fn load_record(repo: &Path, stem: &str) -> anyhow::Result<(PathBuf, StashRecord, String)> {
    let path = find_record_path(repo, stem)
        .ok_or_else(|| anyhow::anyhow!("`{stem}` is not stashed (see `clank stash list`)"))?;
    let mut file =
        std::fs::File::open(&path).with_context(|| format!("opening `{}`", path.display()))?;
    let mut body = String::new();
    file.read_to_string(&mut body)
        .with_context(|| format!("reading `{}`", path.display()))?;
    let stamp = record_revision(&file.metadata()?, &body);
    let current = std::fs::metadata(&path)?;
    if stamp != record_revision(&current, &body) {
        anyhow::bail!("stashed plan `{stem}` changed while it was opened; reopen it");
    }
    let record =
        serde_json::from_str(&body).with_context(|| format!("parsing `{}`", path.display()))?;
    Ok((path, record, stamp))
}

/// The TUI's stashed-plan page reads the Markdown from the record's
/// protective ref. Return the same source with a revision for web
/// restore, which must still name this particular stash.
pub(crate) fn read_stashed(
    repo: &Path,
    stem: &str,
) -> anyhow::Result<(StashRecord, String, String)> {
    if !scan_stash(repo).iter().any(|(name, _)| name == stem) {
        anyhow::bail!("`{stem}` is not stashed (see `clank stash list`)");
    }
    let (_, record, revision) = load_record(repo, stem)?;
    let tip = crate::git_io::resolve_commit(repo, &record.git_ref)
        .ok_or_else(|| anyhow::anyhow!("protective ref `{}` is gone", record.git_ref))?;
    let plan_rel = crate::init_facts::plan_md_rel(stem);
    let markdown = crate::git_io::show_blob(repo, &tip, Path::new(&plan_rel))
        .with_context(|| format!("reading stashed plan `{stem}` from `{}`", record.git_ref))?;
    Ok((record, markdown, revision))
}

/// Read every stash record — the MERGED new+legacy scan (a new record
/// shadows a same-stem legacy one). For `clank stash` list, `status`,
/// the TUI, and the HTML report.
pub fn scan_stash(repo: &Path) -> Vec<(String, StashRecord)> {
    let mut out: Vec<(String, StashRecord)> = Vec::new();
    for dir in [stash_dir(repo), legacy_dir(repo)] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            if out.iter().any(|(s, _)| s == stem) {
                continue; // new location shadows legacy
            }
            if let Ok(body) = std::fs::read_to_string(entry.path())
                && let Ok(state) = serde_json::from_str::<StashRecord>(&body)
            {
                out.push((stem.to_string(), state));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The bare form's options belong to the bare form. Parsed before a
/// verb they would be silently dropped — `clank stash --dry drop foo`
/// running the drop — so a verb with any of them set is refused here,
/// before anything is touched (codex on 70a17f8).
pub fn refuse_parent_options(args: &StashArgs) -> anyhow::Result<()> {
    let Some(cmd) = &args.command else {
        return Ok(());
    };
    let p = &args.push;
    let stray: Vec<&str> = [
        (p.plan.is_some(), "a plan name"),
        (p.repo.is_some(), "--repo"),
        (p.waiting_for.is_some(), "--for"),
        (p.to_queue, "--to-queue"),
        (p.priority.is_some(), "--priority"),
        (p.dry, "--dry"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    if stray.is_empty() {
        return Ok(());
    }
    let verb = match cmd {
        StashCmd::Push(_) => "push",
        StashCmd::Pop(_) => "pop",
        StashCmd::Show(_) => "show",
        StashCmd::Drop(_) => "drop",
        StashCmd::List(_) => "list",
    };
    anyhow::bail!(
        "`clank stash {verb}` takes its options after the verb — {} came before it and would \
         not apply. Write `clank stash {verb} …` with them after, or drop the verb for a bare \
         push.",
        stray.join(", ")
    )
}

/// `clank stash` dispatcher: bare = push, as `git stash` is;
/// push/pop/show/drop/list.
pub async fn run(args: StashArgs) -> anyhow::Result<()> {
    refuse_parent_options(&args)?;
    match args.command {
        None => run_push(args.push).await,
        Some(StashCmd::Push(a)) => run_push(a).await,
        Some(StashCmd::Pop(a)) => run_pop_inner(a.repo.as_deref(), a.plan.as_deref()).await,
        Some(StashCmd::Show(a)) => run_show(a).await,
        Some(StashCmd::Drop(a)) => {
            run_drop_inner(a.repo.as_deref(), a.plan.as_deref(), &mut |w| {
                eprintln!("{w}")
            })
            .await
        }
        Some(StashCmd::List(a)) => run_list(a.repo.as_deref()).await,
    }
}

/// Hidden-alias adapter: `clank shelve [clean]` is `clank stash
/// [drop]`, spelled the old way — the SAME `StashArgs`, so the
/// parent-option refusal and the single-stash inference hold for the
/// alias exactly as for the command (codex on c7cf432).
pub fn shelve_as_stash(args: ShelveArgs) -> StashArgs {
    StashArgs {
        command: args.command.map(|c| match c {
            ShelveCmd::Clean(a) => StashCmd::Drop(super::StashDropArgs {
                plan: a.plan,
                repo: a.repo,
            }),
        }),
        push: StashPushArgs {
            plan: args.plan,
            repo: args.repo,
            waiting_for: args.waiting_for,
            to_queue: args.to_queue,
            priority: args.priority,
            dry: args.dry,
        },
    }
}

pub async fn run_shelve_alias(args: ShelveArgs) -> anyhow::Result<()> {
    run(shelve_as_stash(args)).await
}

/// Hidden-alias adapter: `clank unshelve` is `clank stash pop`.
pub fn unshelve_as_stash(args: UnshelveArgs) -> StashArgs {
    StashArgs {
        command: Some(StashCmd::Pop(super::StashPopArgs {
            plan: args.plan,
            repo: args.repo,
        })),
        push: StashPushArgs {
            plan: None,
            repo: None,
            waiting_for: None,
            to_queue: false,
            priority: None,
            dry: false,
        },
    }
}

pub async fn run_unshelve_alias(args: UnshelveArgs) -> anyhow::Result<()> {
    run(unshelve_as_stash(args)).await
}

/// The stem a pop/show/drop means: the named one, or the only one
/// when exactly one plan is stashed — the rule `push` already uses
/// for the single active plan. Pure over the scan.
fn resolve_stashed(
    stashed: &[(String, StashRecord)],
    plan: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(plan) = plan.map(str::trim).filter(|p| !p.is_empty()) {
        PlanKey::parse(plan).map_err(|e| anyhow::anyhow!("invalid plan `{plan}`: {e}"))?;
        return Ok(plan.to_string());
    }
    match stashed {
        [] => anyhow::bail!("nothing is stashed"),
        [(only, _)] => Ok(only.clone()),
        many => anyhow::bail!(
            "several plans are stashed — name one: {}",
            many.iter()
                .map(|(s, _)| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub async fn run_push(args: StashPushArgs) -> anyhow::Result<()> {
    if !args.to_queue {
        if args.priority.is_some() {
            anyhow::bail!("--priority only applies with --to-queue");
        }
    } else if let Some(p) = args.priority {
        // Mirrors `clank queue add`'s writer range (0-999); the
        // reader is lenient (lenient-queue-filenames) but writers
        // stay canonical.
        if p > 999 {
            anyhow::bail!("--priority must be 0-999 (matches `clank queue add`)");
        }
    }

    let repo = super::resolve_repo(args.repo.as_deref())?;
    // The third writer of the pair. Held from before the
    // already-stashed check, so two pushes of one stem cannot both
    // pass it, and across the ref + record writes that follow.
    let _lock = lock_stash(&repo)?;
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&repo)
        .ok_or_else(|| anyhow::anyhow!("unknown repo basename: {}", repo.display()))?;
    let state = crate::rebuild::rebuild_repo_with_policy(&repo, CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key =
        crate::cli::plan_resolve::resolve_plan(&state, basename.as_str(), args.plan.as_deref())?;
    let stem = plan_key.as_str().to_string();

    if find_record_path(&repo, &stem).is_some() {
        anyhow::bail!(
            "plan `{stem}` is already stashed. \
             `clank stash pop {stem}` to restore it, or \
             `clank stash drop {stem}` to discard it first."
        );
    }
    if let Some(for_plan) = args.waiting_for.as_deref() {
        PlanKey::parse(for_plan)
            .map_err(|e| anyhow::anyhow!("invalid --for plan `{for_plan}`: {e}"))?;
    }

    let preview = crate::preview::build_rewrite_preview(&repo, &state, &plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed: {e}"))?;

    // Same tiered safety as demote had: foreign commits in the
    // range are an unconditional refusal (interleaved plans can't
    // be stashed; `plan-reorder` is the future enabler), non-plan
    // content dropped with the plan.
    safety_check(&preview.commits)?;

    let plan_rel = crate::init_facts::plan_md_rel(&stem);
    if plan_file_dirty(&repo, &plan_rel, preview.head_sha.as_str())? {
        anyhow::bail!(
            "`{plan_rel}` in the working tree differs from HEAD. \
             Commit your plan-body changes or `git stash` before pushing."
        );
    }

    // --to-queue: read the body + pre-check the target before any
    // mutation, demote-style.
    let queue_target = if args.to_queue {
        let body = crate::git_io::show_blob(&repo, &preview.head_sha, Path::new(&plan_rel))
            .context(format!("reading `{plan_rel}` from HEAD into memory"))?;
        let n = args.priority.unwrap_or(500);
        let target = repo.join(format!(".clank/queue/{n:03}-{stem}.md"));
        if target.exists() {
            anyhow::bail!(
                "target file `{}` already exists; pick a different --priority",
                target.strip_prefix(&repo).unwrap_or(&target).display()
            );
        }
        Some((target, body))
    } else {
        None
    };

    let shas: Vec<String> = preview
        .commits
        .iter()
        .filter(|c| !c.foreign)
        .map(|c| c.sha.as_str().to_string())
        .collect();
    if shas.is_empty() {
        anyhow::bail!("plan `{stem}` has no commits to stash");
    }

    if args.dry {
        println!("dry-run: would stash `{stem}`");
        println!(
            "  protective ref: {} @ {}",
            ref_name(&stem),
            preview.head_sha.as_str()
        );
        println!("  commits set aside: {}", shas.len());
        if let Some((target, _)) = &queue_target {
            println!(
                "  plan body → {}",
                target.strip_prefix(&repo).unwrap_or(target).display()
            );
        }
        return Ok(());
    }

    // No confirmation: the ordering below — record and ref before
    // the branch moves — is the safety, not a prompt in front of it,
    // and clank asks no y/N anywhere (stash-does-what-you-mean).

    // ── PROTECT FIRST: the ref lands before anything rewrites. ──
    git_update_ref(&repo, &ref_name(&stem), preview.head_sha.as_str())?;
    let record = StashRecord {
        shas: shas.clone(),
        git_ref: ref_name(&stem),
        waiting_for: args.waiting_for.clone(),
    };
    let sp = state_path(&repo, &stem);
    std::fs::create_dir_all(sp.parent().expect("state path has parent"))?;
    std::fs::write(&sp, serde_json::to_string_pretty(&record)?)
        .with_context(|| format!("writing `{}`", sp.display()))?;

    // ── Drop the plan's commits via the guarded rewrite path. ──
    let drop_commits: Vec<RewriteCommit> = preview
        .commits
        .iter()
        .map(|c| {
            if c.foreign {
                c.clone()
            } else {
                RewriteCommit {
                    sha: c.sha.clone(),
                    subject: c.subject.clone(),
                    disposition: RewriteDisposition::Drop,
                    foreign: false,
                    strip_paths: c.strip_paths.clone(),
                }
            }
        })
        .collect();
    let rewrite_result = run_rewrite(RewriteOpts {
        repo: &repo,
        intro_sha: preview.intro_sha.as_ref(),
        head_sha: &preview.head_sha,
        linear: preview.linear,
        commits: &drop_commits,
        into_branch: None,
        dry: false,
        squash: None,
        squash_tip: None,
        head_strip_paths: &preview.head_strip_paths,
    })
    .await;
    let outcome = match rewrite_result {
        Ok(o) => o,
        Err(e) => {
            // Roll back the protective ref + state ONLY when the
            // branch never moved (a pre-rewrite refusal, e.g. a dirty
            // working tree): the branch
            // still reaches every commit, and stranded state would
            // block the next attempt (codex 1f4800a). But
            // run_rewrite can also fail AFTER moving the branch
            // (update-ref ok, reset --hard failed) — then the
            // commits ARE dropped and the protective ref is the
            // ONLY thing keeping them GC-safe; it must survive
            // (codex 115c014).
            let head_now = git_head(&repo);
            if head_now.as_deref() == Some(preview.head_sha.as_str()) {
                let _ = git_delete_ref(&repo, &ref_name(&stem));
                let _ = std::fs::remove_file(&sp);
            } else {
                eprintln!(
                    "stash: rewrite failed AFTER the branch moved; keeping \
                     {} and {} so the commits stay recoverable",
                    ref_name(&stem),
                    sp.display()
                );
            }
            return Err(e);
        }
    };

    if let Some((target, body)) = &queue_target {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, body)
            .with_context(|| format!("writing plan body to `{}`", target.display()))?;
    }

    if let (Some(tip), Some(branch)) = (outcome.new_tip, outcome.updated_branch) {
        println!(
            "stashed `{stem}`: branch `{branch}` now at {} ({} commits set aside on {})",
            &tip[..tip.len().min(12)],
            shas.len(),
            ref_name(&stem),
        );
    } else {
        println!(
            "stashed `{stem}` ({} commits set aside on {})",
            shas.len(),
            ref_name(&stem)
        );
    }
    if let Some((target, _)) = &queue_target {
        println!(
            "  plan body → {}",
            target.strip_prefix(&repo).unwrap_or(target).display()
        );
    }
    if let Some(for_plan) = &args.waiting_for {
        println!("  waiting on `{for_plan}` — `clank status` will nudge when it finishes");
    }
    println!("  restore with `clank stash pop {stem}`");
    Ok(())
}

pub(crate) async fn run_pop_inner(
    repo_override: Option<&Path>,
    plan: Option<&str>,
) -> anyhow::Result<()> {
    run_pop_checked(repo_override, plan, None).await
}

/// Restore only the stash whose revision the caller opened. The CLI
/// and TUI pass no revision; the web page passes one from `read_stashed`.
pub(crate) async fn run_pop_checked(
    repo_override: Option<&Path>,
    plan: Option<&str>,
    expected_revision: Option<&str>,
) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    // The other verb that destroys the pair: pop deletes the ref and
    // the record once the commits are back. Same section as push and
    // drop, or two of the three can interleave.
    let _lock = lock_stash(&repo)?;
    let stem = resolve_stashed(&scan_stash(&repo), plan)?;

    // Merged lookup: the record may live at the new or the legacy path;
    // its `git_ref` names the protective ref either way.
    let (sp, record, revision) = load_record(&repo, &stem)?;
    if expected_revision.is_some_and(|want| want != revision) {
        anyhow::bail!(
            "stashed plan `{stem}` changed since it was opened; reopen it before restoring"
        );
    }

    // A re-promoted plan with the same stem would collide with the
    // restored plan file; fail closed.
    if repo.join(crate::init_facts::plan_md_rel(&stem)).exists() {
        anyhow::bail!(
            "plan `{stem}` is already active — its stashed commits predate the \
             current plan. `clank stash drop {stem}` to discard them."
        );
    }
    // The rewrite engine's policy, not a raw status: clank's own
    // untracked scratch under `.clank/` — this very stash's record,
    // in a repo whose `.clank/.gitignore` does not cover it — is not
    // the operator's work and must not refuse the pop.
    if crate::cli::rewrite::working_tree_dirty(&repo)? {
        anyhow::bail!("working tree dirty; commit or stash before popping");
    }

    // Cherry-pick oldest-first. On the first conflict git leaves
    // the cherry-pick in progress for the user; the protective ref
    // and record are NOT touched (fail-closed) — finishing by hand
    // + `clank stash drop` completes the restore.
    for sha in &record.shas {
        if !crate::git_plumbing::cherry_pick(&repo, sha)? {
            anyhow::bail!(
                "cherry-pick of {sha} stopped (conflict?). Resolve with git \
                 (`git cherry-pick --continue` / `--abort`); the stash ref \
                 `{}` and record are untouched. After completing by hand, run \
                 `clank stash drop {stem}`.",
                record.git_ref
            );
        }
    }

    // Restore fully landed: drop the protective ref + record (pop =
    // restore AND consume, matching git stash pop).
    git_delete_ref(&repo, &record.git_ref)?;
    std::fs::remove_file(&sp).with_context(|| format!("removing `{}`", sp.display()))?;

    println!(
        "popped `{stem}`: {} commits replayed onto HEAD",
        record.shas.len()
    );
    println!("  reviews reset — the new commits are unreviewed; reviewers will wake");
    Ok(())
}

/// What `drop` says before it discards — on stderr, not as a question:
/// the record of what ran, since this is the one verb with no undo.
fn drop_warning(stem: &str) -> String {
    format!(
        "dropping the stashed commits of `{stem}` — the only copy of that work; \
         `git reflog` will not have them"
    )
}

/// `warn` receives the warning BEFORE the ref or record is touched;
/// injected so a test can hold the order to account.
/// WHICH stash a record is, cheaply enough to carry through a screen.
///
/// The stem is a NAME and names are reused: pop a stash and push
/// another under the same plan and the name still resolves — to
/// different commits. Anything that decides to destroy a stash at one
/// moment and destroys it at another has to say which one it meant, or
/// it is deleting whatever now answers to that name (codex on
/// c1f9301).
///
/// A 64-bit digest rather than the shas themselves because the caller
/// that needs it is `Copy`. That is sound HERE: this guards an honest
/// race between two clank processes, not a forged collision, and the
/// only cost of the vanishing chance of one is the drop the operator
/// asked for going ahead.
pub fn record_fingerprint(record: &StashRecord) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    record.git_ref.hash(&mut h);
    record.shas.hash(&mut h);
    h.finish()
}

/// WHICH stash a caller looked at: the record it read AND where the
/// protective ref pointed at that moment.
///
/// Both halves, because either alone can be stale while the other is
/// not. The record identifies the stash; the ref target is the thing
/// actually deleted, and something can move it while leaving the
/// record untouched — after which the fingerprint still matches and a
/// freshly-resolved target is the NEW one, so a delete conditional on
/// it destroys a ref nobody was shown (codex on fe17dc4).
///
/// The target is the raw oid rather than a hash of it: it is what the
/// compare-and-delete is given, so it is carried exactly. `Copy`,
/// because the confirm screen that holds it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StashIdentity {
    pub fingerprint: u64,
    pub at: [u8; 20],
}

impl StashIdentity {
    /// `None` when the protective ref does not resolve — there is
    /// nothing to be sure about, so nothing should claim to be.
    pub fn of(repo: &Path, record: &StashRecord) -> Option<Self> {
        let at = crate::git_io::resolve_commit(repo, &record.git_ref)?;
        let mut bytes = [0u8; 20];
        let raw = hex_oid(at.as_str())?;
        bytes.copy_from_slice(&raw);
        Some(Self {
            fingerprint: record_fingerprint(record),
            at: bytes,
        })
    }

    pub fn hex(&self) -> String {
        self.at.iter().map(|b| format!("{b:02x}")).collect()
    }
}

fn hex_oid(sha: &str) -> Option<Vec<u8>> {
    if sha.len() != 40 {
        return None;
    }
    (0..20)
        .map(|i| u8::from_str_radix(&sha[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

pub async fn run_drop_inner(
    repo_override: Option<&Path>,
    plan: Option<&str>,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<()> {
    run_drop_checked(repo_override, plan, None, warn).await
}

/// `expect` is the stash the caller MEANT, as a fingerprint.
///
/// `None` for a caller with no window to be raced through — the CLI
/// verb resolves and deletes in one breath. A UI that showed a confirm
/// first has a window, and passes what it showed: the check and the
/// delete happen here, under one resolution, so there is nothing
/// between them to swap the record.
pub async fn run_drop_checked(
    repo_override: Option<&Path>,
    plan: Option<&str>,
    expect: Option<StashIdentity>,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    // HELD across resolve, verify, ref-delete and record-removal. A
    // stash is two objects and this is the verb that destroys both.
    let _lock = lock_stash(&repo)?;
    let stem = resolve_stashed(&scan_stash(&repo), plan)?;
    let sp =
        find_record_path(&repo, &stem).ok_or_else(|| anyhow::anyhow!("`{stem}` is not stashed"))?;
    // The record's OWN git_ref, never a stem-constructed path — a legacy
    // record's ref lives under refs/clank/shelved/ (codex d633f87).
    let record: StashRecord = serde_json::from_str(
        &std::fs::read_to_string(&sp).with_context(|| format!("reading `{}`", sp.display()))?,
    )
    .with_context(|| format!("parsing `{}`", sp.display()))?;
    // The RECORD still being the one that was read, and the ref still
    // pointing where it pointed then. A caller that looked at a stash
    // and then decided to destroy it is only entitled to destroy THAT
    // stash.
    if let Some(want) = expect
        && record_fingerprint(&record) != want.fingerprint
    {
        anyhow::bail!(
            "`{stem}` is not the stash you were shown — its commits changed while the \
             confirmation was open. Nothing was dropped; look again with `clank stash \
             show {stem}`."
        );
    }
    // CAPTURED, never re-resolved. Re-resolving would read whatever the
    // ref points at NOW and then delete exactly that — which is not a
    // guard, it is an obedient delete of something nobody was shown
    // (codex on fe17dc4). With no expectation there is nothing to
    // compare and the delete is unconditional, as the CLI verb's is.
    let expected_at = expect.map(|w| w.hex());
    warn(drop_warning(&stem));
    // COMPARE-AND-DELETE, on top of the lock. The lock stops another
    // clank verb; this stops everything else — another tool, or a
    // future path here that forgets to take it. A ref that moved is
    // refused by git rather than deleted.
    crate::git_plumbing::delete_ref_at(&repo, &record.git_ref, expected_at.as_deref())?;
    std::fs::remove_file(&sp).with_context(|| format!("removing `{}`", sp.display()))?;
    println!("dropped stashed state for `{stem}`");
    Ok(())
}

/// `clank stash show <plan>` — the stashed plan body (read from the
/// record's protective ref: the plan file no longer exists on the
/// branch) + its commit list, oldest first (the pop order).
async fn run_show(args: StashShowArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let stem = resolve_stashed(&scan_stash(&repo), args.plan.as_deref())?;
    let sp = find_record_path(&repo, &stem)
        .ok_or_else(|| anyhow::anyhow!("`{stem}` is not stashed (see `clank stash list`)"))?;
    let record: StashRecord = serde_json::from_str(
        &std::fs::read_to_string(&sp).with_context(|| format!("reading `{}`", sp.display()))?,
    )
    .with_context(|| format!("parsing `{}`", sp.display()))?;

    let tip = crate::git_io::resolve_commit(&repo, &record.git_ref)
        .ok_or_else(|| anyhow::anyhow!("protective ref `{}` is gone", record.git_ref))?;
    println!(
        "stashed `{stem}` ({} commit(s) on {})",
        record.shas.len(),
        record.git_ref
    );
    if let Some(w) = &record.waiting_for {
        println!("waiting on `{w}`");
    }
    println!();
    for sha in &record.shas {
        let key = crate::lifecycle::CommitSha::parse(sha)
            .map_err(|e| anyhow::anyhow!("record sha `{sha}`: {e}"))?;
        let subject = crate::git_io::commit_subject_at(&repo, &key).unwrap_or_default();
        println!("  {} {subject}", &sha[..7.min(sha.len())]);
    }
    println!();
    let plan_rel = crate::init_facts::plan_md_rel(&stem);
    match crate::git_io::show_blob(&repo, &tip, Path::new(&plan_rel)) {
        Ok(body) => print!("{body}"),
        Err(_) => println!("(plan body not present at the protective ref)"),
    }
    Ok(())
}

/// `clank stash list` — one line per item.
async fn run_list(repo_override: Option<&Path>) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    let items = scan_stash(&repo);
    if items.is_empty() {
        println!("stash is empty");
        return Ok(());
    }
    // Readiness (a `--for` dependency that has finished) comes from the
    // fold — the same signal `status` renders.
    let state = crate::rebuild::rebuild_repo_with_policy(&repo, CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    for (stem, record) in &items {
        let note = match &record.waiting_for {
            Some(w)
                if state
                    .fold
                    .finished_plans
                    .iter()
                    .any(|f| f.plan.as_str() == w) =>
            {
                format!(" · was waiting on {w} — READY, pop?")
            }
            Some(w) => format!(" · waiting on {w}"),
            None => String::new(),
        };
        println!("{stem} · {} commit(s){note}", record.shas.len());
    }
    Ok(())
}

/// Current HEAD sha, or None if it can't be read (detached states
/// and read failures both land on the SAFE side of the rollback
/// decision: keep the protective ref).
fn git_head(repo: &Path) -> Option<String> {
    crate::git_io::rev_parse_head(repo)
        .ok()
        .flatten()
        .map(|s| s.as_str().to_string())
}

fn git_update_ref(repo: &Path, name: &str, sha: &str) -> anyhow::Result<()> {
    // Unconditional set — the protective ref is ours to overwrite.
    crate::git_plumbing::update_ref(repo, name, sha, crate::git_plumbing::ExpectedRef::Any)
}

fn git_delete_ref(repo: &Path, name: &str) -> anyhow::Result<()> {
    crate::git_plumbing::delete_ref(repo, name)
}

/// Foreign-commit refusal. The ONLY tier: a commit the plan's own
/// timeline does not claim cannot be set aside as a side effect of
/// stashing that plan, and `--force` does not bypass it (codex
/// 625b8af: a foreign commit can classify as `Rewrite`, so ANY
/// foreign is refused, not just `KeepVerbatim`). Note this covers
/// two sources of foreignness — another plan's commits AND untagged
/// ad-hoc work interleaved in the range — because `foreign` is
/// `!attributed`, attribution being membership in the plan's own
/// timeline.
///
/// There used to be a second tier: any non-foreign commit whose
/// disposition is `Rewrite` — that is, one touching content beyond
/// the plan document — refused unless `--force`. That is what an
/// implementation commit IS, so it fired for every plan past its
/// intro and made `clank stash` unusable on real work, while
/// offering nothing but an instruction to pass `--force`. It was
/// inherited from `demote`, which no longer exists.
///
/// It is not replaced by a smarter predicate, because none exists: a
/// `[plan]` tag asserts ownership of the whole commit, not that no
/// unrelated change rides along in it. What remains is a deliberate
/// trade, and `purge` already makes it on the IRREVERSIBLE path (see
/// `purge::drop_safety_check`) — stash pops back, so it costs
/// strictly less here (stash-stops-refusing-impl-commits).
fn safety_check(commits: &[RewriteCommit]) -> anyhow::Result<()> {
    let foreign_shas = crate::preview::foreign_shas(commits);
    if !foreign_shas.is_empty() {
        let list: Vec<String> = foreign_shas
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        anyhow::bail!(
            "stash push refuses: foreign commit(s) interleaved in plan range: {}. \
             Disentangle first (see the `plan-reorder` queue item) or coordinate \
             with the author(s).",
            list.join(", ")
        );
    }
    Ok(())
}

/// True if `rel_path` in the working tree differs from its content
/// at `head_sha` (inherited from demote).
fn plan_file_dirty(repo: &Path, rel_path: &str, head_sha: &str) -> anyhow::Result<bool> {
    let wt_path = repo.join(rel_path);
    let wt_content = match std::fs::read_to_string(&wt_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("reading `{}`", wt_path.display()));
        }
    };
    let head_sha_parsed = CommitSha::parse(head_sha)
        .map_err(|e| anyhow::anyhow!("invalid head SHA `{head_sha}`: {e}"))?;
    let head_content = crate::git_io::show_blob(repo, &head_sha_parsed, Path::new(rel_path)).ok();
    Ok(wt_content != head_content)
}

#[cfg(test)]
mod drop_boundary_tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A stash's record and the identity a caller would capture from it.
    fn stashed(repo: &Path) -> (StashRecord, StashIdentity) {
        let rec = scan_stash(repo)
            .into_iter()
            .find(|(stem, _)| stem == "foo")
            .map(|(_, r)| r)
            .expect("a stash record");
        let id = StashIdentity::of(repo, &rec).expect("the protective ref resolves");
        (rec, id)
    }

    async fn repo_with_a_stash() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "--quiet", "--initial-branch=main"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        let write = |rel: &str, body: &str| {
            std::fs::create_dir_all(p.join(rel).parent().unwrap()).unwrap();
            std::fs::write(p.join(rel), body).unwrap();
        };
        write(
            ".clank/config.json",
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        );
        write(
            ".clank/.gitignore",
            "/cache/\n/agents/\n/stash/\n/shelved/\n",
        );
        write("base.rs", "// base\n");
        git(p, &["add", "-A"]);
        git(p, &["commit", "--quiet", "-m", "base"]);
        write(".clank/plans/foo.md", "# foo\n\nbody\n");
        git(p, &["add", "-A"]);
        git(p, &["commit", "--quiet", "-m", "[foo] intro"]);
        run_push(StashPushArgs {
            plan: Some("foo".into()),
            repo: Some(p.to_path_buf()),
            waiting_for: None,
            to_queue: false,
            priority: None,
            dry: false,
        })
        .await
        .unwrap();
        dir
    }

    /// The ref is deleted CONDITIONALLY, so a ref that moved between
    /// the check and the delete is refused rather than destroyed.
    ///
    /// The lock keeps other clank verbs out of that window; this is the
    /// other half, for everything the lock cannot reach — another tool,
    /// a stray `git update-ref`, or a future path here that forgets to
    /// take it. Driven through `warn`, which is the one hook that runs
    /// between the two, so the mover is genuinely INSIDE the boundary
    /// rather than before it (codex on 18c3fd1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ref_that_moves_inside_the_boundary_is_not_deleted() {
        let dir = repo_with_a_stash().await;
        let p = dir.path().to_path_buf();
        let (rec, want) = stashed(&p);

        // Something else will point the protective ref elsewhere while
        // the drop is between verifying and deleting.
        let elsewhere = git(&p, &["rev-parse", "HEAD"]);
        assert_ne!(elsewhere, want.hex(), "the mover must change the target");

        let moved = std::cell::Cell::new(false);
        let out = run_drop_checked(Some(&p), Some("foo"), Some(want), &mut |_| {
            git(&p, &["update-ref", &rec.git_ref, &elsewhere]);
            moved.set(true);
        })
        .await;

        assert!(moved.get(), "the seam ran — otherwise this proves nothing");
        let err = out.expect_err("a moved ref must not be deleted");
        assert!(
            crate::git_io::resolve_commit(&p, &rec.git_ref).is_some(),
            "and it is still there: {err:#}"
        );
    }

    /// A ref moved BEFORE the answer — while the confirm sits open —
    /// is not deleted either.
    ///
    /// The boundary test covers the sliver between verify and delete.
    /// This covers the long window that actually exists: a screen a
    /// human is reading. The record is untouched here, so the
    /// fingerprint still matches — the only thing that can catch it is
    /// the ref target having been CAPTURED when the screen opened
    /// rather than resolved when the answer arrives (codex on fe17dc4).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_ref_moved_while_the_confirm_is_open_is_not_deleted() {
        let dir = repo_with_a_stash().await;
        let p = dir.path().to_path_buf();
        let (rec, shown) = stashed(&p);

        // The screen is open. Something else repoints the protective
        // ref and leaves the record alone.
        let elsewhere = git(&p, &["rev-parse", "HEAD"]);
        assert_ne!(elsewhere, shown.hex());
        git(&p, &["update-ref", &rec.git_ref, &elsewhere]);

        let after = scan_stash(&p)
            .into_iter()
            .find(|(stem, _)| stem == "foo")
            .map(|(_, r)| record_fingerprint(&r))
            .expect("the record is still there");
        assert_eq!(
            after, shown.fingerprint,
            "the control: the RECORD is untouched, so a fingerprint-only guard would pass"
        );

        let out = run_drop_checked(Some(&p), Some("foo"), Some(shown), &mut |_| {}).await;

        out.expect_err("a ref that moved under the confirm must not be deleted");
        assert_eq!(
            crate::git_io::resolve_commit(&p, &rec.git_ref)
                .map(|c| c.as_str().to_string())
                .as_deref(),
            Some(elsewhere.as_str()),
            "and the ref somebody else set is still exactly where they put it"
        );
        assert!(!scan_stash(&p).is_empty(), "record intact too");
    }

    /// The drop holds the stash lock for its whole critical section.
    ///
    /// The compare-and-delete makes the REF deletion exact; it says
    /// nothing about the record file removed afterwards. Only the lock
    /// keeps another clank verb out of that gap, and only a second
    /// thread can show it is held — a mutant that removes it passes
    /// every single-threaded test (codex on 18c3fd1).
    #[test]
    fn the_drop_waits_for_the_stash_lock() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = rt.block_on(repo_with_a_stash());
        let p = dir.path().to_path_buf();
        let (rec, want) = stashed(&p);

        let held = lock_stash(&p).expect("the lock");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, path) = (done.clone(), p.clone());
        let dropping = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let out = rt.block_on(run_drop_checked(
                Some(&path),
                Some("foo"),
                Some(want),
                &mut |_| {},
            ));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            out
        });

        for _ in 0..40 {
            if done.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !done.load(std::sync::atomic::Ordering::SeqCst),
            "the drop ran while the stash was locked"
        );
        assert!(
            !scan_stash(&p).is_empty(),
            "and nothing was removed while it waited"
        );

        drop(held);
        dropping.join().unwrap().expect("then it drops");
        assert!(scan_stash(&p).is_empty());
    }

    /// The same path, unmolested, still drops. Without this the test
    /// above passes against a drop that can never delete anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unmoved_ref_is_deleted() {
        let dir = repo_with_a_stash().await;
        let p = dir.path().to_path_buf();
        let (rec, want) = stashed(&p);

        run_drop_checked(Some(&p), Some("foo"), Some(want), &mut |_| {})
            .await
            .expect("an untouched stash drops");
        assert!(crate::git_io::resolve_commit(&p, &rec.git_ref).is_none());
        assert!(scan_stash(&p).is_empty());
    }
}
