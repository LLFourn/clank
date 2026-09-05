//! Shared history-rewriting engine for `clank purge` and
//! `clank finish --purge`. Consumes the daemon's typed
//! `RewritePreviewResponse` and applies it via git plumbing.
//!
//! The engine never re-derives Clank attribution or classification —
//! the daemon's manifest IS the rewrite plan. This module is
//! pure side-effect-producing infrastructure (git plumbing,
//! ref updates, tree builds).

use std::collections::HashSet;
use std::path::Path;

use crate::shell_quote::shell_quote;

use clank_core::api::{RewriteCommit, RewriteDisposition};
use clank_core::ids::CommitSha;

use crate::git_plumbing::{self, ExpectedRef};

/// Engine inputs. Unpacked fields so both single-plan and all-plans
/// preview responses can feed the same engine. `dry == true` makes
/// the engine compute everything up to the first `commit-tree` /
/// `update-ref` call and then exit without producing any git
/// objects or moving refs.
#[derive(Debug)]
pub struct RewriteOpts<'a> {
    pub repo: &'a Path,
    /// Earliest commit in the rewrite range. `None` is treated as
    /// "nothing to rewrite" — the engine refuses; callers should
    /// short-circuit before invoking.
    pub intro_sha: Option<&'a CommitSha>,
    /// Branch tip the preview was computed against. Used as the
    /// expected-old-value for the conditional in-place update-ref.
    pub head_sha: &'a CommitSha,
    /// False if the range contains a merge commit. Engine refuses
    /// in that case.
    pub linear: bool,
    /// Per-commit manifest, in chronological order.
    pub commits: &'a [RewriteCommit],
    /// `None` rewrites the current branch in place; `Some(name)`
    /// writes the rewritten chain to a fresh branch. Refuses if the
    /// branch already exists.
    pub into_branch: Option<&'a str>,
    pub dry: bool,
    /// `Some(msg)` collapses the squash range into one commit on top of
    /// `intro_parent`, with the supplied message. Refuses if any commit in
    /// the COLLAPSED range is marked `foreign` (a squash can't selectively
    /// preserve interleaved foreign work). The resulting tree is the squash
    /// tip's tree minus `head_strip_paths` — Drop commits don't carry
    /// per-commit `strip_paths`, so unioning the manifest's strip_paths
    /// would leak content the per-commit replay would have removed by
    /// skipping.
    pub squash: Option<&'a str>,
    /// Where the squash COLLAPSE ends. `Some(sha)` (a finished plan's
    /// finalize commit) collapses `[intro..sha]` and RESTACKS every later
    /// step individually on top, per its manifest disposition — so purge
    /// stripping applies to restacked trees too. `None` collapses the whole
    /// range to `head_sha` (an active plan). Ignored when not squashing.
    pub squash_tip: Option<&'a CommitSha>,
    /// Strippable paths at the squash tip's tree (`squash_tip` when set,
    /// else `head_sha`; from the preview). The squash mode's source of truth
    /// for what to strip from the collapsed tree. Empty when not squashing.
    pub head_strip_paths: &'a [String],
}

#[derive(Debug, Default)]
pub struct RewriteOutcome {
    /// SHA of the new tip after rewriting. `None` in dry mode.
    pub new_tip: Option<String>,
    /// Branch the engine updated. `None` in dry mode.
    pub updated_branch: Option<String>,
    /// `(old, new)` sha pairs for every commit the rewrite replayed or
    /// collapsed (a squash maps each collapsed old to the one squash
    /// commit). Callers migrate sha-keyed state (review feedback) with
    /// these — a plumbing `update-ref` fires no `post-rewrite` hook.
    /// Empty in dry mode.
    pub pairs: Vec<(CommitSha, CommitSha)>,
}

