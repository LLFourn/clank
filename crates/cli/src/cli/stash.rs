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

async fn run_pop_inner(repo_override: Option<&Path>, plan: Option<&str>) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    let stem = resolve_stashed(&scan_stash(&repo), plan)?;

    // Merged lookup: the record may live at the new or the legacy path;
    // its `git_ref` names the protective ref either way.
    let sp = find_record_path(&repo, &stem)
        .ok_or_else(|| anyhow::anyhow!("`{stem}` is not stashed (see `clank stash list`)"))?;
    let body =
        std::fs::read_to_string(&sp).with_context(|| format!("reading `{}`", sp.display()))?;
    let record: StashRecord =
        serde_json::from_str(&body).with_context(|| format!("parsing `{}`", sp.display()))?;

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
pub async fn run_drop_inner(
    repo_override: Option<&Path>,
    plan: Option<&str>,
    warn: &mut dyn FnMut(String),
) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo_override)?;
    let stem = resolve_stashed(&scan_stash(&repo), plan)?;
    let sp =
        find_record_path(&repo, &stem).ok_or_else(|| anyhow::anyhow!("`{stem}` is not stashed"))?;
    // The record's OWN git_ref, never a stem-constructed path — a legacy
    // record's ref lives under refs/clank/shelved/ (codex d633f87).
    let record: StashRecord = serde_json::from_str(
        &std::fs::read_to_string(&sp).with_context(|| format!("reading `{}`", sp.display()))?,
    )
    .with_context(|| format!("parsing `{}`", sp.display()))?;
    warn(drop_warning(&stem));
    git_delete_ref(&repo, &record.git_ref)?;
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
