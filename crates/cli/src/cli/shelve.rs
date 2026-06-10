//! `clank shelve <plan>` / `clank unshelve <plan>` — set an
//! in-flight plan's commits aside and restore them later.
//! Plan: plan-lifecycle-verbs (absorbs the removed `clank demote`;
//! `clank purge --drop` is the full-delete).
//!
//! Shelve order is the data-safety invariant: the protective ref
//! (`refs/clank/shelved/<plan>`) is written BEFORE the rewrite
//! drops anything, so the plan's commits stay reachable and
//! GC-protected even if every later step fails. Unshelve
//! cherry-picks the recorded shas back; reviews RESET by design
//! (new shas, new context — the fold derives Unreviewed and
//! reviewers re-review; nothing migrates).

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cli::rewrite::{RewriteOpts, run as run_rewrite};
use crate::lifecycle::PlanKey;
use crate::rebuild::CachePolicy;
use clank_core::api::{RewriteCommit, RewriteDisposition};
use clank_core::ids::CommitSha;

use super::{ShelveArgs, ShelveCleanArgs, UnshelveArgs};

/// On-disk shelve record at `.clank/shelved/<plan>.json`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ShelveState {
    /// The plan's attributed commit shas, oldest first — the
    /// cherry-pick order for unshelve.
    pub shas: Vec<String>,
    /// The protective ref keeping those commits reachable.
    pub git_ref: String,
    /// Optional "shelved waiting on this plan" dependency; powers
    /// the `clank status` nudge once that plan finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

pub fn shelved_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/shelved")
}

fn state_path(repo: &Path, stem: &str) -> PathBuf {
    shelved_dir(repo).join(format!("{stem}.json"))
}

fn ref_name(stem: &str) -> String {
    format!("refs/clank/shelved/{stem}")
}

