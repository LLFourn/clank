//! `clank stash` — set an in-flight plan's commits aside (bare, or
//! `push`) and restore them later (`pop`), with `show` / `drop` /
//! `list`. The verbs mirror `git stash`, bare-is-push included
//! (rename-shelve-to-stash, stash-does-what-you-mean; formerly
//! `clank shelve`/`unshelve`, kept as hidden aliases for a release).
//!
//! Push and pop are one transaction each (`stash_txn`): the result is
//! built in the object database, checked against the working tree, and
//! only then published. Push writes the protective ref
//! (`refs/clank/stash/<plan>`) before the branch moves, so the plan's
//! commits stay reachable and GC-protected if a later step fails.
//! Uncommitted work goes into the stash as an automatic commit; foreign
//! commits stay, replayed as their changes. Pop replays the recorded
//! shas back; reviews RESET by design (new shas, new context — the fold
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

use crate::cli::stash_txn as txn;
use crate::lifecycle::PlanKey;
use crate::rebuild::CachePolicy;
use clank_core::ids::CommitSha;

use super::{
    ShelveArgs, ShelveCmd, StashArgs, StashCmd, StashPushArgs, StashShowArgs, UnshelveArgs,
};

/// On-disk stash record — `.clank/stash/<plan>.json` for new pushes,
/// `.clank/shelved/<plan>.json` for legacy ones (read-both). The wire
/// shape is unchanged from the shelve era, so legacy records parse.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StashRecord {
    /// The plan's attributed commit shas, plus any automatic commit of
    /// uncommitted work, oldest first — the order pop replays them in.
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
        (p.dry, "--dry-run"),
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
    push_with(args, &mut |_, _| {}).await
}

