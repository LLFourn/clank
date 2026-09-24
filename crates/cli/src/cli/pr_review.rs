//! `clank pr-review` — drive the multi-agent review loop against a
//! GitHub PR. This module owns the LOCAL state under
//! `.clank/pr-reviews/<pr>/` (the authoritative verdict + round
//! layer); GitHub pending-review I/O lands in a later phase.
//!
//! Cores take an explicit `home`/author so tests drive them
//! in-process (no binary spawning); `run` resolves identity + role
//! from the environment and team config.

use std::path::{Path, PathBuf};

use crate::cli::github::draft::DraftHorizon;
use anyhow::Context;
use clank_core::agent_config::{GithubEventKind, GithubSource, WaitEventSource};
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

/// Where the draft observer remembers what it has already seen.
/// Inside the review's own directory, so it is created, carried and
/// destroyed with the review and cannot outlive it.
/// Keyed by the review RUN, not just the PR.
///
/// Two runs of one PR sharing a path is what made every earlier fix a
/// check-then-act race: validate ownership, and the review can still
/// be replaced before the write lands. Separate files make that
/// impossible rather than unlikely — a dead run can only write its
/// own file, which `finish`/`abort` remove with the directory (codex
/// 96dbec8).
fn draft_horizon_path(repo: &Path, pr: u32, instance: u64) -> PathBuf {
    pr_dir(repo, pr).join(format!("draft-{instance}.json"))
}