/// Read every shelve record (for `clank status` surfacing).
pub fn scan_shelved(repo: &Path) -> Vec<(String, ShelveState)> {
    let Ok(entries) = std::fs::read_dir(shelved_dir(repo)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
            continue;
        };
        if let Ok(body) = std::fs::read_to_string(entry.path()) {
            if let Ok(state) = serde_json::from_str::<ShelveState>(&body) {
                out.push((stem.to_string(), state));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub async fn run_shelve(args: ShelveArgs) -> anyhow::Result<()> {
    if !args.to_queue {
        if args.priority.is_some() {
            anyhow::bail!("--priority only applies with --to-queue");
        }
    } else if let Some(p) = args.priority {
        // Mirrors `clank queue add`: scan_queue only recognizes
        // three-digit priorities.
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

    if state_path(&repo, &stem).exists() {
        anyhow::bail!(
            "plan `{stem}` already has shelved state. \
             `clank unshelve {stem}` to restore it, or \
             `clank shelve clean {stem}` to discard it first."
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
    // be shelved; `plan-reorder` is the future enabler), non-plan
    // content dropped only under --force.
    safety_check(&preview.commits, args.force)?;

    let plan_rel = format!(".clank/plans/{stem}.md");
    if plan_file_dirty(&repo, &plan_rel, preview.head_sha.as_str())? {
        anyhow::bail!(
            "`{plan_rel}` in the working tree differs from HEAD. \
             Commit your plan-body changes or `git stash` before shelving."
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
        anyhow::bail!("plan `{stem}` has no commits to shelve");
    }

    if args.dry {
        println!("dry-run: would shelve `{stem}`");
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

    if !args.yes
        && !confirm(&format!(
            "Shelve `{stem}` ({} commits set aside)?",
            shas.len()
        ))?
    {
        anyhow::bail!("aborted");
    }

    // ── PROTECT FIRST: the ref lands before anything rewrites. ──
    git_update_ref(&repo, &ref_name(&stem), preview.head_sha.as_str())?;
    let shelve_state = ShelveState {
        shas: shas.clone(),
        git_ref: ref_name(&stem),
        waiting_for: args.waiting_for.clone(),
    };
    let sp = state_path(&repo, &stem);
    std::fs::create_dir_all(sp.parent().expect("state path has parent"))?;
    std::fs::write(&sp, serde_json::to_string_pretty(&shelve_state)?)
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
        allow_rewrite_protected: args.allow_rewrite_protected,
        squash: None,
        head_strip_paths: &preview.head_strip_paths,
    })
    .await;
    let outcome = match rewrite_result {
        Ok(o) => o,
        Err(e) => {
            // Roll back the protective ref + state ONLY when the
            // branch never moved (pre-rewrite refusal, e.g.
            // protected branch without the override): the branch
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
                    "shelve: rewrite failed AFTER the branch moved; keeping \
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
            "shelved `{stem}`: branch `{branch}` now at {} ({} commits set aside on {})",
            &tip[..tip.len().min(12)],
            shas.len(),
            ref_name(&stem),
        );
    } else {
        println!(
            "shelved `{stem}` ({} commits set aside on {})",
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
    println!("  restore with `clank unshelve {stem}`");
    Ok(())
}

pub async fn run_unshelve(args: UnshelveArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let stem = args.plan.clone();
    PlanKey::parse(&stem).map_err(|e| anyhow::anyhow!("invalid plan `{stem}`: {e}"))?;

    let sp = state_path(&repo, &stem);
    let body = std::fs::read_to_string(&sp)
        .map_err(|e| anyhow::anyhow!("no shelved state for `{stem}` ({}): {e}", sp.display()))?;
    let shelve_state: ShelveState =
        serde_json::from_str(&body).with_context(|| format!("parsing `{}`", sp.display()))?;

    // A re-promoted plan with the same stem would collide with the
    // restored plan file; fail closed.
    if repo.join(format!(".clank/plans/{stem}.md")).exists() {
        anyhow::bail!(
            "plan `{stem}` is already active — its shelved commits predate the \
             current plan. `clank shelve clean {stem}` to discard them."
        );
    }
    if worktree_dirty(&repo)? {
        anyhow::bail!("working tree dirty; commit or stash before unshelving");
    }

    // Cherry-pick oldest-first. On the first conflict git leaves
    // the cherry-pick in progress for the user; the protective ref
    // and shelve state are NOT touched (fail-closed) — finishing
    // by hand + `clank shelve clean` completes the restore.
    for sha in &shelve_state.shas {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["cherry-pick", "--allow-empty", sha])
            .status()
            .context("spawning git cherry-pick")?;
        if !status.success() {
            anyhow::bail!(
                "cherry-pick of {sha} stopped (conflict?). Resolve with git \
                 (`git cherry-pick --continue` / `--abort`); the shelved ref \
                 `{}` and state are untouched. After completing by hand, run \
                 `clank shelve clean {stem}`.",
                shelve_state.git_ref
            );
        }
    }

    // Restore fully landed: drop the protective ref + state.
    git_delete_ref(&repo, &shelve_state.git_ref)?;
    std::fs::remove_file(&sp).with_context(|| format!("removing `{}`", sp.display()))?;

    println!(
        "unshelved `{stem}`: {} commits replayed onto HEAD",
        shelve_state.shas.len()
    );
    println!("  reviews reset — the new commits are unreviewed; reviewers will wake");
    Ok(())
}

pub async fn run_clean(args: ShelveCleanArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let stem = args.plan.clone();
    let sp = state_path(&repo, &stem);
    if !sp.exists() {
        anyhow::bail!("no shelved state for `{stem}`");
    }
    if !args.yes
        && !confirm(&format!(
            "Permanently discard shelved commits for `{stem}`? This is the only \
             copy of that work."
        ))?
    {
        anyhow::bail!("aborted");
    }
    git_delete_ref(&repo, &ref_name(&stem))?;
    std::fs::remove_file(&sp).with_context(|| format!("removing `{}`", sp.display()))?;
    println!("discarded shelved state for `{stem}`");
    Ok(())
}

/// Current HEAD sha, or None if it can't be read (detached states
/// and read failures both land on the SAFE side of the rollback
/// decision: keep the protective ref).
fn git_head(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_update_ref(repo: &Path, name: &str, sha: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", name, sha])
        .status()
        .context("spawning git update-ref")?;
    if !status.success() {
        anyhow::bail!("git update-ref {name} {sha} failed");
    }
    Ok(())
}

fn git_delete_ref(repo: &Path, name: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", "-d", name])
        .status()
        .context("spawning git update-ref -d")?;
    if !status.success() {
        anyhow::bail!("git update-ref -d {name} failed");
    }
    Ok(())
}

fn worktree_dirty(repo: &Path) -> anyhow::Result<bool> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .context("spawning git status")?;
    Ok(!out.stdout.is_empty())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Tiered safety check (inherited verbatim from demote):
/// - All non-foreign `Drop`: ok.
/// - Any `Rewrite` non-foreign: refuse unless `--force`.
/// - Any foreign commit (regardless of disposition): unconditional
///   refusal — interleaved plans cannot be shelved (codex 625b8af:
///   a foreign commit can classify as `Rewrite`, so ANY foreign is
///   refused, not just `KeepVerbatim`).
fn safety_check(commits: &[RewriteCommit], force: bool) -> anyhow::Result<()> {
    let mut rewrite_shas: Vec<&CommitSha> = Vec::new();
    let mut foreign_shas: Vec<&CommitSha> = Vec::new();
    for c in commits {
        if c.foreign {
            foreign_shas.push(&c.sha);
            continue;
        }
        if c.disposition == RewriteDisposition::Rewrite {
            rewrite_shas.push(&c.sha);
        }
    }
    if !foreign_shas.is_empty() {
        let list: Vec<String> = foreign_shas
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        anyhow::bail!(
            "shelve refuses: foreign commit(s) interleaved in plan range: {}. \
             `--force` does NOT bypass this. Disentangle first (see the \
             `plan-reorder` queue item) or coordinate with the author(s).",
            list.join(", ")
        );
    }
    if !rewrite_shas.is_empty() && !force {
        let list: Vec<String> = rewrite_shas
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        anyhow::bail!(
            "shelve refuses: commit(s) {} touch non-plan content that would be \
             set aside with the plan. Pass `--force` to shelve them anyway.",
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
    let head_content = match crate::git_io::show_blob(repo, &head_sha_parsed, Path::new(rel_path)) {
        Ok(s) => Some(s),
        Err(_) => None,
    };
    Ok(wt_content != head_content)
}