/// Drive the rewrite. Engine refuses pre-flight on:
/// - non-linear range (merge commit in [`intro_sha`, `head_sha`])
/// - dirty working tree
/// - `--into-branch` pointing at an existing branch
///
/// In `--dry` mode the engine prints the planned listing FIRST,
/// then reports any blockers as a footer — the operator wants the
/// commit-by-commit shape even when a blocker would prevent the
/// live run.
pub async fn run(opts: RewriteOpts<'_>) -> anyhow::Result<RewriteOutcome> {
    // Build the execution plan from inputs we always have (commits
    // + maybe-intro_parent). Pre-flight checks below decide whether
    // to enforce or just report.
    let intro_parent = if let Some(intro) = opts.intro_sha {
        parent_of(opts.repo, intro.as_str())?
    } else {
        None
    };
    let plan = build_plan(opts.commits, intro_parent.as_deref());

    let blockers = collect_blockers(&opts)?;

    // The squash COLLAPSE covers steps[..=boundary]; steps after it are
    // restacked individually. `squash_tip: None` → the whole range (an
    // active plan collapsing to HEAD). An EMPTY range keeps boundary None
    // so the "rewrite range is empty" blocker reports (dry prints it, live
    // bails) instead of the slice math panicking (codex eeec2de).
    let boundary = match (
        opts.squash.is_some() && !plan.steps.is_empty(),
        opts.squash_tip,
    ) {
        (true, Some(tip)) => Some(
            plan.steps
                .iter()
                .position(|s| s.sha == tip.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "squash tip {} is not in the rewrite range — preview and \
                         engine disagree; re-run to refresh the preview",
                        short(tip.as_str()),
                    )
                })?,
        ),
        (true, None) => Some(plan.steps.len() - 1),
        (false, _) => None,
    };

    // Squash adds one more blocker: foreign commits INSIDE the collapsed
    // range (a squash can't selectively preserve interleaved foreign work —
    // replay uses full trees, so replaying an interleaved commit after the
    // squash would reset the plan's later changes). Commits AFTER the
    // squash tip are restack steps and never trip this. Name the offenders
    // so the operator knows what to re-tag or move.
    let mut blockers = blockers;
    if let Some(boundary) = boundary {
        // boundary < steps.len() by construction (a position() hit, or
        // len-1 of a non-empty list), so the inclusive slice is in range.
        let offenders: Vec<String> = plan.steps[..=boundary]
            .iter()
            .filter(|s| s.foreign)
            .map(|s| format!("{} {}", short(&s.sha), s.subject))
            .collect();
        if !offenders.is_empty() {
            blockers.push(format!(
                "--squash refuses {} foreign commit(s) interleaved within the \
                 plan's own range:\n    {}\n  Re-tag or move them, or use \
                 `clank purge` without `--squash` first, then retry.",
                offenders.len(),
                offenders.join("\n    "),
            ));
        }
    }

    if opts.dry {
        if let Some(msg) = opts.squash {
            print_squash_dry_run(&opts, &plan, boundary, &blockers, msg);
        } else {
            print_rebase_todo(&opts, &plan, &blockers);
        }
        return Ok(RewriteOutcome::default());
    }

    if let Some(first) = blockers.first() {
        // Live run: bail on the first blocker with the same
        // message --dry would have reported.
        anyhow::bail!("{first}");
    }
    // Re-prove intro exists since the live path uses it for the
    // conditional ref update — the blockers list already caught
    // the None case, this re-check is just for the type system.
    let intro = opts
        .intro_sha
        .expect("blockers would have caught a missing intro");
    let _ = intro;

    let mut pairs: Vec<(CommitSha, CommitSha)> = Vec::new();
    let new_tip = if let Some(message) = opts.squash {
        // None only for an empty range, which the empty-range blocker
        // already bailed on above.
        let boundary = boundary.expect("empty squash range is caught by blockers");
        let squash_source = opts.squash_tip.unwrap_or(opts.head_sha);
        let squashed = apply_squash(
            opts.repo,
            squash_source,
            opts.head_strip_paths,
            intro_parent.as_deref(),
            message,
        )
        .await?;
        let squashed_sha = parse_rewritten(&squashed)?;
        // Every collapsed old maps to the one squash commit (the feedback
        // migrator keeps the LATEST old per new — the finalize commit's).
        for step in &plan.steps[..=boundary] {
            pairs.push((parse_rewritten(&step.sha)?, squashed_sha.clone()));
        }
        // RESTACK the steps after the squash tip individually, per their
        // manifest disposition — purge stripping applies to their trees too.
        let mut parent = squashed;
        for step in &plan.steps[boundary + 1..] {
            let replayed = match step.disposition {
                RewriteDisposition::Drop => None,
                RewriteDisposition::KeepVerbatim => {
                    let tree = git_plumbing::commit_tree_oid(opts.repo, &step.sha)?;
                    Some(git_plumbing::replay_commit(
                        opts.repo,
                        &step.sha,
                        &tree,
                        Some(&parent),
                    )?)
                }
                RewriteDisposition::Rewrite => {
                    let tree = git_plumbing::strip_tree(opts.repo, &step.sha, &step.strip_paths)?;
                    Some(git_plumbing::replay_commit(
                        opts.repo,
                        &step.sha,
                        &tree,
                        Some(&parent),
                    )?)
                }
            };
            if let Some(new) = replayed {
                pairs.push((parse_rewritten(&step.sha)?, parse_rewritten(&new)?));
                parent = new;
            }
        }
        parent
    } else {
        apply_plan(opts.repo, &plan, intro_parent.as_deref(), &mut pairs).await?
    };
    let updated_branch = match opts.into_branch {
        Some(name) => {
            // Create-or-fail: refuse if the ref already exists (atomic
            // — closes the `branch_exists` → create race).
            git_plumbing::update_ref(
                opts.repo,
                &format!("refs/heads/{name}"),
                &new_tip,
                ExpectedRef::CreateOnly,
            )?;
            name.to_string()
        }
        None => {
            let current = current_branch(opts.repo)?;
            // Conditional update: the expected old value is the head we
            // previewed against. If the branch moved between preview
            // and now, refuse — we'd otherwise silently throw away
            // commits that arrived after preview.
            git_plumbing::update_ref(
                opts.repo,
                &format!("refs/heads/{current}"),
                &new_tip,
                ExpectedRef::Match(opts.head_sha.as_str().to_string()),
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "branch `{current}` moved between preview and rewrite \
                     (expected {}); aborting to avoid losing commits — re-run \
                     to refresh the preview. underlying: {e}",
                    short(opts.head_sha.as_str()),
                )
            })?;
            // Re-sync the worktree to the new tip. Kept on git: a
            // worktree-state checkout whose exact semantics
            // (gitignore/fileMode/autocrlf) must match git's.
            git_plumbing::reset_hard(opts.repo, "HEAD")?;
            current
        }
    };

    Ok(RewriteOutcome {
        new_tip: Some(new_tip),
        updated_branch: Some(updated_branch),
        pairs,
    })
}

fn parse_rewritten(sha: &str) -> anyhow::Result<CommitSha> {
    CommitSha::parse(sha).map_err(|e| anyhow::anyhow!("parse rewritten sha `{sha}`: {e}"))
}

/// Inputs for [`reword_in_place`].
#[derive(Debug)]
pub struct RewordOpts<'a> {
    pub repo: &'a Path,
    /// The commit whose MESSAGE is being replaced. May be buried under
    /// later first-parent commits (the off-HEAD case) — those descendants
    /// are replayed on top of the reworded commit.
    pub target_sha: &'a CommitSha,
    /// Branch tip the reword was computed against — the expected-old-value
    /// for the conditional in-place ref update.
    pub head_sha: &'a CommitSha,
    /// BYTES, not `&str`: a message may be invalid UTF-8 and a reword
    /// that preserves it must not launder it through a lossy decode.
    pub new_message: &'a [u8],
    /// Force the rewritten target to be a DIFFERENT commit even when
    /// the message is unchanged. Off by default: a reword normally
    /// changes the message, so the sha changes anyway, and pinning the
    /// committer date keeps re-squashing idempotent. `rereview` sets
    /// it, since minting a fresh review target is its whole purpose.
    pub distinct_target: bool,
    pub dry: bool,
}