/// A horizon that cannot be read is a horizon that has seen nothing:
/// the next snapshot re-baselines silently. Presenting an unreadable
/// file as "everything changed" would spam master for a corrupt
/// scratch file.
fn load_draft_horizon(repo: &Path, pr: u32, instance: u64) -> DraftHorizon {
    std::fs::read_to_string(draft_horizon_path(repo, pr, instance))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Never creates the review's directory. An in-flight read can land
/// after `finish` or `abort` removed it, and recreating it with a
/// `draft.json` and no `pr.json` leaves a husk that makes
/// `start_with` refuse to restart that PR (codex 29431d9).
fn save_draft_horizon(
    repo: &Path,
    pr: u32,
    instance: u64,
    horizon: &DraftHorizon,
) -> anyhow::Result<()> {
    let dir = pr_dir(repo, pr);
    write_into_existing(
        &draft_horizon_path(repo, pr, instance),
        &dir,
        serde_json::to_string_pretty(horizon)?.as_bytes(),
    )
    .with_context(|| format!("PR #{pr}'s review is gone; its draft horizon was not written"))
}

/// A wait handed these revisions to master, so the watch stops owing
/// them. Called on the wait's return path — the only point at which
/// an item has actually reached master — and only after the sources
/// are shut down, so nothing is writing the file concurrently.
pub fn draft_changes_delivered(repo: &Path, pr: u32, instance: u64, revisions: &[u64]) {
    let mut horizon = load_draft_horizon(repo, pr, instance);
    horizon.delivered(revisions);
    if let Err(e) = save_draft_horizon(repo, pr, instance, &horizon) {
        // The review ended in the meantime; the debt died with it.
        let _ = e;
    }
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

## Review summary
<the overview that goes on the draft review itself — the human reads
it above your comments and submits, or does not>
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
    write_into_existing(path, parent, contents)
}

/// Writes atomically WITHOUT creating the parent. A caller whose
/// directory may be removed underneath it needs the write to fail
/// rather than rebuild it: checking `is_dir()` first cannot close
/// that race, because the removal can land between the check and the
/// write (codex 66f7789).
fn write_into_existing(path: &Path, parent: &Path, contents: &[u8]) -> anyhow::Result<()> {
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
pub(crate) fn repo_slug(repo: &Path) -> anyhow::Result<String> {
    let url = crate::git_io::origin_url(repo).ok_or_else(|| {
        anyhow::anyhow!("no `origin` remote; cannot determine owner/name for the gh API")
    })?;
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
    let repo_arg = args.repo.clone();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match args.command {
        PrReviewCmd::Start(a) if a.fork => {
            // Same operation as `clank fork create --pr <N> --review`: the
            // worktree creation + review scaffold + zellij open all
            // live in `fork::run`; this verb is just a second door.
            super::fork::run(crate::cli::ForkArgs {
                command: crate::cli::ForkCmd::Create(crate::cli::ForkCreateArgs {
                    name: None,
                    // This door carries the team's sessions across,
                    // like `fork create` without `--fresh`.
                    fresh: false,
                    source: repo_arg,
                    pr: Some(a.pr),
                    branch: None,
                    path: None,
                    clone: false,
                    team: None,
                    drafts: Vec::new(),
                    prompt: None,
                    no_open: false,
                    review: true,
                }),
            })
            .await
        }
        PrReviewCmd::Start(a) if a.checkout => {
            let dest = checkout_with(&repo, a.pr)?;
            println!("{}", dest.display());
            Ok(())
        }
        PrReviewCmd::Start(a) => {
            let slug = repo_slug(&repo)?;
            let dest = start_with(&repo, &slug, a.pr, None, None)?;
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
        PrReviewCmd::Finish(a) => {
            let caller = crate::agent_env::resolve_identity_from_env(&repo)?;
            finish_with(&repo, home.as_deref(), &caller, a.pr)
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
///
/// `pinned_head`: when the caller already fetched + pinned the PR
/// head (the composed `--fork`/`--checkout` paths fetch it to base
/// the worktree / switch branches), it passes that sha here so the
/// review anchors to the SAME commit — one fetch, and worktree-base
/// == review-pin by construction, not coincidence (ruthless b88ae34).
/// Standalone `pr-review start` passes `None` and fetches.
pub fn start_with(
    repo: &Path,
    slug: &str,
    pr: u32,
    source: Option<&Path>,
    pinned_head: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let dir = pr_dir(repo, pr);
    if dir.exists() {
        anyhow::bail!("PR #{pr} review already started at `{}`", dir.display());
    }
    let head_sha = match pinned_head {
        Some(s) => s.to_string(),
        None => super::fork::fetch_pr_head(source.unwrap_or(repo), pr)?,
    };
    std::fs::create_dir_all(dir.join("reviews"))?;
    let mut state = PrReviewState::new(slug, pr, head_sha);
    state.instance = PrReviewState::new_instance();
    save_state(repo, pr, &state)?;
    write_atomic(&dir.join("master.md"), MASTER_TEMPLATE.as_bytes())?;
    // The draft watch's baseline is anchored HERE, at the review's
    // beginning, not at the first park. Round 0 always hands master a
    // PrMaster item, so the first wait that actually parks comes
    // AFTER master has drafted — and a baseline taken then would
    // silently swallow an operator edit made in between (codex
    // 29431d9). A review starts from no draft, so that is what this
    // records; a pending review that somehow predates it then
    // presents its comments, which is the informative answer.
    save_draft_horizon(repo, pr, state.instance, &DraftHorizon::started())?;
    state.watch_owner = add_review_watch(repo, slug, pr)?.map(|l| l.as_str().to_string());
    save_state(repo, pr, &state)?;
    // `.clank/pr-reviews/` is covered by the `.clank/.gitignore` allow-list
    // (`/*`) — no per-dir entry to add.
    Ok(dir)
}

/// Check out the PR head in the CURRENT worktree (branch `pr-<n>`),
/// then scaffold the review in place. The in-place alternative to
/// `--fork`.
///
/// Fail-closed like `fork`: ALL read-only preconditions are checked
/// before the `git checkout` mutation, so a doomed run never leaves
/// the worktree on a switched branch (codex b944c58). Refuses on a
/// dirty worktree, an already-started review, an unparseable origin
/// slug, or an existing `pr-<n>` branch — all before any mutation.
/// Returns the scaffolded review dir.
pub fn checkout_with(repo: &Path, pr: u32) -> anyhow::Result<PathBuf> {
    if crate::git_io::working_tree_dirty_at(repo)?.is_some() {
        anyhow::bail!("worktree is dirty — commit or stash first; `--checkout` switches branches");
    }
    if pr_dir(repo, pr).exists() {
        anyhow::bail!(
            "PR #{pr} review already started; nothing to check out (run `clank pr-review status`)"
        );
    }
    let slug = repo_slug(repo)?;
    let branch = format!("pr-{pr}");
    let exists = crate::git_io::resolve_commit(repo, &format!("refs/heads/{branch}")).is_some();
    if exists {
        anyhow::bail!("branch `{branch}` already exists; check it out yourself or delete it first");
    }
    // Preconditions all passed — now the network side-effect, then
    // the lone local mutation (the branch switch). The fetched sha is
    // threaded into start_with so the review pins the SAME head the
    // branch was checked out to — one fetch (ruthless b88ae34).
    let sha = super::fork::fetch_pr_head(repo, pr)?;
    crate::git_plumbing::checkout_new_branch(repo, &branch, &sha)?;
    start_with(repo, &slug, pr, None, Some(&sha))
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

/// The watch master gets while it has this review open.
///
/// Derived from the review rather than configured, so nothing has to
/// be written into `config.json` and nothing is left behind when the
/// review ends. Only what the events feed reports as it happens: a
/// new comment, a push, the merge. Edits are deliberately absent —
/// the feed carries no row for one, and catching them means
/// abandoning the feed entirely (`the-published-half-of-a-reviewed-pr`,
/// stashed).
///
/// `include_own_actions` is ON. Every agent shares the human's `gh`
/// identity, so the default would hide exactly the comments this
/// exists to surface; master seeing its own published comments is the
/// price, and the prompt says so.
pub fn review_source(slug: &str, pr: u32) -> GithubSource {
    GithubSource {
        repo: slug.to_string(),
        events: vec![
            GithubEventKind::PrComment,
            GithubEventKind::PrUpdated,
            GithubEventKind::PrMerged,
        ],
        prs: vec![u64::from(pr)],
        include_own_actions: true,
        poll_interval: None,
        branches: Vec::new(),
        prompt: Some(
            "From the PR review you have open. AWARENESS: read it before your next round, \
             then `clank events ack` it. Do not reply to it and do not act on it — the \
             human working on this PR is not asking you a question. You will see your own \
             published comments here too, because every agent shares one gh identity. \
             This watch starts listening when you first PARK, so anything that happened \
             on the PR while you were drafting is already history — read the PR itself \
             rather than trusting these to have caught it."
                .to_string(),
        ),
        delivery: Default::default(),
    }
}

/// Add the review's watch to master's config, or take it away.
///
/// The watch is an ORDINARY `wait_events` entry, written for you at
/// `start` and removed at `finish`/`abort`. Everything downstream
/// treats it as what it is — one you could have typed yourself — so
/// its WAL, its prompt, its acking and its cleanup are the behaviour
/// that already exists rather than a second set of rules.
///
/// Add the watch to master's config, and say WHOSE config it went in.
/// `None` when nothing was added.
///
/// "Already there" is judged by watch IDENTITY, not by the whole
/// entry: `source_key` covers repo/kinds/prs/own-actions and excludes
/// the prompt, the poll interval and the delivery mode, so a watch of
/// yours differing only in those is the SAME watch to everything
/// downstream. Appending beside it would give one WAL, one horizon and
/// one lease to two entries, and the later prompt would win for both
/// (codex ded5760).
fn add_review_watch(repo: &Path, slug: &str, pr: u32) -> anyhow::Result<Option<AgentLabel>> {
    let Some(master) = crate::agent_store::try_resolve_via_team(repo)?.map(|set| set.master) else {
        // No team, so no master to watch on behalf of.
        return Ok(None);
    };
    let ours = review_source(slug, pr);
    let key = crate::cli::github::supervisor::source_key(&ours);
    let mut added = false;
    crate::agent_store::update_agent_config(repo, &master, |cfg| {
        let already = cfg.wait_events.iter().any(|s| match s {
            WaitEventSource::Github(g) => crate::cli::github::supervisor::source_key(g) == key,
            _ => false,
        });
        if !already {
            cfg.wait_events.push(WaitEventSource::Github(ours.clone()));
            added = true;
        }
    })?;
    Ok(added.then_some(master))
}

/// Remove the entry `start` added, from the config it added it to.
///
/// Matched whole, not by identity: an entry edited since — a prompt
/// changed, an interval added — is one somebody has taken over, and
/// taking it away would be deleting their work.
fn remove_review_watch(
    repo: &Path,
    slug: &str,
    pr: u32,
    owner: Option<&str>,
) -> anyhow::Result<()> {
    let Some(owner) = owner else {
        return Ok(());
    };
    let owner = AgentLabel::parse(owner)
        .map_err(|e| anyhow::anyhow!("PR review recorded an unreadable watch owner: {e}"))?;
    let ours = WaitEventSource::Github(review_source(slug, pr));
    crate::agent_store::update_agent_config(repo, &owner, |cfg| {
        cfg.wait_events.retain(|s| s != &ours);
    })
}

/// How often the open review's draft is re-read while master is
/// parked. A draft is edited by hand, so a minute is responsive
/// enough and costs two requests.
pub const DRAFT_POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Watch the open review's draft for as long as master is parked.
///
/// Derived from the active review rather than configured, so it
/// touches no operator watch and cannot outlive the review that owns
/// it — the wait aborts this task on every return path, and `finish`
/// and `abort` take the horizon with the review's directory.
///
/// Awareness only. Every change presents, including master's own:
/// one shared `gh` account and no write path clank owns means there
/// is no provenance to filter by, and every stand-in for it erases
/// the operator's edits too.
pub async fn run_draft_source(
    repo: std::path::PathBuf,
    slug: String,
    pr: u32,
    author: AgentLabel,
    tx: tokio::sync::mpsc::UnboundedSender<clank_core::wait::WaitItem>,
    interval: std::time::Duration,
) {
    let reader = crate::cli::github::draft::production_reader(slug);
    let roster = repo.clone();
    let still_mine = move || draft_watch_belongs_to(&roster, &author);
    drive_draft_source(&repo, pr, &still_mine, &reader, tx, interval).await
}

/// `still_mine` answers "is this watch's agent still master". It is a
/// question rather than a roster, because the answer can change while
/// a read is in flight and the driver has to ask it again after.
async fn drive_draft_source<R: crate::cli::github::draft::DraftReader, F: Fn() -> bool>(
    repo: &Path,
    pr: u32,
    still_mine: &F,
    reader: &R,
    tx: tokio::sync::mpsc::UnboundedSender<clank_core::wait::WaitItem>,
    interval: std::time::Duration,
) {
    // The watch belongs to ONE RUN of the review, not to the wait and
    // not to the PR. A check that a review merely exists cannot say
    // that: finish-then-start during an in-flight read would pass it,
    // and this task would then write the dead run's observation over
    // the fresh run's baseline (codex 66f7789).
    let Some(mine) = owning_instance(repo, pr) else {
        return;
    };
    let mut horizon = load_draft_horizon(repo, pr, mine);

    // Anything an earlier wait produced and could not hand over goes
    // first. Within THIS process each change is sent once: the debt is
    // what survives a wait that died, not a reason to repeat itself
    // tick after tick.
    if !send_all(pr, mine, &tx, &horizon.owed()) {
        return;
    }

    loop {
        let observed = crate::cli::github::draft::observe(reader, pr, &mut horizon).await;
        // Checked AFTER the read, not before it: the read is where the
        // review can end underneath us.
        if owning_instance(repo, pr) != Some(mine) {
            return;
        }
        // The watch is MASTER's. A demotion mid-read leaves the review
        // instance perfectly valid, so the instance check cannot see
        // it — and two waits writing one run's horizon lose each
        // other's changes (codex edf3a63).
        if !still_mine() {
            return;
        }
        match observed {
            Ok(changes) => {
                // Written down BEFORE they are sent, so a wait that
                // dies between the two leaves the debt standing.
                horizon.owe(&changes);
                if let Err(e) = save_draft_horizon(repo, pr, mine, &horizon) {
                    eprintln!("wait: draft watch on #{pr}: {e}");
                    return;
                }
                if !send_all(pr, mine, &tx, &changes) {
                    return;
                }
            }
            // A watch that cannot read is not a wait that should end.
            Err(e) => eprintln!("wait: draft watch on #{pr}: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// Whether the draft watch is this agent's to run.
///
/// MASTER's alone, and asked again on every tick rather than at spawn:
/// a demotion leaves the review untouched, so nothing else notices it.
/// An unresolvable role is a no — a watch that cannot prove it owns
/// the review must not write to it.
pub fn draft_watch_belongs_to(repo: &Path, author: &AgentLabel) -> bool {
    crate::agent_store::resolve_role(repo, author)
        .is_ok_and(|r| r == clank_core::vocab::Role::Master)
}

/// The instance token of the review this watch belongs to, or `None`
/// when there is no review or it predates instance tokens — in which
/// case the watch owns nothing and does not run.
fn owning_instance(repo: &Path, pr: u32) -> Option<u64> {
    match load_state(repo, pr) {
        Ok(s) if s.instance != 0 => Some(s.instance),
        _ => None,
    }
}

/// `false` when the channel is gone, which ends the watch with the
/// debt intact.
fn send_all(
    pr: u32,
    instance: u64,
    tx: &tokio::sync::mpsc::UnboundedSender<clank_core::wait::WaitItem>,
    changes: &[crate::cli::github::draft::DraftTransition],
) -> bool {
    for t in changes.iter().cloned() {
        if tx
            .send(clank_core::wait::WaitItem::DraftChanged {
                pr,
                instance,
                comment: t.comment,
                path: t.path,
                change: t.change,
                revision: t.revision,
            })
            .is_err()
        {
            return false;
        }
    }
    true
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
    remove_review_watch(repo, &state.repo, pr, state.watch_owner.as_deref())?;
    std::fs::remove_dir_all(pr_dir(repo, pr))
        .with_context(|| format!("removing PR #{pr} review scratch"))?;
    Ok(())
}

/// The text posted as the published review's summary: the content
/// under the `## Review summary` heading of `master.md`, to the next
/// `## ` heading or EOF. Errors if missing or still empty/the
/// placeholder — clank won't publish a blank or template summary.
fn extract_summary(master_md: &str) -> anyhow::Result<String> {
    let mut lines = master_md.lines();
    let found = lines.by_ref().any(|l| l.trim() == "## Review summary");
    if !found {
        anyhow::bail!("master.md has no `## Review summary` section to hand over");
    }
    let body: String = lines
        .take_while(|l| !l.trim_start().starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim();
    if body.is_empty() || body.starts_with('<') {
        anyhow::bail!(
            "master.md `## Review summary` is empty or still the placeholder; \
             write the review summary before submitting"
        );
    }
    Ok(body.to_string())
}

/// The remote half of the handoff, injected so its ORDER — every
/// remote step, and only then the local scratch — is testable
/// without a network (codex 6b8aaed). Production is [`RealGh`].
pub trait Handoff {
    fn pending(&self, slug: &str, pr: u32) -> anyhow::Result<Option<gh::PendingReview>>;
    /// `head_sha` is the review's PINNED commit — what the team
    /// actually read — not whatever the PR points at now.
    fn open_with_summary(
        &self,
        slug: &str,
        pr: u32,
        head_sha: &str,
        body: &str,
    ) -> anyhow::Result<()>;
    fn set_summary(&self, slug: &str, pr: u32, id: u64, body: &str) -> anyhow::Result<()>;
    fn replies_left(&self, slug: &str, pr: u32, id: u64) -> anyhow::Result<usize>;
    /// Remove the loop's 👍 scaffolding; returns how many.
    fn sweep_thumbs(&self, slug: &str, pr: u32, id: u64) -> anyhow::Result<usize>;
}

pub struct RealGh;

impl Handoff for RealGh {
    fn pending(&self, slug: &str, pr: u32) -> anyhow::Result<Option<gh::PendingReview>> {
        gh::resolve_pending_review(slug, pr)
    }
    fn open_with_summary(
        &self,
        slug: &str,
        pr: u32,
        head_sha: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        gh::create_summary_review(slug, pr, head_sha, body)
    }
    fn set_summary(&self, slug: &str, pr: u32, id: u64, body: &str) -> anyhow::Result<()> {
        gh::set_review_body(slug, pr, id, body)
    }
    fn replies_left(&self, slug: &str, pr: u32, id: u64) -> anyhow::Result<usize> {
        gh::count_replies(slug, pr, id)
    }
    fn sweep_thumbs(&self, slug: &str, pr: u32, id: u64) -> anyhow::Result<usize> {
        gh::sweep_thumbs_up(slug, pr, id)
    }
}

/// Hand the converged review over. MASTER-ONLY, and it submits
/// NOTHING.
///
/// The review stays a pending review — visible to its author, nobody
/// else — so the operator reads it in GitHub's own interface, edits
/// what they disagree with, and submits it or does not. Whether a
/// review goes out as APPROVE or REQUEST_CHANGES, and in what words,
/// is not a thing a team of agents decides on someone's behalf, and a
/// published review cannot be taken back.
///
/// A RETRYABLE handoff: the gate, then the remote work, and only then
/// the local scratch. Anything that fails leaves the scratch where it
/// was, so running it again resumes rather than stranding the review
/// half-handed-over.
pub fn finish_with(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
) -> anyhow::Result<()> {
    finish_through(repo, home, caller, pr, &RealGh)
}

/// GitHub's refusal to give a body to a review that has none.
///
/// Probed on a live PR: REST `PUT .../reviews/<id>` and GraphQL
/// `updatePullRequestReview` both answer with this sentence, so the
/// text is the contract, not the status code — a 422 alone covers
/// other refusals that ARE worth retrying.
///
/// Read off the SERVER's answer only. The formatted error chain also
/// carries the argv, and the argv carries the review summary an agent
/// wrote; a summary quoting this sentence would otherwise make every
/// transient failure look permanent.
fn is_missing_body_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<gh::GhFailure>().is_some_and(|f| {
        f.said
            .contains("Could not edit a review with a missing body")
    })
}

/// Record that THIS review cannot be handed over, so the converged
/// gate stops waking master to retry what cannot work.
///
/// Stamped with the instance: a later run on the same PR is a
/// different review and inherits nothing.
fn block_handoff(repo: &Path, pr: u32, state: &PrReviewState) -> anyhow::Result<()> {
    let mut next = state.clone();
    next.handoff_blocked = Some(state.instance);
    save_state(repo, pr, &next)
}

pub fn finish_through(
    repo: &Path,
    home: Option<&Path>,
    caller: &AgentLabel,
    pr: Option<u32>,
    gh: &impl Handoff,
) -> anyhow::Result<()> {
    require_master(repo, home, caller, "finish")?;
    let pr = resolve_pr(repo, pr)?;
    let state = load_state(repo, pr)?;

    // Convergence gate — fail closed on team-resolution failure,
    // and compute exactly as the wait surface does (current-round
    // verdicts through compute_gate with latest_touched_plan=false).
    let tiers = crate::agent_store::load_reviewer_tiers_with(repo, home)?;
    let verdicts = load_verdicts(repo, pr)?;
    let current: Vec<clank_core::wait::ReviewEntry> = verdicts
        .iter()
        .filter(|(_, v)| v.reviewed_round == state.round)
        .map(|(author, v)| clank_core::wait::ReviewEntry {
            author: author.clone(),
            verdict: v.verdict,
            at: crate::age::written_at(&reviewer_path(repo, pr, author)),
        })
        .collect();
    let gate =
        clank_core::wait::compute_gate(&current, &tiers.commit, &tiers.plan, &tiers.final_, false);
    if gate != clank_core::vocab::CommitGateState::Finished {
        anyhow::bail!(
            "not converged (gate: {}); run `clank pr-review status` — all reviewers must be FINISHED for round {}",
            gate.as_str(),
            state.round
        );
    }

    let md = std::fs::read_to_string(pr_dir(repo, pr).join("master.md"))?;
    let summary = extract_summary(&md)?;

    // A review with no inline comments has no draft yet — so make
    // one, carrying the summary. "I read it and this is what I think"
    // is a review, and it should not need an invented line comment to
    // exist, nor lose its summary to a refusal (codex 6b8aaed).
    // Running `finish` IS the operator asking again, so a block from
    // a previous attempt is lifted before this one. Cleared here and
    // not on success, so a run that fails for some OTHER reason
    // leaves master wakeable — only the diagnosed refusal below
    // re-blocks, and only for this instance.
    if state.handoff_blocked.is_some() {
        let mut cleared = state.clone();
        cleared.handoff_blocked = None;
        save_state(repo, pr, &cleared)?;
    }

    let mut thumbs = 0;
    let replies = match gh.pending(&state.repo, pr)? {
        Some(review) => {
            // Clean first, then write the summary onto what is left:
            // the scaffolding comes off the draft before it is handed
            // over, not after.
            thumbs = gh.sweep_thumbs(&state.repo, pr, review.id)?;
            if let Err(e) = gh.set_summary(&state.repo, pr, review.id, &summary) {
                if is_missing_body_refusal(&e) {
                    block_handoff(repo, pr, &state)?;
                    anyhow::bail!(
                        "PR #{pr}: this review was created without a body, and GitHub will not \
                         add one — REST and GraphQL both refuse with \"Could not edit a review \
                         with a missing body\", so the agreed summary cannot be put on it.\n  \
                         Nothing was changed: the draft, its comments and the local review are \
                         all still there, and the summary is in \
                         .clank/pr-reviews/{pr}/master.md.\n  \
                         Handing this one over automatically is not possible, so master will \
                         not be woken to retry it. `clank pr-review finish` runs again if you \
                         want it to."
                    );
                }
                return Err(e);
            }
            gh.replies_left(&state.repo, pr, review.id)?
        }
        None => {
            gh.open_with_summary(&state.repo, pr, state.head_sha.as_str(), &summary)?;
            0
        }
    };

    // Remote work done — only NOW is the scratch expendable. Failing
    // before this point leaves it for the next attempt.
    remove_review_watch(repo, &state.repo, pr, state.watch_owner.as_deref())?;
    std::fs::remove_dir_all(pr_dir(repo, pr))
        .with_context(|| format!("removing PR #{pr} review scratch after handoff"))?;

    println!(
        "review for PR #{pr} is ready — it is a DRAFT and nothing was submitted\n  \
         read it: https://github.com/{}/pull/{pr}/files",
        state.repo
    );
    // Reviewer replies that survived integration are SURFACED, never
    // deleted. Publishing swept them so they could not post onto the
    // PR; nothing publishes now, and a reply still in the draft is
    // feedback master did not integrate — deleting it here would
    // erase the one thing this handoff exists to put in front of a
    // human.
    if thumbs > 0 {
        println!("  removed {thumbs} 👍 the review loop left on your comments",);
    }
    if replies > 0 {
        println!(
            "  {replies} reviewer repl{} still in it: read {} before you submit",
            if replies == 1 { "y" } else { "ies" },
            if replies == 1 { "it" } else { "them" },
        );
    }
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

    /// Set a PENDING review's body without submitting it. GitHub's
    /// update-a-review endpoint is independent of submission, which
    /// is what lets the summary sit on the draft instead of being
    /// held back for a publish that never comes.
    pub fn update_body_argv(slug: &str, pr: u32, review_id: u64, body: &str) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "PUT".into(),
            format!("repos/{slug}/pulls/{pr}/reviews/{review_id}"),
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

    pub(super) fn comment_reactions_argv(slug: &str, comment_id: u64) -> Vec<String> {
        vec![
            "api".into(),
            format!("repos/{slug}/pulls/comments/{comment_id}/reactions"),
            "--paginate".into(),
            "--slurp".into(),
        ]
    }

    pub(super) fn delete_reaction_argv(
        slug: &str,
        comment_id: u64,
        reaction_id: u64,
    ) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "DELETE".into(),
            format!("repos/{slug}/pulls/comments/{comment_id}/reactions/{reaction_id}"),
        ]
    }

    /// Every comment id in a slurped `…/reviews/{id}/comments`
    /// response — replies AND master's top-level comments, since a
    /// reviewer's 👍 lands on the latter.
    pub(super) fn parse_comment_ids(json: &str) -> anyhow::Result<Vec<u64>> {
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
                ids.push(
                    c.get("id")
                        .and_then(|v| v.as_u64())
                        .context("review comment missing numeric id")?,
                );
            }
        }
        Ok(ids)
    }

    /// Ids of the `+1` reactions in a slurped reactions response.
    ///
    /// ONLY `+1`: a reviewer's approval is scaffolding this loop put
    /// there, and anything else on the comment is not ours to remove.
    pub(super) fn parse_thumbs_up_ids(json: &str) -> anyhow::Result<Vec<u64>> {
        let pages: serde_json::Value =
            serde_json::from_str(json).context("parsing slurped reactions")?;
        let Some(pages) = pages.as_array() else {
            anyhow::bail!("slurped reactions is not an array of pages");
        };
        let mut ids = Vec::new();
        for page in pages {
            let Some(reactions) = page.as_array() else {
                anyhow::bail!("a reactions page is not an array");
            };
            for r in reactions {
                if r.get("content").and_then(|v| v.as_str()) == Some("+1") {
                    ids.push(
                        r.get("id")
                            .and_then(|v| v.as_u64())
                            .context("reaction missing numeric id")?,
                    );
                }
            }
        }
        Ok(ids)
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

    /// What a `gh` call FAILED with, keeping the server's answer
    /// apart from the request that provoked it.
    ///
    /// The two must not be one string. The argv carries `body=<the
    /// review summary>`, and a summary is free text an agent wrote —
    /// a review OF this very bug would quote GitHub's refusal in it.
    /// Classifying on the formatted whole then reads our own request
    /// back as the server's verdict and blocks a handoff that was
    /// only transiently broken (codex 904bc9b).
    #[derive(Debug)]
    pub struct GhFailure {
        /// `gh`'s stderr: the server's message and nothing of ours.
        pub said: String,
        /// The request, for the operator to read. NEVER classified on.
        pub argv: Vec<String>,
    }

    impl std::fmt::Display for GhFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "gh {:?} failed: {}", self.argv, self.said)
        }
    }

    impl std::error::Error for GhFailure {}

    /// Split one failed `gh` call into the server's answer and our
    /// request.
    ///
    /// Named and separate so the split can be TESTED: it is the whole
    /// invariant, and `run_gh` itself needs a real `gh` to run. Only
    /// `stderr` may reach `said` — folding the argv in is the bug
    /// this exists to prevent.
    pub(super) fn failure_from(args: &[String], stderr: &[u8]) -> GhFailure {
        GhFailure {
            said: String::from_utf8_lossy(stderr).trim().to_string(),
            argv: args.to_vec(),
        }
    }

    fn run_gh(args: &[String]) -> anyhow::Result<String> {
        let out = std::process::Command::new("gh")
            .args(args)
            .output()
            .context("spawning gh")?;
        if !out.status.success() {
            return Err(failure_from(args, &out.stderr).into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Resolve the PR's pending review, or `None` if there isn't one.
    pub fn resolve_pending_review(slug: &str, pr: u32) -> anyhow::Result<Option<PendingReview>> {
        parse_pending_review(&run_gh(&resolve_argv(slug, pr))?)
    }

    /// Discard the pending review if one exists; no-op otherwise.
    pub fn discard_pending_review(slug: &str, pr: u32) -> anyhow::Result<()> {
        if let Some(review) = resolve_pending_review(slug, pr)? {
            run_gh(&discard_argv(slug, pr, review.id)).map(|_| ())?;
        }
        Ok(())
    }

    /// Open a pending review carrying only `body` — no inline
    /// comments, no event. A review whose finding is "I read it and
    /// this is what I think" is a real review, and it should not need
    /// an invented line comment to exist (codex 6b8aaed).
    pub(super) fn create_summary_argv(
        slug: &str,
        pr: u32,
        head_sha: &str,
        body: &str,
    ) -> Vec<String> {
        vec![
            "api".into(),
            "--method".into(),
            "POST".into(),
            format!("repos/{slug}/pulls/{pr}/reviews"),
            // The PINNED sha, never GitHub's default. Omitting it
            // attaches the review to the PR's most recent commit, so
            // a push between `start` and `finish` would leave a
            // summary describing what the team read attached to code
            // it never saw — and retiring the scratch takes the pin
            // with it (codex d19fc6c).
            "-f".into(),
            format!("commit_id={head_sha}"),
            "-f".into(),
            format!("body={body}"),
        ]
    }

    /// Put `body` on the pending review. Submits nothing.
    pub fn set_review_body(slug: &str, pr: u32, review_id: u64, body: &str) -> anyhow::Result<()> {
        run_gh(&update_body_argv(slug, pr, review_id, body)).map(|_| ())
    }

    /// Open a summary-only pending review. Nothing is submitted: no
    /// `event` is sent, which is what makes the new review PENDING.
    pub fn create_summary_review(
        slug: &str,
        pr: u32,
        head_sha: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        run_gh(&create_summary_argv(slug, pr, head_sha, body)).map(|_| ())
    }

    /// Remove every 👍 the loop left on the draft's comments, and
    /// say how many. Idempotent: a draft with none loses nothing.
    ///
    /// Reviewers approve a comment by reacting rather than replying,
    /// so those reactions mean "this reviewer agreed with master's
    /// draft" — scaffolding, not a finding about the code. Handing
    /// the draft over covered in the team's own thumbs would be
    /// handing over the scaffolding with the building.
    pub fn sweep_thumbs_up(slug: &str, pr: u32, review_id: u64) -> anyhow::Result<usize> {
        sweep_thumbs_with(slug, pr, review_id, &|argv| run_gh(argv))
    }

    /// The sweep over an injected caller, so WHICH comments it asks
    /// about is testable — a reviewer's 👍 lands on master's
    /// top-level comments, which the reply parser filters out, and
    /// nothing else would notice that mistake.
    pub(super) fn sweep_thumbs_with(
        slug: &str,
        pr: u32,
        review_id: u64,
        call: &dyn Fn(&[String]) -> anyhow::Result<String>,
    ) -> anyhow::Result<usize> {
        let comments = parse_comment_ids(&call(&review_comments_argv(slug, pr, review_id))?)?;
        let mut removed = 0;
        for comment in comments {
            for reaction in parse_thumbs_up_ids(&call(&comment_reactions_argv(slug, comment))?)? {
                call(&delete_reaction_argv(slug, comment, reaction))?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// How many threaded replies are still in the draft.
    ///
    /// Counted, never deleted. While the review was published this
    /// swept them first, so a reply landing after convergence could
    /// not post onto the PR; nothing publishes now, and a surviving
    /// reply is feedback master did not integrate — the handoff
    /// exists to put that in front of a human, not to erase it.
    pub fn count_replies(slug: &str, pr: u32, review_id: u64) -> anyhow::Result<usize> {
        Ok(parse_reply_ids(&run_gh(&review_comments_argv(slug, pr, review_id))?)?.len())
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
    // PR reviews have no plan doc, so only commit + final-tier
    // reviewers (final + gate) ever participate; plan-only reviewers
    // are not summoned.
    let tiers = crate::agent_store::load_reviewer_tiers_with(repo, home)?;
    let reviewers: Vec<AgentLabel> = tiers.commit.into_iter().chain(tiers.final_).collect();
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
        // Convergence stays a separate fact: the team DID agree, and
        // what failed afterwards is the handoff.
        if state.handoff_blocked == Some(state.instance) {
            let _ = writeln!(
                out,
                "handoff BLOCKED: this review was created without a body, and GitHub will not \
                 add one, so the summary cannot be put on it.\n  \
                 Nothing was lost -- the draft and the summary are both still there.\n  \
                 Master is not being woken to retry; `clank pr-review finish` runs it again."
            );
        }
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
                    at: crate::age::written_at(&reviewer_path(repo, pr, &author)),
                    author,
                    verdict: v.verdict,
                })
                .collect();
            Some(clank_core::wait::PrReviewInput {
                pr,
                repo: state.repo,
                round: state.round,
                // Resolved against THIS instance: a block recorded by
                // an earlier run on the same PR is not this run's.
                handoff_blocked: state.handoff_blocked == Some(state.instance),
                // The round's state file is written by `start` and
                // `propose` only, both master-only: its write time is
                // when this round opened, which is when its reviewers
                // were summoned.
                opened_at: crate::age::written_at(&state_path(repo, pr)),
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
        anyhow::anyhow!(
            "no master agent configured; cannot authorize `pr-review {verb}`. \
             Build a roster with `clank agent add` / `clank agent promote`."
        )
    })?;
    if crate::agent_store::role_from_registered_set(&set, caller) == Some(Role::Master) {
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

    /// A repo with PR `pr`'s review already started, because a draft
    /// horizon only ever exists inside one.
    fn scratch(pr: u32) -> (PathBuf, u64) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clank-draft-horizon-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(pr_dir(&dir, pr).join("reviews")).unwrap();
        let mut state = PrReviewState::new("o/r", pr, "head");
        state.instance = PrReviewState::new_instance();
        save_state(&dir, pr, &state).unwrap();
        // As `start_with` does. Without it the fixture's horizon is
        // `Unknown`, which absorbs its first read silently — and a
        // test whose first read says nothing cannot tell a correct
        // watch from one writing over a fresh review.
        save_draft_horizon(&dir, pr, state.instance, &DraftHorizon::started()).unwrap();
        (dir, state.instance)
    }

    #[test]
    fn a_horizon_with_no_file_yet_has_seen_nothing() {
        let (repo, run) = scratch(7);
        std::fs::remove_file(draft_horizon_path(&repo, 7, run)).unwrap();
        assert_eq!(load_draft_horizon(&repo, 7, run), DraftHorizon::default());
    }

    #[test]
    fn an_unreadable_horizon_rebaselines_rather_than_crying_wolf() {
        let (repo, run) = scratch(7);
        let path = draft_horizon_path(&repo, 7, run);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            load_draft_horizon(&repo, 7, run),
            DraftHorizon::default(),
            "a corrupt scratch file must not present the whole draft as new"
        );
    }

    /// A draft scripted tick by tick; exhausted -> the last answer
    /// repeats, which is what an unchanging draft looks like.
    struct Scripted(
        std::sync::Mutex<std::collections::VecDeque<anyhow::Result<Vec<&'static str>>>>,
    );

    impl crate::cli::github::draft::DraftReader for Scripted {
        async fn pending_review(&self, _: u32) -> anyhow::Result<Option<u64>> {
            Ok(Some(9))
        }
        async fn read(
            &self,
            _: u32,
            _: u64,
        ) -> anyhow::Result<Vec<crate::cli::github::draft::DraftComment>> {
            let step = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(vec!["last"]));
            Ok(step?
                .into_iter()
                .map(|body| crate::cli::github::draft::DraftComment {
                    id: 1,
                    path: "f.rs".into(),
                    body: body.into(),
                    updated_at: "T".into(),
                })
                .collect())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_delivers_each_change_once_and_remembers_it_across_waits() {
        let (repo, run) = scratch(7);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let reader = Scripted(std::sync::Mutex::new(
            [
                Ok(vec!["drafted"]),
                Ok(vec!["the operator edited it"]),
                Err(anyhow::anyhow!("github is down")),
                Ok(vec!["and again"]),
            ]
            .into(),
        ));

        let task = tokio::spawn({
            let repo = repo.clone();
            async move {
                // The task is aborted below, exactly as the wait does
                // on every return path.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    drive_draft_source(
                        &repo,
                        7,
                        &|| true,
                        &reader,
                        tx,
                        std::time::Duration::from_secs(60),
                    ),
                )
                .await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_secs(200)).await;
        task.abort();

        let mut seen = Vec::new();
        while let Ok(item) = rx.try_recv() {
            match item {
                clank_core::wait::WaitItem::DraftChanged { revision, .. } => seen.push(revision),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            seen,
            [1, 2, 3],
            "the review was anchored empty, so master's own first comment IS a change; \
             the failed read reports nothing; and each change is delivered once"
        );

        // The horizon outlived the watch, so the next wait picks up
        // where this one left off. A horizon that was never saved
        // would ALSO be silent here — it would baseline afresh — so
        // the revision is what tells the two apart.
        let mut next = load_draft_horizon(&repo, 7, run);
        assert!(
            next.absorb_snapshot(9, &[(1, "and again")]).is_empty(),
            "what this wait already reported is not reported again"
        );
        assert_eq!(
            next.absorb_snapshot(9, &[(1, "something new")]),
            [4],
            "and the count continues, which a re-baselined horizon could not do"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_nobody_received_is_not_spent() {
        // The ordering — deliver, THEN persist — only shows itself
        // when delivery does not happen. Dropping the receiver is the
        // one way a test can make that true.
        let (repo, run) = scratch(7);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reader = Scripted(std::sync::Mutex::new(
            [Ok(vec!["drafted"]), Ok(vec!["edited"])].into(),
        ));

        // Baseline while someone is listening.
        let task = tokio::spawn({
            let repo = repo.clone();
            async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    drive_draft_source(
                        &repo,
                        7,
                        &|| true,
                        &reader,
                        tx,
                        std::time::Duration::from_secs(60),
                    ),
                )
                .await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        drop(rx);
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        task.abort();

        let next = load_draft_horizon(&repo, 7, run);
        assert_eq!(
            next.owed().iter().map(|t| t.revision).collect::<Vec<_>>(),
            [1, 2],
            "nothing here ever reached master — no wait returned — so both changes are \
             still owed, including the one produced after the receiver went away"
        );
    }

    /// A watch that must END on its own. A driver that keeps polling
    /// a review it no longer owns would otherwise spin forever under
    /// the paused clock and HANG the test rather than failing it.
    async fn drive_watch_bounded<R: crate::cli::github::draft::DraftReader>(
        repo: &Path,
        pr: u32,
        reader: &R,
        tx: tokio::sync::mpsc::UnboundedSender<clank_core::wait::WaitItem>,
    ) {
        tokio::time::timeout(
            std::time::Duration::from_secs(600),
            drive_draft_source(
                repo,
                pr,
                &|| true,
                reader,
                tx,
                std::time::Duration::from_secs(60),
            ),
        )
        .await
        .expect("the watch must stop once it no longer owns its review");
    }

    /// A reader that runs `during` while the read is in flight, so a
    /// test can end or replace the review at the one moment the
    /// driver cannot see.
    struct RacingRead<F: Fn() + Send + Sync>(F);

    impl<F: Fn() + Send + Sync> crate::cli::github::draft::DraftReader for RacingRead<F> {
        async fn pending_review(&self, _: u32) -> anyhow::Result<Option<u64>> {
            Ok(Some(9))
        }
        async fn read(
            &self,
            _: u32,
            _: u64,
        ) -> anyhow::Result<Vec<crate::cli::github::draft::DraftComment>> {
            (self.0)();
            Ok(vec![crate::cli::github::draft::DraftComment {
                id: 1,
                path: "f.rs".into(),
                body: "from the old run".into(),
                updated_at: "T".into(),
            }])
        }
    }

    /// Abort and start again on the same PR, as the operator can.
    fn restart(repo: &Path, pr: u32) -> u64 {
        let _ = std::fs::remove_dir_all(pr_dir(repo, pr));
        std::fs::create_dir_all(pr_dir(repo, pr).join("reviews")).unwrap();
        let mut fresh = PrReviewState::new("o/r", pr, "head");
        fresh.instance = PrReviewState::new_instance();
        save_state(repo, pr, &fresh).unwrap();
        save_draft_horizon(repo, pr, fresh.instance, &DraftHorizon::started()).unwrap();
        fresh.instance
    }

    #[test]
    fn an_old_waits_acknowledgement_cannot_spend_a_new_runs_change() {
        // Revisions restart at 1 for each run, so acknowledging by
        // `(pr, revision)` alone lets run A's late acknowledgement
        // delete run B's revision 1 — a change nobody has seen
        // (codex 96dbec8).
        let (repo, a) = scratch(7);
        let mut horizon_a = load_draft_horizon(&repo, 7, a);
        assert_eq!(horizon_a.absorb_snapshot(9, &[(1, "run A's comment")]), [1]);
        save_draft_horizon(&repo, 7, a, &horizon_a).unwrap();

        let b = restart(&repo, 7);
        let mut horizon_b = load_draft_horizon(&repo, 7, b);
        assert_eq!(
            horizon_b.absorb_snapshot(9, &[(1, "run B's comment")]),
            [1],
            "run B's first change is revision 1 as well"
        );
        save_draft_horizon(&repo, 7, b, &horizon_b).unwrap();

        // Run A's wait finally prints, and acknowledges ITS revision 1.
        draft_changes_delivered(&repo, 7, a, &[1]);

        assert_eq!(
            load_draft_horizon(&repo, 7, b)
                .owed()
                .iter()
                .map(|t| t.revision)
                .collect::<Vec<_>>(),
            [1],
            "run B still owes its own change, which nobody has seen"
        );
        assert!(
            load_draft_horizon(&repo, 7, a).owed().is_empty(),
            "and run A's own debt is what was settled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_demotion_mid_read_neither_wakes_nor_spends_the_change() {
        // A demotion leaves the review instance perfectly valid, so
        // the instance check cannot see it. Two waits writing one
        // run's horizon lose each other's changes, and an item
        // returned by the demoted agent would be ACKED — settling a
        // debt owed to whoever is master now (codex edf3a63).
        let (repo, run) = scratch(7);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let demoted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let racer = {
            let demoted = demoted.clone();
            move || demoted.store(true, std::sync::atomic::Ordering::SeqCst)
        };
        let still_mine = {
            let demoted = demoted.clone();
            move || !demoted.load(std::sync::atomic::Ordering::SeqCst)
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(600),
            drive_draft_source(
                &repo,
                7,
                &still_mine,
                &RacingRead(racer),
                tx,
                std::time::Duration::from_secs(60),
            ),
        )
        .await
        .expect("a watch that is no longer master's must stop");

        assert!(
            rx.try_recv().is_err(),
            "the demoted agent does not wake with a change it no longer owns"
        );
        assert!(
            load_draft_horizon(&repo, 7, run).owed().is_empty(),
            "and it wrote nothing, so the change is still there for the real master to find"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_review_ending_mid_read_neither_wakes_nor_resurrects() {
        let (repo, _run) = scratch(7);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let gone = {
            let repo = repo.clone();
            move || {
                let _ = std::fs::remove_dir_all(pr_dir(&repo, 7));
            }
        };
        drive_watch_bounded(&repo, 7, &RacingRead(gone), tx).await;

        assert!(
            rx.try_recv().is_err(),
            "a change read from a review that ended mid-read is not master's news"
        );
        assert!(
            !pr_dir(&repo, 7).exists(),
            "and nothing rebuilt the directory the cleanup removed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_restarted_review_is_not_written_over_by_the_old_ones_watch() {
        let (repo, run) = scratch(7);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fresh_run = std::sync::Arc::new(std::sync::Mutex::new(0u64));
        let racer = {
            let repo = repo.clone();
            let fresh_run = fresh_run.clone();
            move || {
                // finish/abort, then start again on the same PR —
                // both presence checks would pass for the NEW run.
                *fresh_run.lock().unwrap() = restart(&repo, 7);
            }
        };
        drive_watch_bounded(&repo, 7, &RacingRead(racer), tx).await;

        assert!(
            rx.try_recv().is_err(),
            "the dead run's read is not the new run's news"
        );
        let b = *fresh_run.lock().unwrap();
        let mut fresh = load_draft_horizon(&repo, 7, b);
        assert_eq!(
            fresh.absorb_snapshot(9, &[(1, "from the new run")]),
            [1],
            "the new run's anchored baseline survived — the old watch did not write over it"
        );
        assert!(
            !draft_horizon_path(&repo, 7, run).exists()
                || load_draft_horizon(&repo, 7, run) != load_draft_horizon(&repo, 7, b),
            "and whatever the dead run wrote, it wrote to its OWN file"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_ends_with_the_review_and_leaves_no_husk() {
        // Finishing or aborting while master is parked need not
        // produce another item, so the wait stays parked and nothing
        // else would stop this (codex 29431d9).
        let (repo, _run) = scratch(7);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let reader = Scripted(std::sync::Mutex::new([Ok(vec!["drafted"])].into()));

        let task = tokio::spawn({
            let repo = repo.clone();
            async move {
                drive_draft_source(
                    &repo,
                    7,
                    &|| true,
                    &reader,
                    tx,
                    std::time::Duration::from_secs(60),
                )
                .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        std::fs::remove_dir_all(pr_dir(&repo, 7)).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        assert!(
            task.is_finished(),
            "the watch stops with the review that owns it"
        );
        assert!(
            !pr_dir(&repo, 7).exists(),
            "and no read landing late rebuilds the scratch directory — a husk with a \
             draft.json and no pr.json makes `start` refuse to restart that PR"
        );
    }

    #[test]
    fn a_horizon_cannot_be_written_for_a_review_that_is_gone() {
        let (repo, run) = scratch(7);
        std::fs::remove_dir_all(pr_dir(&repo, 7)).unwrap();
        assert!(save_draft_horizon(&repo, 7, run, &DraftHorizon::default()).is_err());
        assert!(!pr_dir(&repo, 7).exists());
    }

    #[test]
    fn a_review_starts_from_no_draft_rather_than_from_never_having_looked() {
        // The gap this closes: round 0 hands master a PrMaster item, so
        // the first wait that PARKS is after master has drafted. A
        // baseline taken then swallows an operator edit made in
        // between.
        let mut started = DraftHorizon::started();
        assert_eq!(
            started.absorb_snapshot(9, &[(1, "master's first comment")]),
            [1],
            "the first comment on a review that began empty is a change, not a baseline"
        );
        let mut never_looked = DraftHorizon::default();
        assert!(
            never_looked
                .absorb_snapshot(9, &[(1, "master's first comment")])
                .is_empty(),
            "which is exactly what a default horizon cannot say"
        );
    }

    #[test]
    fn the_review_watch_is_this_pr_and_what_the_feed_actually_reports() {
        let w = review_source("o/r", 42);
        assert_eq!(w.prs, [42]);
        assert!(
            w.events.iter().all(|k| k.is_pr_scoped()),
            "a kind the `prs` list cannot confine would leak the rest of the repo: {:?}",
            w.events
        );
        assert_eq!(
            w.events,
            [
                GithubEventKind::PrComment,
                GithubEventKind::PrUpdated,
                GithubEventKind::PrMerged
            ],
            "comments, pushes and the merge — what the feed reports as it happens"
        );
        assert!(
            w.include_own_actions,
            "one shared gh identity, so the default hides the human's own comments"
        );
        assert!(
            w.prompt.is_some(),
            "a watch nobody wrote has to say what it is for"
        );
    }

    #[test]
    fn a_horizon_is_never_rebaselined_by_the_round() {
        // An interval cannot stand in for authorship: whatever the
        // OPERATOR did inside it is erased along with master's own
        // writes, and that is the only thing this watch exists to
        // catch (codex ce892ea).
        let (repo, run) = scratch(7);
        let mut h = DraftHorizon::default();
        h.absorb_snapshot(9, &[(1, "drafted")]);
        save_draft_horizon(&repo, 7, run, &h).unwrap();

        let mut h = load_draft_horizon(&repo, 7, run);
        assert_eq!(
            h.absorb_snapshot(9, &[(1, "the operator changed it while master was busy")]),
            [1],
            "an edit made between two rounds is still an edit"
        );
    }

    #[test]
    fn start_fork_and_checkout_are_mutually_exclusive() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            f: crate::cli::PrReviewStartArgs,
        }
        assert!(T::try_parse_from(["t", "5"]).is_ok());
        assert!(T::try_parse_from(["t", "5", "--fork"]).is_ok());
        assert!(T::try_parse_from(["t", "5", "--checkout"]).is_ok());
        assert!(
            T::try_parse_from(["t", "5", "--fork", "--checkout"]).is_err(),
            "--fork and --checkout must conflict"
        );
    }

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
            gh::discard_argv("o/r", 7, 42),
            vec!["api", "--method", "DELETE", "repos/o/r/pulls/7/reviews/42"]
        );
    }

    /// The classifier reads the SERVER's answer, not the request.
    ///
    /// `GhFailure` keeps them apart on purpose: the argv carries
    /// `body=<the review summary>`, and a summary is free text an
    /// agent wrote. A review OF this very bug would quote GitHub's
    /// refusal in its summary — and then every transient failure on
    /// that review would be classified permanent and its retry
    /// suppressed (codex 904bc9b).
    #[test]
    fn a_summary_quoting_the_refusal_does_not_make_a_failure_permanent() {
        let refusal = "Could not edit a review with a missing body";
        // A real review summary about this bug. It is the argv, not
        // the answer.
        let argv = gh::update_body_argv(
            "o/r",
            123,
            42,
            &format!("The handoff broke because GitHub says \"{refusal}\"."),
        );
        assert!(
            argv.iter().any(|a| a.contains(refusal)),
            "the fixture must actually carry the phrase in the request",
        );

        let transient: anyhow::Error = gh::GhFailure {
            said: "HTTP 502: Bad Gateway".to_string(),
            argv: argv.clone(),
        }
        .into();
        assert!(
            format!("{transient:#}").contains(refusal),
            "the formatted chain DOES contain it -- that is the trap",
        );
        assert!(
            !is_missing_body_refusal(&transient),
            "a 502 is retryable however the summary is worded",
        );

        // And the real thing is still recognised.
        let real: anyhow::Error = gh::GhFailure {
            said: format!("HTTP 422: {refusal} (https://api.github.com/...)"),
            argv,
        }
        .into();
        assert!(is_missing_body_refusal(&real));

        // An error that is not a gh failure at all classifies as
        // retryable rather than matching on stray text.
        assert!(!is_missing_body_refusal(&anyhow::anyhow!(
            "spawning gh: {refusal}"
        )));
    }

    /// Only the server's answer reaches `said`.
    ///
    /// `run_gh` needs a real `gh`, so the split it performs lives in
    /// `failure_from` where it can be asked directly. Folding the argv
    /// into `said` would make the classifier read our own request back
    /// as the verdict, which is the bug the type exists to prevent.
    #[test]
    fn the_servers_answer_is_kept_apart_from_our_request() {
        let refusal = "Could not edit a review with a missing body";
        let argv = gh::update_body_argv("o/r", 123, 42, &format!("GitHub says \"{refusal}\"."));
        let f = gh::failure_from(&argv, b"  HTTP 502: Bad Gateway\n");

        assert_eq!(
            f.said, "HTTP 502: Bad Gateway",
            "stderr, trimmed, and nothing else"
        );
        assert!(
            !f.said.contains(refusal),
            "the request must not leak into the answer: {}",
            f.said
        );
        assert!(
            f.argv.iter().any(|a| a.contains(refusal)),
            "while the request is kept, for the operator to read",
        );
        assert!(
            !is_missing_body_refusal(&f.into()),
            "so a 502 stays retryable however the summary is worded",
        );
    }

    #[test]
    fn finish_states_no_outcome() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            f: crate::cli::PrReviewFinishArgs,
        }
        // The whole point: clank no longer names a GitHub review
        // outcome, so there is nothing to pass and nothing required.
        assert!(T::try_parse_from(["t"]).is_ok());
        assert!(T::try_parse_from(["t", "--pr", "7"]).is_ok());
    }

    #[test]
    fn the_sweep_reaches_master_s_own_comments() {
        // The 👍 that matters is on a TOP-LEVEL comment — a reviewer
        // approving master's draft. Asking only about replies would
        // leave exactly the reactions this is for.
        let asked = std::sync::Mutex::new(Vec::new());
        let call = |argv: &[String]| -> anyhow::Result<String> {
            let url = argv.iter().find(|a| a.contains("repos/")).unwrap().clone();
            asked.lock().unwrap().push(url.clone());
            Ok(if url.ends_with("/comments") {
                // One top-level comment, one reply.
                r#"[[{"id": 10}, {"id": 11, "in_reply_to_id": 10}]]"#.to_string()
            } else if url.contains("/comments/10/reactions") {
                r#"[[{"id": 500, "content": "+1"}]]"#.to_string()
            } else {
                "[[]]".to_string()
            })
        };
        let removed = gh::sweep_thumbs_with("o/r", 7, 42, &call).unwrap();
        assert_eq!(removed, 1, "the top-level comment's 👍 was removed");
        let asked = asked.lock().unwrap().clone();
        assert!(
            asked
                .iter()
                .any(|u| u == "repos/o/r/pulls/comments/10/reactions"),
            "master's own comment was asked about: {asked:?}",
        );
        assert!(
            asked
                .iter()
                .any(|u| u == "repos/o/r/pulls/comments/10/reactions/500"),
            "and its 👍 deleted: {asked:?}",
        );
    }

    #[test]
    fn sweeping_a_cleaned_draft_changes_nothing() {
        // The plan promises finish is retryable, which only means
        // something if the second run is a no-op. A fake that keeps
        // returning the reaction it was told to delete cannot show
        // that, so this one removes it — and keeps the reaction that
        // was never ours.
        let state = std::sync::Mutex::new(vec![
            (500u64, "+1".to_string()),
            (501u64, "heart".to_string()),
        ]);
        let deletes = std::sync::Mutex::new(0usize);
        let call = |argv: &[String]| -> anyhow::Result<String> {
            let url = argv.iter().find(|a| a.contains("repos/")).unwrap().clone();
            if url.ends_with("/comments") {
                return Ok(r#"[[{"id": 10}]]"#.to_string());
            }
            if let Some(rest) = url.strip_prefix("repos/o/r/pulls/comments/10/reactions") {
                if let Some(id) = rest.strip_prefix('/') {
                    let id: u64 = id.parse().unwrap();
                    state.lock().unwrap().retain(|(r, _)| *r != id);
                    *deletes.lock().unwrap() += 1;
                    return Ok(String::new());
                }
                let page: Vec<String> = state
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, content)| format!(r#"{{"id": {id}, "content": "{content}"}}"#))
                    .collect();
                return Ok(format!("[[{}]]", page.join(",")));
            }
            Ok("[[]]".to_string())
        };

        assert_eq!(gh::sweep_thumbs_with("o/r", 7, 42, &call).unwrap(), 1);
        assert_eq!(
            gh::sweep_thumbs_with("o/r", 7, 42, &call).unwrap(),
            0,
            "the draft is already clean: nothing more to remove",
        );
        assert_eq!(*deletes.lock().unwrap(), 1, "and nothing deleted twice");
        assert_eq!(
            *state.lock().unwrap(),
            vec![(501, "heart".to_string())],
            "the reaction that was never the loop's is still there",
        );
    }

    #[test]
    fn only_the_loops_thumbs_are_swept() {
        // A reviewer's 👍 is scaffolding. Anything else on the
        // comment belongs to whoever put it there.
        let json = r#"[[
            {"id": 1, "content": "+1"},
            {"id": 2, "content": "heart"},
            {"id": 3, "content": "+1"},
            {"id": 4, "content": "-1"}
        ]]"#;
        assert_eq!(gh::parse_thumbs_up_ids(json).unwrap(), vec![1, 3]);
    }

    #[test]
    fn reactions_are_read_from_every_comment_not_just_replies() {
        // Reviewers react on MASTER's top-level comments, which is
        // exactly what the reply parser filters out.
        let json = r#"[[
            {"id": 10},
            {"id": 11, "in_reply_to_id": 10}
        ]]"#;
        assert_eq!(gh::parse_comment_ids(json).unwrap(), vec![10, 11]);
        assert_eq!(gh::parse_reply_ids(json).unwrap(), vec![11]);
    }

    #[test]
    fn the_reaction_endpoints_are_the_documented_ones() {
        assert_eq!(
            gh::comment_reactions_argv("o/r", 99),
            vec![
                "api",
                "repos/o/r/pulls/comments/99/reactions",
                "--paginate",
                "--slurp",
            ]
        );
        assert_eq!(
            gh::delete_reaction_argv("o/r", 99, 7),
            vec![
                "api",
                "--method",
                "DELETE",
                "repos/o/r/pulls/comments/99/reactions/7",
            ]
        );
    }

    #[test]
    fn a_summary_only_draft_is_pinned_and_unsubmitted() {
        let argv = gh::create_summary_argv("o/r", 7, "deadbee", "the summary");
        assert_eq!(
            argv,
            vec![
                "api",
                "--method",
                "POST",
                "repos/o/r/pulls/7/reviews",
                "-f",
                "commit_id=deadbee",
                "-f",
                "body=the summary",
            ],
            "the review is attached to what was READ, not to the PR's head now",
        );
        assert!(
            !argv.iter().any(|a| a.starts_with("event=")),
            "and no event, which is what leaves it pending: {argv:?}",
        );
    }

    #[test]
    fn the_body_endpoint_updates_without_submitting() {
        let argv = gh::update_body_argv("o/r", 7, 42, "the summary");
        assert_eq!(
            argv,
            vec![
                "api",
                "--method",
                "PUT",
                "repos/o/r/pulls/7/reviews/42",
                "-f",
                "body=the summary",
            ],
            "PUT on the review itself — not the /events endpoint, which submits",
        );
        assert!(
            !argv.iter().any(|a| a.contains("events")),
            "nothing here can publish: {argv:?}",
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
    }

    #[test]
    fn extract_summary_matrix() {
        let ok = "# notes\n\n## Review summary\nShip it: the refactor is clean.\n";
        assert_eq!(
            extract_summary(ok).unwrap(),
            "Ship it: the refactor is clean."
        );
        // Stops at the next ## heading.
        let multi = "## Review summary\nthe summary\n\n## Other\nignored\n";
        assert_eq!(extract_summary(multi).unwrap(), "the summary");
        // Missing section, empty, and the unfilled placeholder all error.
        assert!(extract_summary("# notes\nno section").is_err());
        assert!(extract_summary("## Review summary\n\n").is_err());
        assert!(extract_summary("## Review summary\n<the text…>\n").is_err());
    }
}
