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
        PrReviewCmd::Propose(a) => {
            let caller = crate::agent_env::resolve_identity_from_env(&repo)?;
            let round = propose_with(&repo, home.as_deref(), &caller, a.pr)?;
            println!("opened review round {round} — reviewers summoned");
            Ok(())
        }
        PrReviewCmd::Abort(a) => {
            let caller = crate::agent_env::resolve_identity_from_env(&repo)?;
            abort_with(&repo, home.as_deref(), &caller, a.pr)?;
            Ok(())
        }
        PrReviewCmd::Submit(a) => {
            let caller = crate::agent_env::resolve_identity_from_env(&repo)?;
            submit_with(&repo, home.as_deref(), &caller, a.pr)
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

/// Open (or re-open) the review for the current draft by bumping
/// the round — the explicit master handoff that summons reviewers.
/// MASTER-ONLY. The first call (round 0 → 1) opens the initial
/// review; later calls (after integrating a round's feedback)
/// re-open at a fresh round so the prior round's approvals go stale
/// and reviewers re-review. Master posts the GitHub draft comments
/// first; clank can't (and shouldn't) verify the GitHub side, so
/// this is master asserting "the draft is ready". Returns the new
/// round.
pub fn propose_with(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
) -> anyhow::Result<u64> {
    require_master(repo, home, caller, "propose")?;
    let pr = resolve_pr(repo, pr)?;
    let mut state = load_state(repo, pr)?;
    state.round += 1;
    save_state(repo, pr, &state)?;
    Ok(state.round)
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

/// Discard a PR review: the GitHub pending review (if any) AND the
/// local scratch. MASTER-ONLY: a reviewer's abort would delete the
/// team's shared review state.
///
/// The GitHub discard is best-effort — if `gh` is unavailable or
/// there's no pending review, the local scratch is still removed
/// (warn, don't fail): abort is master's explicit "throw it away",
/// and a dangling draft is recoverable by hand.
pub fn abort_with(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
) -> anyhow::Result<()> {
    require_master(repo, home, caller, "abort")?;
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;
    if let Err(e) = gh::discard_pending_review(&state.repo, pr) {
        eprintln!("warning: discarding the GitHub pending review failed: {e:#}");
    }
    std::fs::remove_dir_all(pr_dir(repo, pr))
        .with_context(|| format!("removing PR #{pr} review scratch"))?;
    Ok(())
}

/// The text posted as the published review's summary: the content
/// under the `## Submit body` heading of `master.md`, to the next
/// `## ` heading or EOF. Errors if missing or still empty/the
/// placeholder — clank won't publish a blank or template summary.
fn extract_submit_body(master_md: &str) -> anyhow::Result<String> {
    let mut lines = master_md.lines();
    let found = lines.by_ref().any(|l| l.trim() == "## Submit body");
    if !found {
        anyhow::bail!("master.md has no `## Submit body` section to publish");
    }
    let body: String = lines
        .take_while(|l| !l.trim_start().starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim();
    if body.is_empty() || body.starts_with('<') {
        anyhow::bail!(
            "master.md `## Submit body` is empty or still the placeholder; \
             write the review summary before submitting"
        );
    }
    Ok(body.to_string())
}

/// Publish the converged review to the PR. MASTER-ONLY. Gate must
/// be FINISHED; then freeze the round, re-sweep reviewer replies
/// (so only master's top-level comments publish), and submit with
/// master.md's summary body. On success the local scratch is
/// removed (the published review is the durable record).
pub fn submit_with(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
) -> anyhow::Result<()> {
    require_master(repo, home, caller, "submit")?;
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;

    // Convergence gate — fail closed on team-resolution failure,
    // and compute exactly as the wait surface does (current-round
    // verdicts through compute_gate with latest_touched_plan=false).
    let (commit_reviewers, gate_reviewers) =
        crate::agent_store::load_reviewer_tiers_with(repo, home)?;
    let verdicts = load_verdicts(repo, pr)?;
    let current: Vec<clank_core::wait::ReviewEntry> = verdicts
        .iter()
        .filter(|(_, v)| v.reviewed_round == state.round)
        .map(|(author, v)| clank_core::wait::ReviewEntry {
            author: author.clone(),
            verdict: v.verdict,
        })
        .collect();
    let gate = clank_core::wait::compute_gate(&current, &commit_reviewers, &gate_reviewers, false);
    if gate != clank_core::vocab::CommitGateState::Finished {
        anyhow::bail!(
            "not converged (gate: {}); run `clank pr-review status` — all reviewers must be FINISHED for round {}",
            gate.as_str(),
            state.round
        );
    }

    let body = extract_submit_body(&std::fs::read_to_string(
        pr_dir(repo, pr).join("master.md"),
    )?)?;

    // RE-SWEEP reviewer replies IMMEDIATELY before publishing: this
    // is the guard against a reply landing after the convergence
    // check (ruthless e9d1bbb #2). It is best-effort, not atomic —
    // GitHub offers no transaction across sweep + submit, and a
    // reviewer's raw `gh` can't be intercepted — so a reply in the
    // sub-second sweep→submit gap could still publish. Keeping the
    // two calls adjacent with nothing between them is the honest
    // minimum; there is no stronger guarantee available. (An earlier
    // `submitting` flag claimed a freeze it couldn't enforce against
    // raw gh, so it was removed rather than left as a false model.)
    let swept = gh::sweep_replies(&state.repo, pr)?;
    gh::submit_review(&state.repo, pr, &body)?;
    if swept > 0 {
        eprintln!(
            "swept {swept} reviewer repl{} before publishing",
            if swept == 1 { "y" } else { "ies" }
        );
    }

    // Published — the review on GitHub is the durable record; drop
    // the local scratch so the wait surface stops surfacing it.
    std::fs::remove_dir_all(pr_dir(repo, pr))
        .with_context(|| format!("removing PR #{pr} review scratch after publish"))?;
    println!("published review for PR #{pr}");
    Ok(())
}

/// The pending-review LIFECYCLE on GitHub: resolve / submit /
/// discard. clank owns only these — comment substance is agents'
/// raw `gh` per the skill. The id is resolved per-call from the
/// singleton (never stored). Pure argv-builders + a response parser
/// are unit-tested; only `run_gh` touches the network.
pub mod gh {
    use anyhow::Context;

    /// The team's single pending review on a PR.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PendingReview {
        /// REST numeric id (submit/discard).
        pub id: u64,
        /// GraphQL node id (reply creation).
        pub node_id: String,
    }

    pub(super) fn resolve_argv(slug: &str, pr: u32) -> Vec<String> {
        // `--paginate` alone emits one JSON document PER PAGE
        // (concatenated), which a single-array parse can't read on a
        // multi-page PR (codex dcd25f3). `--slurp` merges the pages
        // into one array-of-pages, which `parse_pending_review`
        // flattens.
        vec![
            "api".into(),
            format!("repos/{slug}/pulls/{pr}/reviews"),
            "--paginate".into(),
            "--slurp".into(),
        ]
    }

    pub(super) fn submit_argv(slug: &str, pr: u32, review_id: u64, body: &str) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "POST".into(),
            format!("repos/{slug}/pulls/{pr}/reviews/{review_id}/events"),
            "-f".into(),
            "event=COMMENT".into(),
            "-f".into(),
            format!("body={body}"),
        ]
    }

    pub(super) fn discard_argv(slug: &str, pr: u32, review_id: u64) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "DELETE".into(),
            format!("repos/{slug}/pulls/{pr}/reviews/{review_id}"),
        ]
    }

    pub(super) fn review_comments_argv(slug: &str, pr: u32, review_id: u64) -> Vec<String> {
        vec![
            "api".into(),
            format!("repos/{slug}/pulls/{pr}/reviews/{review_id}/comments"),
            "--paginate".into(),
            "--slurp".into(),
        ]
    }

    pub(super) fn delete_comment_argv(slug: &str, comment_id: u64) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "DELETE".into(),
            format!("repos/{slug}/pulls/comments/{comment_id}"),
        ]
    }

    /// Numeric ids of the THREADED REPLIES (reviewer comments) in a
    /// slurped `…/reviews/{id}/comments` response: those with a
    /// non-null `in_reply_to_id`. Top-level (master) comments have
    /// `in_reply_to_id` absent/null and are kept. STRUCTURAL — never
    /// keys on body text (the marker is human-attribution only).
    pub(super) fn parse_reply_ids(json: &str) -> anyhow::Result<Vec<u64>> {
        let pages: serde_json::Value =
            serde_json::from_str(json).context("parsing slurped review comments")?;
        let Some(pages) = pages.as_array() else {
            anyhow::bail!("slurped review comments is not an array of pages");
        };
        let mut ids = Vec::new();
        for page in pages {
            let Some(comments) = page.as_array() else {
                anyhow::bail!("a comments page is not an array");
            };
            for c in comments {
                if c.get("in_reply_to_id").is_some_and(|v| !v.is_null()) {
                    let id = c
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .context("reply comment missing numeric id")?;
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    /// The PENDING review from a `gh api --paginate --slurp`
    /// response: an array of PAGES, each page an array of review
    /// objects. Flattened across pages; the singleton guarantees at
    /// most one PENDING, so the first wins.
    pub(super) fn parse_pending_review(json: &str) -> anyhow::Result<Option<PendingReview>> {
        let pages: serde_json::Value =
            serde_json::from_str(json).context("parsing slurped reviews response")?;
        let Some(pages) = pages.as_array() else {
            anyhow::bail!("slurped reviews response is not an array of pages");
        };
        for page in pages {
            let Some(reviews) = page.as_array() else {
                anyhow::bail!("a reviews page is not an array");
            };
            for r in reviews {
                if r.get("state").and_then(|s| s.as_str()) == Some("PENDING") {
                    let id = r
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .context("pending review missing numeric id")?;
                    let node_id = r
                        .get("node_id")
                        .and_then(|v| v.as_str())
                        .context("pending review missing node_id")?
                        .to_string();
                    return Ok(Some(PendingReview { id, node_id }));
                }
            }
        }
        Ok(None)
    }

    fn run_gh(args: &[String]) -> anyhow::Result<String> {
        let out = std::process::Command::new("gh")
            .args(args)
            .output()
            .context("spawning gh")?;
        if !out.status.success() {
            anyhow::bail!(
                "gh {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Resolve the PR's pending review, or `None` if there isn't one.
    pub fn resolve_pending_review(slug: &str, pr: u32) -> anyhow::Result<Option<PendingReview>> {
        parse_pending_review(&run_gh(&resolve_argv(slug, pr))?)
    }

    /// Submit the pending review with `body` as the summary.
    /// Errors if there's no pending review to submit.
    pub fn submit_review(slug: &str, pr: u32, body: &str) -> anyhow::Result<()> {
        let review = resolve_pending_review(slug, pr)?
            .context("no pending review to submit (has master drafted any comments?)")?;
        run_gh(&submit_argv(slug, pr, review.id, body)).map(|_| ())
    }

    /// Discard the pending review if one exists; no-op otherwise.
    pub fn discard_pending_review(slug: &str, pr: u32) -> anyhow::Result<()> {
        if let Some(review) = resolve_pending_review(slug, pr)? {
            run_gh(&discard_argv(slug, pr, review.id)).map(|_| ())?;
        }
        Ok(())
    }

    /// Delete every threaded reply (reviewer comment) from the
    /// pending review, leaving only master's top-level comments.
    /// Returns the count deleted. Run IMMEDIATELY before submit so a
    /// reply landing during convergence can't publish onto the PR
    /// (the submit TOCTOU guard).
    pub fn sweep_replies(slug: &str, pr: u32) -> anyhow::Result<usize> {
        let Some(review) = resolve_pending_review(slug, pr)? else {
            return Ok(0);
        };
        let ids = parse_reply_ids(&run_gh(&review_comments_argv(slug, pr, review.id))?)?;
        for id in &ids {
            run_gh(&delete_comment_argv(slug, *id))?;
        }
        Ok(ids.len())
    }
}

/// Human-readable status: round + per-reviewer verdict + who we're
/// waiting on.
pub fn status_with(repo: &Path, home: Option<&Path>, pr: Option<u32>) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;
    // Fail CLOSED on team-resolution failure: an empty reviewer set
    // would make `pending_reviewers` report "converged" and falsely
    // mark the review done (codex da7ab89). The reviewer set is what
    // convergence is measured against, so its absence is an error,
    // not a degrade.
    let (commit_reviewers, gate_reviewers) =
        crate::agent_store::load_reviewer_tiers_with(repo, home)?;
    let reviewers: Vec<AgentLabel> = commit_reviewers.into_iter().chain(gate_reviewers).collect();
    let verdicts = load_verdicts(repo, pr)?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "pr #{} ({})  round {}",
        state.number, state.repo, state.round
    );
    let _ = writeln!(
        out,
        "  {}",
        crate::cli::status::pr_url(&state.repo, state.number)
    );
    // Round 0: the review isn't open yet — master drafts then
    // `propose`. No reviewers are summoned, so don't report them as
    // pending (that would contradict the wait surface).
    if state.round == 0 {
        let _ = writeln!(
            out,
            "  drafting — run `clank pr-review propose` to open for review"
        );
        return Ok(out);
    }
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

/// Wait-surface inputs for every active PR review: the
/// current-round verdicts (`reviewed_round == round`) mapped to
/// `ReviewEntry`, which `derive_status` feeds to `compute_gate`.
/// Best-effort per PR — a PR whose state/verdicts can't be read is
/// skipped rather than failing the whole projection (the wait
/// surface must never panic the loop).
pub fn pr_review_inputs(repo: &Path) -> Vec<clank_core::wait::PrReviewInput> {
    active_prs(repo)
        .into_iter()
        .filter_map(|pr| {
            let state = load_state(repo, pr).ok()?;
            let verdicts = load_verdicts(repo, pr).ok()?;
            let current_verdicts = verdicts
                .into_iter()
                .filter(|(_, v)| v.reviewed_round == state.round)
                .map(|(author, v)| clank_core::wait::ReviewEntry {
                    author,
                    verdict: v.verdict,
                })
                .collect();
            Some(clank_core::wait::PrReviewInput {
                pr,
                repo: state.repo,
                round: state.round,
                current_verdicts,
            })
        })
        .collect()
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

    #[test]
    fn gh_argv_shapes() {
        assert_eq!(
            gh::resolve_argv("o/r", 7),
            vec!["api", "repos/o/r/pulls/7/reviews", "--paginate", "--slurp"]
        );
        assert_eq!(
            gh::submit_argv("o/r", 7, 42, "summary body"),
            vec![
                "api",
                "--method",
                "POST",
                "repos/o/r/pulls/7/reviews/42/events",
                "-f",
                "event=COMMENT",
                "-f",
                "body=summary body"
            ]
        );
        assert_eq!(
            gh::discard_argv("o/r", 7, 42),
            vec!["api", "--method", "DELETE", "repos/o/r/pulls/7/reviews/42"]
        );
    }

    #[test]
    fn parse_pending_review_finds_the_singleton() {
        // Slurped shape: array of PAGES. A mix of submitted + pending
        // within one page: only the PENDING one returns.
        let json = r#"[[
            {"id": 1, "node_id": "PRR_a", "state": "COMMENTED"},
            {"id": 2, "node_id": "PRR_b", "state": "PENDING"}
        ]]"#;
        let pr = gh::parse_pending_review(json).unwrap().unwrap();
        assert_eq!(pr.id, 2);
        assert_eq!(pr.node_id, "PRR_b");
    }

    #[test]
    fn parse_pending_review_spans_pages() {
        // codex dcd25f3: the PENDING review can be on a LATER page.
        // --slurp gives an array of pages; the parser flattens.
        let json = r#"[
            [{"id": 1, "node_id": "PRR_a", "state": "COMMENTED"}],
            [{"id": 2, "node_id": "PRR_b", "state": "PENDING"}]
        ]"#;
        let pr = gh::parse_pending_review(json).unwrap().unwrap();
        assert_eq!(pr.id, 2);
    }

    #[test]
    fn parse_pending_review_none_when_no_pending() {
        let json = r#"[[{"id": 1, "node_id": "PRR_a", "state": "APPROVED"}]]"#;
        assert_eq!(gh::parse_pending_review(json).unwrap(), None);
        // No pages, or an empty page.
        assert_eq!(gh::parse_pending_review("[]").unwrap(), None);
        assert_eq!(gh::parse_pending_review("[[]]").unwrap(), None);
    }

    #[test]
    fn parse_pending_review_rejects_malformed() {
        assert!(gh::parse_pending_review("{not an array}").is_err());
        // A PENDING entry missing its ids is an error, not a silent skip.
        assert!(gh::parse_pending_review(r#"[[{"state":"PENDING"}]]"#).is_err());
    }

    #[test]
    fn parse_reply_ids_is_structural_not_marker_keyed() {
        // Replies (in_reply_to_id set) are swept; top-level (master)
        // comments are kept — regardless of body text / 🤖 marker.
        let json = r#"[[
            {"id": 1, "in_reply_to_id": null, "body": "master top-level"},
            {"id": 2, "in_reply_to_id": 1, "body": "🤖codex🤖 reply"},
            {"id": 3, "in_reply_to_id": 1, "body": "reply WITHOUT a marker"},
            {"id": 4, "body": "master top-level, no in_reply_to_id key"}
        ]]"#;
        let mut ids = gh::parse_reply_ids(json).unwrap();
        ids.sort_unstable();
        // 2 and 3 are replies (incl. the unmarked one); 1 and 4 are top-level.
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn gh_sweep_argv_shapes() {
        assert_eq!(
            gh::review_comments_argv("o/r", 7, 42),
            vec![
                "api",
                "repos/o/r/pulls/7/reviews/42/comments",
                "--paginate",
                "--slurp"
            ]
        );
        assert_eq!(
            gh::delete_comment_argv("o/r", 99),
            vec!["api", "--method", "DELETE", "repos/o/r/pulls/comments/99"]
        );
    }

    #[test]
    fn extract_submit_body_matrix() {
        let ok = "# notes\n\n## Submit body\nShip it: the refactor is clean.\n";
        assert_eq!(
            extract_submit_body(ok).unwrap(),
            "Ship it: the refactor is clean."
        );
        // Stops at the next ## heading.
        let multi = "## Submit body\nthe summary\n\n## Other\nignored\n";
        assert_eq!(extract_submit_body(multi).unwrap(), "the summary");
        // Missing section, empty, and the unfilled placeholder all error.
        assert!(extract_submit_body("# notes\nno section").is_err());
        assert!(extract_submit_body("## Submit body\n\n").is_err());
        assert!(extract_submit_body("## Submit body\n<the text…>\n").is_err());
    }
}