/// Reword `target_sha` to `new_message` and replay every first-parent
/// descendant up to `head_sha` on top, then move the current branch — the
/// off-HEAD counterpart to `git commit --amend`. The target's tree, parent,
/// and author are preserved (only the message changes); descendants keep their
/// trees, messages, and authors (author dates intact).
///
/// Returns the `(old, new)` sha pairs for the reworded target and every
/// replayed descendant (empty in `--dry`), so the caller can migrate
/// sha-keyed state (e.g. review feedback — the git `post-rewrite` hook does
/// NOT fire for a plumbing `update-ref`).
///
/// Refuses on: a merge commit in `[target, head]` (first-parent replay would
/// drop a parent), a dirty working tree, or `target_sha` not being on
/// `head_sha`'s first-parent chain.
pub async fn reword_in_place(opts: RewordOpts<'_>) -> anyhow::Result<Vec<(CommitSha, CommitSha)>> {
    // The target must sit on head's first-parent chain, else the descendant
    // walk (anchored by hiding `target`) would not be relative to it.
    let on_chain = crate::git_io::first_parent_chain_find_at(
        opts.repo,
        opts.head_sha,
        &HashSet::from([opts.target_sha.clone()]),
    )?;
    if on_chain.as_ref() != Some(opts.target_sha) {
        anyhow::bail!(
            "commit {} is not on the current branch's first-parent history; \
             cannot reword it in place",
            short(opts.target_sha.as_str()),
        );
    }
    // Descendants: first-parent commits strictly after target, up to head.
    let descendants =
        crate::git_io::first_parent_commits_between(opts.repo, opts.target_sha, opts.head_sha)?;

    let mut blockers = Vec::new();
    // A merge anywhere in [target, head] means first-parent replay would drop a
    // second parent — refuse rather than silently lose history.
    let mut merges = Vec::new();
    for sha in std::iter::once(opts.target_sha).chain(descendants.iter().map(|m| &m.sha)) {
        if crate::git_io::commit_parent_count_at(opts.repo, sha)? > 1 {
            merges.push(short(sha.as_str()));
        }
    }
    if !merges.is_empty() {
        blockers.push(format!(
            "range contains merge commit(s) ({}); refusing to reword (first-parent \
             replay would drop a parent)",
            merges.join(", "),
        ));
    }
    if working_tree_dirty(opts.repo)? {
        blockers.push("working tree dirty; commit or stash first".to_string());
    }

    if opts.dry {
        println!("# clank reword preview");
        println!(
            "# target {} → new message; {} descendant commit(s) replayed on top",
            short(opts.target_sha.as_str()),
            descendants.len(),
        );
        for b in &blockers {
            println!("#   BLOCKER: {b}");
        }
        return Ok(Vec::new());
    }
    if let Some(first) = blockers.first() {
        anyhow::bail!("{first}");
    }

    // Rebuild the target with the new message (tree/parent/author preserved via
    // `squash_commit`), then replay each descendant on top.
    let target_tree = git_plumbing::commit_tree_oid(opts.repo, opts.target_sha.as_str())?;
    let target_parent = crate::git_io::parent_of_at(opts.repo, opts.target_sha)?;
    let new_target = if opts.distinct_target {
        git_plumbing::recommit_distinct(
            opts.repo,
            opts.target_sha.as_str(),
            &target_tree,
            target_parent.as_ref().map(CommitSha::as_str),
            opts.new_message,
        )?
    } else {
        git_plumbing::squash_commit(
            opts.repo,
            opts.target_sha.as_str(),
            &target_tree,
            target_parent.as_ref().map(CommitSha::as_str),
            opts.new_message,
        )?
    };
    let parse_new = |s: &str| {
        CommitSha::parse(s).map_err(|e| anyhow::anyhow!("parse rewritten sha `{s}`: {e}"))
    };
    let mut pairs = vec![(opts.target_sha.clone(), parse_new(&new_target)?)];
    let mut tip = new_target;
    for d in &descendants {
        let tree = git_plumbing::commit_tree_oid(opts.repo, d.sha.as_str())?;
        let new = git_plumbing::replay_commit(opts.repo, d.sha.as_str(), &tree, Some(&tip))?;
        pairs.push((d.sha.clone(), parse_new(&new)?));
        tip = new;
    }

    // Move the current branch in place, guarding against a concurrent update.
    let current = current_branch(opts.repo)?;
    git_plumbing::update_ref(
        opts.repo,
        &format!("refs/heads/{current}"),
        &tip,
        ExpectedRef::Match(opts.head_sha.as_str().to_string()),
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "branch `{current}` moved between preview and reword (expected {}); \
             aborting to avoid losing commits — re-run. underlying: {e}",
            short(opts.head_sha.as_str()),
        )
    })?;
    // Re-sync the worktree to the new tip (trees are identical, so this is a
    // no-op content-wise — it just keeps HEAD/index/worktree coherent).
    git_plumbing::reset_hard(opts.repo, "HEAD")?;

    Ok(pairs)
}

#[derive(Debug)]
struct PlanStep {
    sha: String,
    subject: String,
    disposition: RewriteDisposition,
    foreign: bool,
    strip_paths: Vec<String>,
}

#[derive(Debug)]
struct ExecutionPlan {
    intro_parent: Option<String>,
    steps: Vec<PlanStep>,
}

fn build_plan(commits: &[RewriteCommit], intro_parent: Option<&str>) -> ExecutionPlan {
    ExecutionPlan {
        intro_parent: intro_parent.map(str::to_string),
        steps: commits
            .iter()
            .map(|c| PlanStep {
                sha: c.sha.as_str().to_string(),
                subject: c.subject.clone(),
                disposition: c.disposition,
                foreign: c.foreign,
                strip_paths: c.strip_paths.clone(),
            })
            .collect(),
    }
}

/// Pre-flight diagnostics. Live run bails on the first; `--dry`
/// reports the full list as a footer after the listing.
fn collect_blockers(opts: &RewriteOpts<'_>) -> anyhow::Result<Vec<String>> {
    let mut blockers = Vec::new();
    if !opts.linear {
        blockers.push(
            "range contains a merge commit; refusing to rewrite (merge-tree \
             rewriting is out of scope)"
                .to_string(),
        );
    }
    if opts.intro_sha.is_none() {
        blockers.push("rewrite range is empty; nothing to do".to_string());
    }
    if working_tree_dirty(opts.repo)? {
        blockers.push("working tree dirty; commit or stash first".to_string());
    }
    if let Some(branch) = opts.into_branch
        && branch_exists(opts.repo, branch)?
    {
        blockers.push(format!(
            "branch `{branch}` already exists; refusing to overwrite. \
             Pick a different name or delete it first."
        ));
    }
    Ok(blockers)
}