/// [`run_push`], with `hooks` run at each [`Stage`] of the transaction.
#[doc(hidden)]
pub async fn push_with(
    args: StashPushArgs,
    hooks: &mut (dyn FnMut(Stage, &Path) + Send),
) -> anyhow::Result<()> {
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
    let _slot = super::plan_mutation::lock(&repo)?;
    // A dry run writes nothing, not even the lock file in the stash
    // directory; the plan mutation lock above still serializes it.
    let _lock = if args.dry {
        None
    } else {
        Some(lock_stash(&repo)?)
    };
    if let Some(named) = args
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        && PlanKey::parse(named).is_ok()
        && find_record_path(&repo, named).is_some()
    {
        println!("`{named}` is already stashed; `clank stash pop {named}` restores it");
        return Ok(());
    }
    let basename = crate::lifecycle::RepoBasename::from_repo_root(&repo)
        .ok_or_else(|| anyhow::anyhow!("unknown repo basename: {}", repo.display()))?;
    let state = crate::rebuild::rebuild_repo_with_policy(&repo, CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plan_key =
        crate::cli::plan_resolve::resolve_plan(&state, basename.as_str(), args.plan.as_deref())?;
    let stem = plan_key.as_str().to_string();

    if find_record_path(&repo, &stem).is_some() {
        println!("`{stem}` is already stashed; `clank stash pop {stem}` restores it");
        return Ok(());
    }
    if let Some(for_plan) = args.waiting_for.as_deref() {
        PlanKey::parse(for_plan)
            .map_err(|e| anyhow::anyhow!("invalid --for plan `{for_plan}`: {e}"))?;
    }

    let preview = crate::preview::build_rewrite_preview(&repo, &state, &plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed: {e}"))?;
    let intro = preview
        .intro_sha
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plan `{stem}` has no intro commit"))?;
    let base = crate::git_io::first_parent_at(&repo, intro.as_str())?;
    let owned: Vec<String> = preview
        .commits
        .iter()
        .filter(|c| !c.foreign)
        .map(|c| c.sha.as_str().to_string())
        .collect();
    let foreign: Vec<String> = preview
        .commits
        .iter()
        .filter(|c| c.foreign)
        .map(|c| c.sha.as_str().to_string())
        .collect();
    let plan_rel = crate::init_facts::plan_md_rel(&stem);
    let automatic = format!("[{stem}] clank stash: uncommitted work at stash time");

    let mut attempt = 0;
    loop {
        attempt += 1;
        let captured = txn::Capture::read(&repo)?;
        if captured.head != preview.head_sha.as_str() {
            anyhow::bail!(
                "HEAD moved from {} to {} while the plan was being read; nothing moved",
                txn::short(preview.head_sha.as_str()),
                txn::short(&captured.head)
            );
        }
        let snap = txn::snapshot(&repo, &captured, &automatic)?;
        let t0 = snap.clone().unwrap_or_else(|| captured.head.clone());
        let mut set_aside = owned.clone();
        set_aside.extend(snap.clone());
        let (tip, replayed) = match txn::replay_onto(&repo, base.as_deref(), &foreign)? {
            txn::Built::Conflict { sha, paths } => anyhow::bail!(
                "stash refuses: {} ({}) is not this plan's, and it does not apply without \
                 the plan's commits, at {}. Nothing moved.",
                txn::short(&sha),
                subject(&repo, &sha),
                paths.join(", ")
            ),
            txn::Built::Tip { tip, replayed } => (tip, replayed),
        };
        let (tip, emptied) = match tip {
            Some(tip) => (tip, false),
            None => {
                let empty = crate::git_plumbing::empty_tree(&repo)?;
                let root = crate::git_plumbing::commit_new(
                    &repo,
                    &empty,
                    None,
                    &format!("clank stash: {stem} set aside; it was this branch's whole history"),
                )?;
                (root, true)
            }
        };

        // --to-queue: the body as it is on disk now, which the snapshot holds.
        let queue_target = if args.to_queue {
            let body = crate::git_io::show_blob(
                &repo,
                &CommitSha::parse(&t0).map_err(|e| anyhow::anyhow!("sha `{t0}`: {e}"))?,
                Path::new(&plan_rel),
            )
            .context(format!("reading `{plan_rel}` into memory"))?;
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

        let blocked = txn::preflight(&repo, &captured.tracked, &tip)?;
        if args.dry {
            println!("dry-run: would stash `{stem}`");
            println!(
                "  protective ref: {} @ {}",
                ref_name(&stem),
                txn::short(&t0)
            );
            println!("  commits set aside: {}", set_aside.len());
            if snap.is_some() {
                let paths = crate::git_io::working_tree_status(&repo)
                    .map(|s| s.changed)
                    .unwrap_or_default();
                println!("  automatic commit `{automatic}` of: {}", paths.join(", "));
            }
            for r in &replayed {
                println!("  would keep {}", describe_replay(&repo, r));
            }
            if !blocked.is_empty() {
                println!("  would refuse, in the way: {}", blocked.join("; "));
            }
            if let Some((target, _)) = &queue_target {
                println!(
                    "  plan body → {}",
                    target.strip_prefix(&repo).unwrap_or(target).display()
                );
            }
            return Ok(());
        }
        if !blocked.is_empty() {
            anyhow::bail!(
                "stash refuses: the working tree has files the stash would destroy: {}. \
                 Nothing moved.",
                blocked.join("; ")
            );
        }
        hooks(Stage::Checked, &repo);
        match txn::fence(&repo, &captured)? {
            txn::Fence::Same => {}
            txn::Fence::Newer if attempt < ATTEMPTS => continue,
            txn::Fence::Newer => anyhow::bail!(
                "the work kept changing while stashing ({ATTEMPTS} tries); nothing moved"
            ),
            txn::Fence::Moved(why) => anyhow::bail!("{why}; nothing moved"),
        }
        hooks(Stage::Fenced, &repo);

        // ── PROTECT FIRST: the ref and record land before anything moves. ──
        git_update_ref(&repo, &ref_name(&stem), &t0)?;
        let record = StashRecord {
            shas: set_aside.clone(),
            git_ref: ref_name(&stem),
            waiting_for: args.waiting_for.clone(),
        };
        let sp = state_path(&repo, &stem);
        std::fs::create_dir_all(sp.parent().expect("state path has parent"))?;
        std::fs::write(&sp, serde_json::to_string_pretty(&record)?)
            .with_context(|| format!("writing `{}`", sp.display()))?;
        let unprotect = || {
            let _ = crate::git_plumbing::delete_ref_at(&repo, &ref_name(&stem), Some(&t0));
            let _ = std::fs::remove_file(&sp);
        };

        let published = match txn::publish(&repo, &captured, &captured.tracked, &tip) {
            Ok(txn::Published::RolledBack(lines)) => {
                unprotect();
                anyhow::bail!(
                    "stash refuses: something changed while stashing: {}. Nothing moved.",
                    lines.join("; ")
                );
            }
            Ok(txn::Published::Partial { lines, at_tip }) => anyhow::bail!(
                "stash: {}, but the working tree may be only partly written: {}. Every \
                 set-aside commit, the uncommitted work among them, is kept on {} and in {}. \
                 Fix the cause, then {}.",
                where_head(&captured, &tip, at_tip),
                lines.join("; "),
                ref_name(&stem),
                sp.display(),
                if at_tip {
                    format!(
                        "`git reset --hard` finishes the move, or `clank stash pop {stem}` \
                         undoes it"
                    )
                } else {
                    format!("`git reset --hard {}` finishes the move", txn::short(&tip))
                }
            ),
            Ok(published) => published,
            Err(e) => {
                // `publish` errs only while nothing has moved.
                unprotect();
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
        println!(
            "stashed `{stem}`: now at {} ({} commits set aside on {})",
            txn::short(&tip),
            set_aside.len(),
            ref_name(&stem),
        );
        if let Some(snap) = &snap {
            println!(
                "  automatic commit {} holds the uncommitted work, set aside with the plan",
                txn::short(snap)
            );
        }
        for r in &replayed {
            println!("  kept {}", describe_replay(&repo, r));
        }
        if emptied {
            println!(
                "  the plan was this branch's whole history: it now starts at an empty commit"
            );
        }
        if published == txn::Published::IndexKept {
            println!(
                "  the index changed while stashing and was left as it was; \
                 `git status` shows the difference"
            );
        }
        warn_untracked(&repo);
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
        return Ok(());
    }
}

/// Where a partial publish left HEAD, for its message.
fn where_head(captured: &txn::Capture, tip: &str, at_tip: bool) -> String {
    let on = captured
        .binding
        .as_deref()
        .map(|b| format!("`{}`", b.trim_start_matches("refs/heads/")))
        .unwrap_or_else(|| "HEAD".into());
    if at_tip {
        format!("{on} is at {}", txn::short(tip))
    } else {
        format!("{on} is still at {}", txn::short(&captured.head))
    }
}

/// Where pop keeps its result reachable while it writes the working tree.
/// It lives from just before the write until that attempt succeeds or is
/// proven to have written nothing; a partial attempt leaves it, and every
/// later pop of the plan refuses until the operator has recovered from it
/// and deleted it.
fn pop_ref(stem: &str) -> String {
    format!("refs/clank/stash-pop/{stem}")
}

/// How many times a transaction is rebuilt for work that changed under it.
const ATTEMPTS: usize = 3;

/// Where a transaction is when its test seam runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum Stage {
    /// Built and checked against the working tree; not yet fenced.
    Checked,
    /// Fenced; about to publish.
    Fenced,
}

fn subject(repo: &Path, sha: &str) -> String {
    CommitSha::parse(sha)
        .ok()
        .and_then(|s| crate::git_io::open(repo).ok()?.commit_subject(&s).ok())
        .unwrap_or_default()
}

fn describe_replay(repo: &Path, r: &txn::Replay) -> String {
    format!(
        "{} → {} {}{}",
        txn::short(&r.original),
        txn::short(&r.new),
        subject(repo, &r.original),
        if r.linearized {
            " (a merge, kept as one commit)"
        } else {
            ""
        }
    )
}

fn warn_untracked(repo: &Path) {
    let left = txn::untracked_left(repo);
    if left.is_empty() {
        return;
    }
    const SHOWN: usize = 10;
    let more = left.len().saturating_sub(SHOWN);
    let mut names = left.into_iter().take(SHOWN).collect::<Vec<_>>().join(", ");
    if more > 0 {
        names.push_str(&format!(" and {more} more"));
    }
    eprintln!("warning: untracked files were left as they are: {names}");
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
    pop_with(repo_override, plan, expected_revision, &mut |_, _| {}).await
}

/// [`run_pop_checked`], with `hooks` run at each [`Stage`].
#[doc(hidden)]
pub async fn pop_with(
    repo_override: Option<&Path>,
    plan: Option<&str>,
    expected_revision: Option<&str>,
    hooks: &mut (dyn FnMut(Stage, &Path) + Send),
) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    // The other verb that destroys the pair: pop deletes the ref and
    // the record once the commits are back. Same section as push and
    // drop, or two of the three can interleave.
    let slot = super::plan_mutation::lock(&repo)?;
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
    slot.empty_state()?;
    if let Some(held) = crate::git_io::resolve_commit(&repo, &pop_ref(&stem)) {
        anyhow::bail!(
            "an earlier pop of `{stem}` stopped part way, and its result, with the \
             uncommitted work from before it, is still kept on {} at {}. Nothing moved. \
             Restore what you need from it (`git reset --hard {}` puts the branch and \
             working tree there), then remove it with `git update-ref -d {}` and pop again.",
            pop_ref(&stem),
            txn::short(held.as_str()),
            txn::short(held.as_str()),
            pop_ref(&stem)
        );
    }
    let protected = crate::git_io::resolve_commit(&repo, &record.git_ref)
        .ok_or_else(|| anyhow::anyhow!("protective ref `{}` is gone", record.git_ref))?;

    let mut attempt = 0;
    let (tip, snap) = loop {
        attempt += 1;
        let captured = txn::Capture::read(&repo)?;
        // No plan is active, so no plan owns this work.
        let snap = txn::snapshot(
            &repo,
            &captured,
            "clank stash pop: uncommitted work at pop time",
        )?;
        let base = snap.clone().unwrap_or_else(|| captured.head.clone());
        let tip = match txn::replay_onto(&repo, Some(&base), &record.shas)? {
            txn::Built::Conflict { sha, paths } => anyhow::bail!(
                "pop refuses: stashed commit {} ({}) conflicts with the branch as it is now, \
                 at {}. Nothing moved, and the stash is untouched.",
                txn::short(&sha),
                subject(&repo, &sha),
                paths.join(", ")
            ),
            txn::Built::Tip { tip, .. } => tip.expect("replayed onto a base"),
        };
        let blocked = txn::preflight(&repo, &captured.tracked, &tip)?;
        if !blocked.is_empty() {
            anyhow::bail!(
                "pop refuses: the working tree has files the restore would destroy: {}. \
                 Nothing moved, and the stash is untouched.",
                blocked.join("; ")
            );
        }
        hooks(Stage::Checked, &repo);
        match txn::fence(&repo, &captured)? {
            txn::Fence::Same => {}
            txn::Fence::Newer if attempt < ATTEMPTS => continue,
            txn::Fence::Newer => anyhow::bail!(
                "the work kept changing while popping ({ATTEMPTS} tries); nothing moved"
            ),
            txn::Fence::Moved(why) => anyhow::bail!("{why}; nothing moved"),
        }
        hooks(Stage::Fenced, &repo);
        // The result, the pre-pop snapshot under it, is kept reachable
        // before the working tree is written, so no failure loses it.
        let held = pop_ref(&stem);
        // Create-only: an existing one is an earlier attempt's, possibly
        // the last copy of its work, and never this attempt's to replace.
        crate::git_plumbing::update_ref(
            &repo,
            &held,
            &tip,
            crate::git_plumbing::ExpectedRef::CreateOnly,
        )
        .with_context(|| format!("{held} appeared while popping; nothing moved"))?;
        let published = match txn::publish(&repo, &captured, &captured.tracked, &tip) {
            Ok(published) => published,
            Err(e) => {
                let _ = crate::git_plumbing::delete_ref_at(&repo, &held, Some(&tip));
                return Err(e);
            }
        };
        if !matches!(published, txn::Published::Partial { .. }) {
            let _ = crate::git_plumbing::delete_ref_at(&repo, &held, Some(&tip));
        }
        match published {
            txn::Published::RolledBack(lines) => anyhow::bail!(
                "pop refuses: something changed while popping: {}. Nothing moved, and the \
                 stash is untouched.",
                lines.join("; ")
            ),
            txn::Published::Partial { lines, at_tip } => anyhow::bail!(
                "pop: {}, but the working tree may be only partly written: {}. The restored \
                 commits, with the uncommitted work from before the pop committed under \
                 them, are kept on {held}; the stash ref and record are kept too. Fix the \
                 cause, then `git reset --hard {}` finishes the restore, and `clank stash \
                 drop {stem}` and `git update-ref -d {held}` clear what is left.",
                where_head(&captured, &tip, at_tip),
                lines.join("; "),
                txn::short(&tip)
            ),
            txn::Published::IndexKept => println!(
                "  the index changed while popping and was left as it was; \
                 `git status` shows the difference"
            ),
            txn::Published::Done => {}
        }
        break (tip, snap);
    };

    // Restore fully landed: drop the protective ref + record (pop =
    // restore AND consume, matching git stash pop).
    crate::git_plumbing::delete_ref_at(&repo, &record.git_ref, Some(protected.as_str()))?;
    std::fs::remove_file(&sp).with_context(|| format!("removing `{}`", sp.display()))?;

    println!(
        "popped `{stem}`: {} commits replayed, now at {}",
        record.shas.len(),
        txn::short(&tip)
    );
    if let Some(snap) = &snap {
        println!(
            "  automatic commit {} holds the uncommitted work from before the pop",
            txn::short(snap)
        );
    }
    warn_untracked(&repo);
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
fn git_update_ref(repo: &Path, name: &str, sha: &str) -> anyhow::Result<()> {
    // Unconditional set — the protective ref is ours to overwrite.
    crate::git_plumbing::update_ref(repo, name, sha, crate::git_plumbing::ExpectedRef::Any)
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

    async fn repo_with_a_stash() -> super::super::monitor::TempRepo {
        let dir = super::super::monitor::TempRepo::new();
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
        let (_rec, want) = stashed(&p);

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
