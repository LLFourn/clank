//! `clank pr-review` — drive the multi-agent review loop against a
//! GitHub PR. This module owns the LOCAL state under
//! `.clank/pr-reviews/<pr>/` (the authoritative verdict + round
//! layer); GitHub pending-review I/O lands in a later phase.
//!
//! Cores take an explicit `home`/author so tests drive them
//! in-process (no binary spawning); `run` resolves identity + role
//! from the environment and team config.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::ids::AgentLabel;
use clank_core::pr_review::{PrReviewState, ReviewerVerdict, pending_reviewers};
use clank_core::vocab::{Role, Verdict};

use super::{PrReviewArgs, PrReviewCmd, resolve_repo};

fn pr_reviews_root(repo: &Path) -> PathBuf {
    repo.join(".clank").join("pr-reviews")
}

fn pr_dir(repo: &Path, pr: u32) -> PathBuf {
    pr_reviews_root(repo).join(pr.to_string())
}

fn state_path(repo: &Path, pr: u32) -> PathBuf {
    pr_dir(repo, pr).join("pr.json")
}

fn reviewer_path(repo: &Path, pr: u32, label: &AgentLabel) -> PathBuf {
    pr_dir(repo, pr)
        .join("reviews")
        .join(format!("{}.md", label.as_str()))
}

const MASTER_TEMPLATE: &str = "\
# PR review — master notes

## Summary
<one-paragraph overview of the review>

## General concerns
<cross-cutting points not tied to a single line>

## Submit body
<the text posted as the review summary when you submit>
";

fn load_state(repo: &Path, pr: u32) -> anyhow::Result<PrReviewState> {
    let path = state_path(repo, pr);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading `{}` (is PR #{pr} started?)", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing `{}`", path.display()))
}

fn save_state(repo: &Path, pr: u32, state: &PrReviewState) -> anyhow::Result<()> {
    let path = state_path(repo, pr);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(state)?;
    write_atomic(&path, body.as_bytes())
}

fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-pr-review-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    use std::io::Write as _;
    tmp.write_all(contents)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Active PR-review numbers (dirs under `.clank/pr-reviews/` that
/// have a `pr.json`), ascending.
fn active_prs(repo: &Path) -> Vec<u32> {
    let mut prs: Vec<u32> = std::fs::read_dir(pr_reviews_root(repo))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n: u32 = e.file_name().to_str()?.parse().ok()?;
            state_path(repo, n).is_file().then_some(n)
        })
        .collect();
    prs.sort_unstable();
    prs
}