/// Emit a `git rebase --interactive` todo list. The operator can
/// pipe it to git via `GIT_SEQUENCE_EDITOR='cp <file>' git rebase
/// --interactive --keep-empty <intro>^`, or just read it as a
/// preview. When ANY blocker is present every todo command is
/// commented out so the file isn't pipeable unchanged — `git`
/// ignores `#` lines, so a blocked output that left commands
/// uncommented would let the operator execute a rewrite the live
/// command would refuse.
fn print_rebase_todo(opts: &RewriteOpts<'_>, plan: &ExecutionPlan, blockers: &[String]) {
    let cmt = if blockers.is_empty() { "" } else { "# " };

    println!("# clank rewrite preview");
    if let Some(intro) = opts.intro_sha {
        println!(
            "# range: {}..{} ({} commits in rewrite range)",
            short(intro.as_str()),
            short(opts.head_sha.as_str()),
            plan.steps.len(),
        );
    } else {
        println!("# range: (empty — no .clank/ history)");
    }
    println!(
        "# starting parent: {}",
        plan.intro_parent.as_deref().unwrap_or("(root)"),
    );
    let target = match opts.into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch untouched)"),
        None => "the CURRENT branch (in place)".into(),
    };
    println!("# target: {target}");
    println!("#");
    if !blockers.is_empty() {
        println!("# BLOCKERS (live run would refuse):");
        for b in blockers {
            println!("#   - {b}");
        }
        println!("#");
        println!("# Every todo command below is commented out because of");
        println!("# the blockers above. Address them, re-run `--dry`,");
        println!("# and only then pipe to git.");
        println!("#");
    } else {
        println!("# Run with:");
        println!("#   GIT_SEQUENCE_EDITOR='cp <this-file>' \\");
        let intro_anchor = opts
            .intro_sha
            .map(|s| short(s.as_str()))
            .unwrap_or_else(|| "<intro>".into());
        println!("#     git rebase --interactive --keep-empty {intro_anchor}^");
        println!("#");
        println!("# Caveats:");
        println!("#   - `--into-branch` isn't supported by git rebase. To");
        println!("#     preview on a side branch: `git checkout -b scratch`");
        println!("#     before running the rebase.");
        println!("#   - No `<expected-old>` ref-update guard like the live");
        println!("#     engine. Don't race other branch updates.");
        println!("#   - `--keep-empty` is required so commits that become");
        println!("#     empty after strip stay visible for review.");
        println!("#");
    }

    let foreign_count = plan.steps.iter().filter(|s| s.foreign).count();
    if foreign_count > 0 {
        println!("# {foreign_count} commit(s) in the range are foreign to this plan");
        println!("# (they appear here so the parent chain is contiguous).");
        println!("#");
    }

    for step in &plan.steps {
        let foreign_tag = if step.foreign { " [foreign]" } else { "" };
        match step.disposition {
            RewriteDisposition::Drop => {
                println!(
                    "{cmt}drop  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
            }
            RewriteDisposition::KeepVerbatim => {
                println!(
                    "{cmt}pick  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
            }
            RewriteDisposition::Rewrite => {
                println!(
                    "{cmt}edit  {} {}{}",
                    short(&step.sha),
                    step.subject,
                    foreign_tag
                );
                let joined = step
                    .strip_paths
                    .iter()
                    .map(|p| shell_quote(p))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{cmt}# strip: {}", step.strip_paths.join(", "));
                println!("{cmt}# run: git rm --cached {joined} && \\");
                println!("{cmt}#      git commit --amend --no-edit --allow-empty && \\");
                println!("{cmt}#      git rebase --continue");
            }
        }
    }
}

/// Squash-mode dry-run: emit a description, NOT a rebase-todo.
/// The collapsed-into-one shape doesn't map cleanly to git
/// rebase's directives, so we present the target as a one-commit
/// summary that's explicitly not pipeable to git. Operator runs
/// the live command if they want to execute it.
fn print_squash_dry_run(
    opts: &RewriteOpts<'_>,
    plan: &ExecutionPlan,
    boundary: Option<usize>,
    blockers: &[String],
    msg: &str,
) {
    let collapse_end = boundary.unwrap_or(plan.steps.len().saturating_sub(1));
    let restacked = plan.steps.len().saturating_sub(collapse_end + 1);
    println!("# clank rewrite --squash preview");
    if let Some(intro) = opts.intro_sha {
        let tip = opts.squash_tip.unwrap_or(opts.head_sha);
        println!(
            "# range: {}..{} ({} commit(s) would collapse into one)",
            short(intro.as_str()),
            short(tip.as_str()),
            collapse_end + 1,
        );
        if restacked > 0 {
            println!("# restacked on top: {restacked} later commit(s), preserved individually");
        }
    } else {
        println!("# range: (empty — no .clank/ history)");
    }
    println!(
        "# starting parent: {}",
        plan.intro_parent.as_deref().unwrap_or("(root)"),
    );
    let target = match opts.into_branch {
        Some(b) => format!("a NEW branch `{b}` (current branch untouched)"),
        None => "the CURRENT branch (in place)".into(),
    };
    println!("# target: {target}");
    println!("#");
    println!("# would create ONE commit on top of starting parent:");
    println!("#   message: {msg}");
    let tree_src = if opts.squash_tip.is_some() {
        "the finalize commit's tree"
    } else {
        "HEAD's tree"
    };
    if opts.head_strip_paths.is_empty() {
        println!("#   tree: {tree_src}, unchanged");
    } else {
        println!("#   tree: {tree_src} MINUS:");
        for p in opts.head_strip_paths {
            println!("#     - {p}");
        }
    }
    println!("#");
    if !blockers.is_empty() {
        println!("# BLOCKERS (live run would refuse):");
        for b in blockers {
            println!("#   - {b}");
        }
        println!("#");
    }
    println!("# Squash mode is NOT pipeable to git rebase. Run the live command");
    println!("# without --dry to materialize the squashed commit.");
}

/// Walk `plan.steps` in order, producing a new tip SHA. When every
/// step is `Drop`, the branch should move to `intro_parent` — that's
/// "plan abandoned mid-flight, leave no trace." When `intro_parent`
/// is `None` (intro is the root commit), there's no valid SHA to
/// point the branch at, so we error with a clear message: the
/// operator must keep at least one commit or delete the branch
/// outright.
async fn apply_plan(
    repo: &Path,
    plan: &ExecutionPlan,
    intro_parent: Option<&str>,
    pairs: &mut Vec<(CommitSha, CommitSha)>,
) -> anyhow::Result<String> {
    let mut parent: Option<String> = plan.intro_parent.clone();
    let mut produced_any = false;
    for step in &plan.steps {
        match step.disposition {
            RewriteDisposition::Drop => {
                // Parent chain hops over this commit.
            }
            RewriteDisposition::KeepVerbatim => {
                let tree = git_plumbing::commit_tree_oid(repo, &step.sha)?;
                let new_sha =
                    git_plumbing::replay_commit(repo, &step.sha, &tree, parent.as_deref())?;
                pairs.push((parse_rewritten(&step.sha)?, parse_rewritten(&new_sha)?));
                parent = Some(new_sha);
                produced_any = true;
            }
            RewriteDisposition::Rewrite => {
                let new_tree = git_plumbing::strip_tree(repo, &step.sha, &step.strip_paths)?;
                let new_sha =
                    git_plumbing::replay_commit(repo, &step.sha, &new_tree, parent.as_deref())?;
                pairs.push((parse_rewritten(&step.sha)?, parse_rewritten(&new_sha)?));
                parent = Some(new_sha);
                produced_any = true;
            }
        }
    }
    if !produced_any {
        match intro_parent {
            Some(p) => Ok(p.to_string()),
            None => anyhow::bail!(
                "every commit in range would be dropped AND the plan's intro is a root commit — \
                 cannot point a branch at 'nothing'. Use `git branch -D` to delete the branch \
                 outright if that's what you want.",
            ),
        }
    } else {
        parent.ok_or_else(|| anyhow::anyhow!("rewrite produced commits but lost the tip"))
    }
}

/// Collapse the squash range into ONE commit on top of `intro_parent`.
/// `source_sha` is the squash tip (a buried plan's finalize commit, or HEAD
/// for an active plan): its tree — minus the strip set — is the collapsed
/// tree (the plan's cumulative end state), and its author/date seed the
/// commit. Foreign-commit refusal lives in `run()`'s blocker check; this
/// function assumes the range is clean.
async fn apply_squash(
    repo: &Path,
    source_sha: &CommitSha,
    strip_paths: &[String],
    intro_parent: Option<&str>,
    message: &str,
) -> anyhow::Result<String> {
    // The strip set comes from the preview, computed at the squash tip's
    // tree — NOT the union of per-commit strip_paths. Per-commit strips
    // only fire for `Rewrite`, but `Drop` commits add content that would
    // be removed by skipping them. For squash, we collapse to a single
    // commit, so the strip set must reflect every path the per-commit
    // replay would have removed as of the squash tip.
    let strip: Vec<String> = strip_paths.to_vec();
    // Build the squashed tree from the tip's tree (the union of everything
    // the collapsed range produced) with the strip paths removed. Paths in
    // the strip set but absent from the tree are no-ops — added-then-deleted
    // in the range, already gone.
    let src = source_sha.as_str();
    let new_tree = git_plumbing::strip_tree(repo, src, &strip)?;
    // `squash_commit` carries the idempotence above: the tip's author is
    // preserved and the committer date is pinned to it, so re-squashing
    // reproduces the same sha (finish-squash-idempotent-on-finished).
    git_plumbing::squash_commit(repo, src, &new_tree, intro_parent, message.as_bytes())
}

pub(crate) fn working_tree_dirty(repo: &Path) -> anyhow::Result<bool> {
    let status = crate::git_io::working_tree_status(repo)?;
    // Clank's OWN untracked scratch under `.clank/` (cache, agent
    // configs, queue, a freshly-written `.gitignore`, …) is local
    // runtime state, not the operator's work — it must NOT block a
    // history rewrite (rewrite-scratch-dir-worktree). It's also
    // irrelevant to the rewrite, which only edits COMMITTED history.
    // Tracked modifications anywhere (incl. committed `.clank/` plan
    // files) and untracked files OUTSIDE `.clank/` still count.
    // gix's dirwalk COLLAPSES a wholly-untracked `.clank/` to a single
    // entry `.clank` (no trailing slash), where `git status --porcelain`
    // emitted `.clank/` — so match both the dir itself and its contents.
    let under_clank = |p: &str| p == ".clank" || p.starts_with(".clank/");
    let untracked_outside_clank = status.untracked.iter().any(|p| !under_clank(p));
    Ok(!status.changed.is_empty() || untracked_outside_clank)
}

fn branch_exists(repo: &Path, branch: &str) -> anyhow::Result<bool> {
    Ok(crate::git_io::resolve_commit(repo, &format!("refs/heads/{branch}")).is_some())
}

fn parent_of(repo: &Path, sha: &str) -> anyhow::Result<Option<String>> {
    let sha = CommitSha::parse(sha).map_err(|e| anyhow::anyhow!("parse sha `{sha}`: {e}"))?;
    // `git_io::parent_of` returns `None` for a root commit, matching
    // the old `rev-parse <sha>^` failure.
    Ok(crate::git_io::parent_of_at(repo, &sha)?.map(|p| p.as_str().to_string()))
}

fn current_branch(repo: &Path) -> anyhow::Result<String> {
    crate::git_io::current_branch_at(repo)?
        .ok_or_else(|| anyhow::anyhow!("HEAD is detached; cannot determine the branch to rewrite"))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::api::{RewriteCommit, RewriteDisposition, RewritePreviewResponse};
    use clank_core::ids::{CommitSha, PlanKey};

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        run(dir.path(), &["config", "user.email", "test@test"]);
        run(dir.path(), &["config", "user.name", "test"]);
        run(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) -> String {
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "--quiet", "-m", msg]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn show_subject(repo: &Path, sha: &str) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", "-s", "--format=%s", sha])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn rev_list(repo: &Path, branch: &str) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-list", "--reverse", branch])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn tree_has(repo: &Path, sha: &str, path: &str) -> bool {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-tree", "-r", "--name-only", sha])
            .output()
            .unwrap();
        let listing = String::from_utf8(out.stdout).unwrap();
        listing.lines().any(|l| l == path)
    }

    fn mk_preview(
        intro: &str,
        head: &str,
        commits: Vec<(String, String, RewriteDisposition, bool, Vec<String>)>,
    ) -> RewritePreviewResponse {
        RewritePreviewResponse {
            plan_id: "clank/foo.md".into(),
            plan_stem: PlanKey::parse("foo").unwrap(),
            intro_sha: Some(CommitSha::parse(intro).unwrap()),
            head_sha: CommitSha::parse(head).unwrap(),
            linear: true,
            squash_tip: None,
            head_strip_paths: Vec::new(),
            commits: commits
                .into_iter()
                .map(|(sha, subj, disp, foreign, strip)| RewriteCommit {
                    sha: CommitSha::parse(&sha).unwrap(),
                    subject: subj,
                    disposition: disp,
                    foreign,
                    strip_paths: strip,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn rewrite_into_branch_strips_plan_file_from_mixed_commit() {
        let dir = init_repo();
        // First-ever commit so the engine has an intro_parent of None.
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".clank/plans/foo.md", "# foo v2\n");
        write(dir.path(), "src/lib.rs", "// code\n");
        let mixed = commit(dir.path(), "mixed: revise foo + code");

        let preview = mk_preview(
            &intro,
            &mixed,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    mixed.clone(),
                    "mixed: revise foo + code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".clank/plans/foo.md".into()],
                ),
            ],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("purged"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();
        let new_tip = outcome.new_tip.unwrap();
        assert_eq!(outcome.updated_branch.as_deref(), Some("purged"));

        // The rewritten chain on `purged` should have exactly one new
        // commit after the seed (because intro was Drop).
        let chain = rev_list(dir.path(), "purged");
        assert_eq!(chain.len(), 2, "seed + one rewritten commit; got {chain:?}");
        assert_eq!(chain.last().unwrap(), &new_tip);
        // The rewritten tip preserves the original subject.
        assert_eq!(
            show_subject(dir.path(), &new_tip),
            "mixed: revise foo + code"
        );
        // ...but its tree omits .clank/plans/foo.md and keeps src/lib.rs.
        assert!(!tree_has(dir.path(), &new_tip, ".clank/plans/foo.md"));
        assert!(tree_has(dir.path(), &new_tip, "src/lib.rs"));
        // The original `main` branch is untouched.
        let main_chain = rev_list(dir.path(), "main");
        assert!(main_chain.contains(&mixed));
    }

    #[tokio::test]
    async fn rewrite_dry_run_makes_no_commits_or_refs() {
        let dir = init_repo();
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("dry-target"),
            dry: true,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();
        assert!(outcome.new_tip.is_none());
        assert!(outcome.updated_branch.is_none());
        // The branch was NOT created.
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/dry-target"])
            .status()
            .unwrap();
        assert!(!out.success(), "dry-run must not create the target branch");
    }

    #[tokio::test]
    async fn rewrite_refuses_existing_into_branch() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        run(dir.path(), &["branch", "already-exists"]);
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("already-exists"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("already-exists"), "got: {msg}");
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    #[tokio::test]
    async fn rewrite_all_drop_points_branch_at_intro_parent() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".clank/plans/foo.md", "# foo v2\n");
        let revision = commit(dir.path(), "plan: foo v2");

        let preview = mk_preview(
            &intro,
            &revision,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    revision.clone(),
                    "plan: foo v2".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
            ],
        );
        let outcome = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("purged"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();
        assert_eq!(outcome.new_tip.as_deref(), Some(seed.as_str()));
        let chain = rev_list(dir.path(), "purged");
        assert_eq!(chain, vec![seed]);
    }

    #[tokio::test]
    async fn rewrite_conditional_in_place_update_refuses_if_branch_moved() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");

        // Simulate: preview was fetched against `intro`, then a new
        // commit landed.
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let raced_commit = commit(dir.path(), "raced commit after preview");

        let preview = mk_preview(
            &intro,
            &intro, // stale head_sha
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );

        let err = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: None, // in-place — triggers the conditional update
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("moved between preview"), "got: {msg}");

        // Branch tip is still the raced commit; the engine did not
        // clobber it.
        let chain = rev_list(dir.path(), "main");
        assert_eq!(chain.last().unwrap(), &raced_commit);
    }

    /// End-to-end single-plan purge: codex's specific request.
    /// `clank purge foo --into-branch scrubbed` on
    /// `seed → plan → code` must produce a `scrubbed` branch that
    /// contains `src/main.rs` and does NOT contain
    /// `.clank/plans/foo.md`. The previous diff-touch
    /// classification would have marked `code` as KeepVerbatim and
    /// leaked the inherited plan file.
    #[tokio::test]
    async fn rewrite_single_plan_strips_inherited_plan_file() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "later code");

        // Manifest as the (corrected) single-plan endpoint would
        // emit it: intro Drop, code Rewrite with strip_paths=[foo.md].
        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    code.clone(),
                    "later code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".clank/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();
        assert!(
            !tree_has(dir.path(), "scrubbed", ".clank/plans/foo.md"),
            "inherited plan file leaked into scrubbed branch"
        );
        assert!(tree_has(dir.path(), "scrubbed", "src/main.rs"));
    }

    /// End-to-end: simulate an all-plans purge across an intro
    /// commit + a pure-code commit. Asserts that with strip_paths
    /// populated on the KeepVerbatim-equivalent step, the
    /// rewritten branch's final tree does NOT contain the plan
    /// file even though that commit's diff didn't touch it.
    #[tokio::test]
    async fn rewrite_strips_inherited_clank_from_later_pure_code() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "later code");

        // Manifest mirrors what api_rewrite_preview_all would
        // emit with tree-based classification: intro Drops, the
        // pure-code commit Rewrites with the inherited
        // .clank/plans/foo.md in strip_paths.
        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    code.clone(),
                    "later code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".clank/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();

        // The scrubbed branch's tip must contain src/main.rs but
        // NOT .clank/plans/foo.md.
        assert!(
            !tree_has(dir.path(), "scrubbed", ".clank/plans/foo.md"),
            "plan file leaked into rewritten branch"
        );
        assert!(
            tree_has(dir.path(), "scrubbed", "src/main.rs"),
            "code file lost from rewritten branch"
        );
        assert!(
            tree_has(dir.path(), "scrubbed", "README.md"),
            "seed file lost from rewritten branch"
        );
    }

    #[tokio::test]
    async fn apply_squash_is_idempotent() {
        // Re-squashing an already-squashed commit MUST reproduce the
        // same sha (finish-squash-idempotent-on-finished): the gix
        // commit object pins the committer date to the (preserved)
        // author date, so tree + parents + author + committer are all
        // deterministic across runs.
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "base.txt", "base\n");
        let base = commit(repo, "base");
        write(repo, "feature.txt", "f\n");
        let c = commit(repo, "feature work");

        let s1 = apply_squash(
            repo,
            &CommitSha::parse(&c).unwrap(),
            &[],
            Some(&base),
            "squashed",
        )
        .await
        .unwrap();
        let s2 = apply_squash(
            repo,
            &CommitSha::parse(&s1).unwrap(),
            &[],
            Some(&base),
            "squashed",
        )
        .await
        .unwrap();
        assert_eq!(s1, s2, "re-squash must reproduce the same sha");
    }

    #[tokio::test]
    async fn rewrite_squash_collapses_range_into_one_commit() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        write(dir.path(), ".clank/plans/foo.md", "# foo v2\n");
        write(dir.path(), "src/lib.rs", "// code v1\n");
        let _mixed = commit(dir.path(), "plan revision + code");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        let code = commit(dir.path(), "more code");

        let preview = mk_preview(
            &intro,
            &code,
            vec![
                (
                    intro.clone(),
                    "plan: foo".into(),
                    RewriteDisposition::Drop,
                    false,
                    vec![],
                ),
                (
                    _mixed.clone(),
                    "plan revision + code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".clank/plans/foo.md".into()],
                ),
                (
                    code.clone(),
                    "more code".into(),
                    RewriteDisposition::Rewrite,
                    false,
                    vec![".clank/plans/foo.md".into()],
                ),
            ],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("squashed"),
            dry: false,

            squash: Some("Implement foo"),
            squash_tip: None,
            head_strip_paths: &[".clank/plans/foo.md".to_string()],
        })
        .await
        .unwrap();

        // The squashed branch should be: seed → ONE squash commit
        // containing src/lib.rs + src/main.rs + README.md but no
        // .clank/plans/foo.md.
        let chain = rev_list(dir.path(), "squashed");
        assert_eq!(chain.len(), 2, "seed + one squash commit; got {chain:?}");
        let squash_tip = chain.last().unwrap();
        assert_eq!(show_subject(dir.path(), squash_tip), "Implement foo");
        assert!(tree_has(dir.path(), squash_tip, "src/lib.rs"));
        assert!(tree_has(dir.path(), squash_tip, "src/main.rs"));
        assert!(tree_has(dir.path(), squash_tip, "README.md"));
        assert!(!tree_has(dir.path(), squash_tip, ".clank/plans/foo.md"));
    }

    #[tokio::test]
    async fn rewrite_squash_strips_drop_only_paths_via_head_strip() {
        // The codex-flagged Drop-only leak: a range of Drop-only
        // commits has empty per-commit strip_paths, so the OLD
        // squash logic (union of per-step strips) would strip
        // nothing and leak the plan file into the squashed tree.
        // Tree-based head_strip_paths captures it.
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");

        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![], // Drop has empty strip_paths by design
            )],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("squashed"),
            dry: false,

            squash: Some("squashed plan"),
            // Daemon provides head_strip_paths — the squash uses
            // these, NOT the union of per-step strip_paths.
            squash_tip: None,
            head_strip_paths: &[".clank/plans/foo.md".to_string()],
        })
        .await
        .unwrap();
        assert!(
            !tree_has(dir.path(), "squashed", ".clank/plans/foo.md"),
            "Drop-only squash must strip the plan file via head_strip_paths"
        );
    }

    #[tokio::test]
    async fn rewrite_squash_refuses_foreign_commit_in_range() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                true, // foreign
                vec![],
            )],
        );
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("squashed"),
            dry: false,

            squash: Some("collapse"),
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("foreign"), "got: {msg}");
    }

    #[tokio::test]
    async fn rewrite_runs_in_place_on_the_default_branch() {
        // `main`/`master` is where every clank plan is worked on;
        // rewriting it in place is the tool's job, not a hazard it
        // asks permission for (stash-does-what-you-mean).
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: None, // in-place
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .expect("the default branch rewrites in place");
        assert_ne!(git_head(dir.path()), intro, "the branch moved");
        assert_eq!(
            rev_list(dir.path(), "main").len(),
            1,
            "main itself was rewritten: the seed alone remains"
        );
    }

    #[tokio::test]
    async fn rewrite_into_branch_writes_a_fresh_branch() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let _seed = commit(dir.path(), "seed");
        write(dir.path(), ".clank/plans/foo.md", "# foo\n");
        let intro = commit(dir.path(), "plan: foo");
        let preview = mk_preview(
            &intro,
            &intro,
            vec![(
                intro.clone(),
                "plan: foo".into(),
                RewriteDisposition::Drop,
                false,
                vec![],
            )],
        );
        super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("scrubbed"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rewrite_refuses_non_linear_range() {
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let seed = commit(dir.path(), "seed");
        let preview = RewritePreviewResponse {
            plan_id: "clank/foo.md".into(),
            plan_stem: PlanKey::parse("foo").unwrap(),
            intro_sha: Some(CommitSha::parse(&seed).unwrap()),
            head_sha: CommitSha::parse(&seed).unwrap(),
            linear: false,
            commits: Vec::new(),
            squash_tip: None,
            head_strip_paths: Vec::new(),
        };
        let err = super::run(RewriteOpts {
            repo: dir.path(),
            intro_sha: preview.intro_sha.as_ref(),
            head_sha: &preview.head_sha,
            linear: preview.linear,
            commits: &preview.commits,
            into_branch: Some("x"),
            dry: false,

            squash: None,
            squash_tip: None,
            head_strip_paths: &[],
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("merge commit"), "{err}");
    }

    #[test]
    fn build_stripped_tree_works_in_a_linked_worktree() {
        // The bug: a LINKED worktree's `.git` is a FILE, so the old
        // hardcoded `<repo>/.git/clank-rewrite` ENOTDIR'd
        // (rewrite-scratch-dir-worktree). Reproduce with a REAL linked
        // worktree and assert both tree builders succeed there.
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "keep.txt", "keep\n");
        write(repo, "strip.txt", "strip\n");
        commit(repo, "base");

        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join("wt");
        run(
            repo,
            &[
                "worktree",
                "add",
                "--quiet",
                wt.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        assert!(
            wt.join(".git").is_file(),
            "linked worktree `.git` is a file"
        );

        write(&wt, "wt.txt", "in worktree\n");
        let sha = commit(&wt, "wt commit");

        // Pre-fix this ENOTDIR'd at create_dir_all(<wt>/.git/clank-rewrite).
        // The gix tree builder edits the tree in memory — no scratch
        // index — so nothing can ENOTDIR on the worktree's `.git` file.
        let tree = git_plumbing::strip_tree(&wt, &sha, &["strip.txt".to_string()]).unwrap();
        assert!(tree_has(&wt, &tree, "keep.txt"));
        assert!(!tree_has(&wt, &tree, "strip.txt"), "stripped path removed");
    }

    #[test]
    fn working_tree_dirty_excludes_clank_scratch() {
        // Clank's own untracked `.clank/` scratch must not block a
        // rewrite, but real uncommitted work still must.
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "src.txt", "x\n");
        commit(repo, "base");
        assert!(!working_tree_dirty(repo).unwrap(), "clean tree");

        // Untracked clank scratch → NOT dirty.
        write(repo, ".clank/cache/state.json", "{}\n");
        write(repo, ".clank/config.json", "{}\n");
        assert!(
            !working_tree_dirty(repo).unwrap(),
            "untracked .clank/ scratch must not block"
        );

        // An untracked file OUTSIDE .clank/ → dirty.
        write(repo, "stray.txt", "oops\n");
        assert!(working_tree_dirty(repo).unwrap(), "stray untracked counts");
        std::fs::remove_file(repo.join("stray.txt")).unwrap();
        assert!(!working_tree_dirty(repo).unwrap());

        // A modified TRACKED file → dirty.
        write(repo, "src.txt", "changed\n");
        assert!(working_tree_dirty(repo).unwrap(), "tracked edit must block");
    }

    #[tokio::test]
    async fn reword_in_place_rewords_buried_commit_and_replays_descendants() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "README.md", "seed\n");
        let _seed = commit(repo, "seed");
        write(repo, "a.txt", "a\n");
        let target = commit(repo, "original finish message");
        write(repo, "b.txt", "b\n");
        let _b = commit(repo, "plan B commit");
        write(repo, "c.txt", "c\n");
        let head = commit(repo, "plan C commit");

        let pairs = super::reword_in_place(RewordOpts {
            repo,
            target_sha: &CommitSha::parse(&target).unwrap(),
            head_sha: &CommitSha::parse(&head).unwrap(),
            new_message: "reworded whole-plan summary\n\nwhy it exists".as_bytes(),

            distinct_target: false,
            dry: false,
        })
        .await
        .unwrap();

        // target + 2 descendants = 3 (old, new) pairs.
        assert_eq!(pairs.len(), 3);
        // History stays linear and the same length (seed + 3 rewritten).
        let chain = rev_list(repo, "main");
        assert_eq!(chain.len(), 4, "seed + reworded target + B + C: {chain:?}");
        let (new_target, new_b, new_c) = (&chain[1], &chain[2], &chain[3]);
        // Reworded commit carries the new subject; descendants preserved.
        assert_eq!(
            show_subject(repo, new_target),
            "reworded whole-plan summary"
        );
        assert_eq!(show_subject(repo, new_b), "plan B commit");
        assert_eq!(show_subject(repo, new_c), "plan C commit");
        // The rewrite actually changed shas.
        assert_ne!(new_target, &target);
        assert_ne!(new_c, &head);
        // Trees are preserved: every file survives to the new tip.
        assert!(tree_has(repo, new_c, "a.txt"));
        assert!(tree_has(repo, new_c, "b.txt"));
        assert!(tree_has(repo, new_c, "c.txt"));
        // Pairs map old→new for the target and the final tip.
        assert_eq!(pairs[0].0.as_str(), target);
        assert_eq!(pairs[0].1.as_str(), *new_target);
        assert_eq!(pairs.last().unwrap().1.as_str(), *new_c);
    }

    /// The property `rereview` needs: rewriting with an UNCHANGED
    /// message mints a new commit every time, including back to back.
    ///
    /// Without `distinct_target` the second rewrite reproduces the
    /// first's sha exactly — `squash_commit` pins the committer date to
    /// the author date, so once a commit has been through it there is
    /// nothing left to differ. That makes `rereview` work once per plan
    /// and then silently do nothing, which is worse than never working.
    ///
    /// Deliberately NO sleeping: needing one would mean the guarantee
    /// still rests on the wall clock.
    #[tokio::test]
    async fn a_distinct_target_mints_a_new_sha_on_every_same_message_rewrite() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "a.txt", "a\n");
        let c = commit(repo, "[foo] intro");
        let mut sha = CommitSha::parse(&c).unwrap();

        let mut seen = vec![sha.clone()];
        for round in 0..3 {
            let pairs = reword_in_place(RewordOpts {
                repo,
                target_sha: &sha,
                head_sha: &sha,
                new_message: "[foo] intro".as_bytes(),

                distinct_target: true,
                dry: false,
            })
            .await
            .unwrap();
            let new = pairs[0].1.clone();
            assert_ne!(new, sha, "round {round} reproduced its own target");
            assert!(!seen.contains(&new), "round {round} reused an earlier sha");
            seen.push(new.clone());
            sha = new;
        }

        // The tree and message are untouched throughout — only the
        // review target moved.
        let msg = crate::git_io::commit_subject_at(repo, &sha).unwrap();
        assert!(msg.starts_with("[foo] intro"), "message preserved: {msg}");
    }

    /// The control: WITHOUT the opt-in, a second same-message rewrite
    /// reproduces the first's sha. This is the behaviour `rereview`
    /// cannot use, pinned so it is not mistaken for a bug later.
    #[tokio::test]
    async fn a_pinned_target_reproduces_its_own_sha_on_a_second_rewrite() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "a.txt", "a\n");
        let c = commit(repo, "[foo] intro");
        let sha = CommitSha::parse(&c).unwrap();

        let once = reword_in_place(RewordOpts {
            repo,
            target_sha: &sha,
            head_sha: &sha,
            new_message: "[foo] intro".as_bytes(),

            distinct_target: false,
            dry: false,
        })
        .await
        .unwrap()[0]
            .1
            .clone();

        let twice = reword_in_place(RewordOpts {
            repo,
            target_sha: &once,
            head_sha: &once,
            new_message: "[foo] intro".as_bytes(),

            distinct_target: false,
            dry: false,
        })
        .await
        .unwrap()[0]
            .1
            .clone();

        assert_eq!(
            once, twice,
            "the pinned committer date makes this a no-op — which is why \
             rereview must opt out of it"
        );
    }

    #[tokio::test]
    async fn reword_in_place_refuses_dirty_tree() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "README.md", "seed\n");
        let _seed = commit(repo, "seed");
        write(repo, "a.txt", "a\n");
        let target = commit(repo, "target");
        write(repo, "b.txt", "b\n");
        let head = commit(repo, "head");
        write(repo, "a.txt", "uncommitted edit\n"); // dirty a tracked file

        let err = super::reword_in_place(RewordOpts {
            repo,
            target_sha: &CommitSha::parse(&target).unwrap(),
            head_sha: &CommitSha::parse(&head).unwrap(),
            new_message: "x\n\nwhy".as_bytes(),

            distinct_target: false,
            dry: false,
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("working tree dirty"), "{err}");
    }

    #[tokio::test]
    async fn reword_in_place_refuses_target_off_first_parent_chain() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "README.md", "seed\n");
        let seed = commit(repo, "seed");
        write(repo, "a.txt", "a\n");
        let head = commit(repo, "head");
        // A commit on a side branch — NOT on main's first-parent chain.
        run(repo, &["checkout", "-q", "-b", "side", &seed]);
        write(repo, "s.txt", "s\n");
        let off = commit(repo, "side commit");
        run(repo, &["checkout", "-q", "main"]);

        let err = super::reword_in_place(RewordOpts {
            repo,
            target_sha: &CommitSha::parse(&off).unwrap(),
            head_sha: &CommitSha::parse(&head).unwrap(),
            new_message: "x\n\nwhy".as_bytes(),

            distinct_target: false,
            dry: false,
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("first-parent history"), "{err}");
    }

    #[tokio::test]
    async fn squash_with_empty_range_reports_blocker_instead_of_panicking() {
        // codex eeec2de: squash + an empty rewrite range (intro None, no
        // commits) used to underflow in the foreign-offender slice math.
        // Both dry and live must surface the empty-range blocker instead.
        let dir = init_repo();
        write(dir.path(), "README.md", "seed\n");
        let seed = commit(dir.path(), "seed");
        let head = CommitSha::parse(&seed).unwrap();
        let opts = |dry| RewriteOpts {
            repo: dir.path(),
            intro_sha: None,
            head_sha: &head,
            linear: true,
            commits: &[],
            into_branch: None,
            dry,

            squash: Some("collapse nothing"),
            squash_tip: None,
            head_strip_paths: &[],
        };
        // Dry: prints the blocker, returns cleanly.
        let outcome = super::run(opts(true)).await.unwrap();
        assert!(outcome.new_tip.is_none());
        // Live: bails with the empty-range blocker.
        let err = super::run(opts(false)).await.unwrap_err();
        assert!(
            format!("{err}").contains("rewrite range is empty"),
            "got: {err}"
        );
    }

    #[test]
    fn write_finalize_commit_moves_plan_file_and_stays_dangling() {
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "src.rs", "fn main() {}\n");
        write(repo, ".clank/plans/foo.md", "# foo plan body\n");
        let head = commit(repo, "[foo] intro");
        let refs_before = ref_count(repo);

        let synth = git_plumbing::write_finalize_commit(repo, &head, "foo", "[foo] finish preview")
            .unwrap();

        // Tree: plan file MOVED to finished/, same blob; code untouched.
        assert!(!tree_has(repo, &synth, ".clank/plans/foo.md"));
        assert!(tree_has(repo, &synth, ".clank/finished/foo.md"));
        assert!(tree_has(repo, &synth, "src.rs"));
        let blob = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", &format!("{synth}:.clank/finished/foo.md")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(blob.stdout).unwrap(), "# foo plan body\n");
        // Parent is head; the commit is DANGLING (no refs created, HEAD still).
        let parent = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", &format!("{synth}^")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(parent.stdout).unwrap().trim(), head);
        assert_eq!(ref_count(repo), refs_before, "no refs may be created");
        let chain = rev_list(repo, "main");
        assert_eq!(chain.last().unwrap(), &head, "branch untouched");
    }

    fn ref_count(repo: &Path) -> usize {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["for-each-ref"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().lines().count()
    }

    #[tokio::test]
    async fn reword_in_place_refuses_merge_in_range_live_and_reports_in_dry() {
        // A merge among the descendants means first-parent replay would drop a
        // parent. The LIVE run refuses; the DRY run — the same computation —
        // reports the blocker and moves nothing.
        let dir = init_repo();
        let repo = dir.path();
        write(repo, "README.md", "seed\n");
        let seed = commit(repo, "seed");
        write(repo, "a.txt", "a\n");
        let target = commit(repo, "target");
        // A side branch merged back in → merge commit above the target.
        run(repo, &["checkout", "-q", "-b", "side", &seed]);
        write(repo, "s.txt", "s\n");
        commit(repo, "side work");
        run(repo, &["checkout", "-q", "main"]);
        run(
            repo,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        );
        let head = git_head(repo);

        let target_sha = CommitSha::parse(&target).unwrap();
        let head_sha = CommitSha::parse(&head).unwrap();
        let opts = |dry| RewordOpts {
            repo,
            target_sha: &target_sha,
            head_sha: &head_sha,
            new_message: "x\n\nwhy".as_bytes(),

            distinct_target: false,
            dry,
        };
        let err = super::reword_in_place(opts(false)).await.unwrap_err();
        assert!(format!("{err}").contains("merge commit"), "{err}");

        let pairs = super::reword_in_place(opts(true)).await.unwrap();
        assert!(pairs.is_empty(), "dry returns no pairs");
        assert_eq!(git_head(repo), head, "dry must not move the branch");
    }

    fn git_head(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}