/// Resolve which PR a verb targets: the explicit `--pr`, else the
/// single active one. Zero or many (without `--pr`) is an error —
/// mirrors the single-visible-plan inference.
fn resolve_pr(repo: &Path, explicit: Option<u32>) -> anyhow::Result<u32> {
    if let Some(pr) = explicit {
        return Ok(pr);
    }
    match active_prs(repo).as_slice() {
        [only] => Ok(*only),
        [] => anyhow::bail!("no active PR review — run `clank pr-review start <pr>` first"),
        many => anyhow::bail!(
            "{} active PR reviews ({}); pass `--pr <n>`",
            many.len(),
            many.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Parse `owner/name` from the `origin` remote URL (ssh or https).
fn repo_slug(repo: &Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("spawning git remote get-url origin")?;
    if !out.status.success() {
        anyhow::bail!("no `origin` remote; cannot determine owner/name for the gh API");
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_slug(&url).ok_or_else(|| anyhow::anyhow!("cannot parse owner/name from origin `{url}`"))
}

/// `git@github.com:owner/name.git` / `https://github.com/owner/name(.git)`
/// → `owner/name`. Pure for testing.
fn parse_slug(url: &str) -> Option<String> {
    let tail = url
        .rsplit_once("github.com")
        .map(|(_, t)| t.trim_start_matches([':', '/']))?;
    let tail = tail
        .strip_suffix(".git")
        .unwrap_or(tail)
        .trim_end_matches('/');
    let mut parts = tail.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

pub async fn run(args: PrReviewArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match args.command {
        PrReviewCmd::Start(a) => {
            let slug = repo_slug(&repo)?;
            let dest = start_with(&repo, &slug, a.pr, None)?;
            println!("{}", dest.display());
            Ok(())
        }
        PrReviewCmd::Note(a) => {
            let author = match a.author.as_deref() {
                Some(raw) => AgentLabel::parse(raw)
                    .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
                None => crate::agent_env::resolve_identity_from_env(&repo)?,
            };
            let path = note_with(&repo, &author, a.pr, a.verdict.into(), &a.message)?;
            println!("{}", path.display());
            Ok(())
        }
        PrReviewCmd::Abort(a) => {
            let caller = crate::agent_env::resolve_identity_from_env(&repo)?;
            abort_with(&repo, home.as_deref(), &caller, a.pr)?;
            Ok(())
        }
        PrReviewCmd::Status(a) => {
            print!("{}", status_with(&repo, home.as_deref(), a.pr)?);
            Ok(())
        }
    }
}

/// Scaffold `.clank/pr-reviews/<pr>/` for a fresh review: pin the
/// PR head, write `pr.json` (round 0, no pending review yet),
/// the master template, and the `reviews/` dir; ensure the
/// gitignore entry. The GitHub pending review is created later.
pub fn start_with(
    repo: &Path,
    slug: &str,
    pr: u32,
    source: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let dir = pr_dir(repo, pr);
    if dir.exists() {
        anyhow::bail!("PR #{pr} review already started at `{}`", dir.display());
    }
    let head_sha = super::fork::fetch_pr_head(source.unwrap_or(repo), pr)?;
    std::fs::create_dir_all(dir.join("reviews"))?;
    save_state(repo, pr, &PrReviewState::new(slug, pr, head_sha))?;
    write_atomic(&dir.join("master.md"), MASTER_TEMPLATE.as_bytes())?;
    crate::init_facts::ensure_clank_gitignore_entry(repo, "/pr-reviews/")?;
    Ok(dir)
}

/// Record a reviewer's verdict for the current round.
pub fn note_with(
    repo: &Path,
    author: &AgentLabel,
    pr: Option<u32>,
    verdict: Verdict,
    summary: &str,
) -> anyhow::Result<PathBuf> {
    if verdict == Verdict::Unmarked {
        anyhow::bail!("verdict `unmarked` cannot be written");
    }
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;
    let entry = ReviewerVerdict::new(verdict, state.round, summary.trim());
    let path = reviewer_path(repo, pr, author);
    write_atomic(&path, entry.render().as_bytes())?;
    Ok(path)
}

/// Discard a PR review's local scratch. MASTER-ONLY: a reviewer's
/// abort would delete the team's shared review state.
pub fn abort_with(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
) -> anyhow::Result<()> {
    require_master(repo, home, caller, "abort")?;
    let pr = resolve_pr(repo, pr)?;
    std::fs::remove_dir_all(pr_dir(repo, pr))
        .with_context(|| format!("removing PR #{pr} review scratch"))?;
    Ok(())
}

/// Human-readable status: round + per-reviewer verdict + who we're
/// waiting on.
pub fn status_with(repo: &Path, home: Option<&Path>, pr: Option<u32>) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;
    let (commit_reviewers, gate_reviewers) =
        crate::agent_store::load_reviewer_tiers_with(repo, home).unwrap_or_default();
    let reviewers: Vec<AgentLabel> = commit_reviewers.into_iter().chain(gate_reviewers).collect();
    let verdicts = load_verdicts(repo, pr)?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "pr #{} ({})  round {}",
        state.number, state.repo, state.round
    );
    for label in &reviewers {
        let cell = match verdicts.iter().find(|(l, _)| l == label) {
            Some((_, v)) if v.is_current(state.round) => v.verdict.as_str().to_string(),
            Some((_, v)) => format!("{} (stale, round {})", v.verdict.as_str(), v.reviewed_round),
            None => "—".to_string(),
        };
        let _ = writeln!(out, "  {}: {cell}", label.as_str());
    }
    let pending = pending_reviewers(state.round, &reviewers, &verdicts);
    if pending.is_empty() {
        let _ = writeln!(out, "converged: all reviewers FINISHED");
    } else {
        let _ = writeln!(
            out,
            "waiting on: {}",
            pending
                .iter()
                .map(|l| l.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(out)
}

/// Parse every `reviews/<label>.md` for a PR. A corrupt file is an
/// error (a misread verdict could publish prematurely).
fn load_verdicts(repo: &Path, pr: u32) -> anyhow::Result<Vec<(AgentLabel, ReviewerVerdict)>> {
    let dir = pr_dir(repo, pr).join("reviews");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(label) = AgentLabel::parse(stem) else {
            continue;
        };
        let raw = std::fs::read_to_string(&path)?;
        let verdict = ReviewerVerdict::parse(&raw)
            .map_err(|e| anyhow::anyhow!("corrupt `{}`: {e}", path.display()))?;
        out.push((label, verdict));
    }
    out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(out)
}

/// Bail unless `caller` is the team's master.
fn require_master(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    verb: &str,
) -> anyhow::Result<()> {
    let set = crate::agent_store::try_resolve_via_team_with(repo, home)?.ok_or_else(|| {
        anyhow::anyhow!("no team configured; cannot authorize `pr-review {verb}`")
    })?;
    if crate::agent_store::role_from_registered_set(&set, caller) == Role::Master {
        Ok(())
    } else {
        anyhow::bail!(
            "`pr-review {verb}` is master-only; `{}` is not the team master (`{}`)",
            caller.as_str(),
            set.master.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_handles_ssh_and_https() {
        assert_eq!(
            parse_slug("git@github.com:LLFourn/clank.git").as_deref(),
            Some("LLFourn/clank")
        );
        assert_eq!(
            parse_slug("https://github.com/LLFourn/clank.git").as_deref(),
            Some("LLFourn/clank")
        );
        assert_eq!(
            parse_slug("https://github.com/LLFourn/clank").as_deref(),
            Some("LLFourn/clank")
        );
        assert_eq!(parse_slug("/local/bare.git"), None);
    }
}
