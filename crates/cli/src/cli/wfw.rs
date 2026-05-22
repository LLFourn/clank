//! `clank wfw` — block until the calling agent has wait-surface
//! items to print.
//!
//! Two roles. `--role master` watches for plans where the gate
//! has moved on without master. `--role reviewers` watches for
//! plans whose latest reviewable commit needs an opinion from
//! `--author`. Both also receive `Finished` notices when a
//! watched plan transitions into `finished_plans` — wfw's exit
//! is positive in either case.
//!
//! `wfw` runs the same projection `clank status` does, then
//! filters it through `clank_core::wait::derive_work` for the
//! agent's perspective, then concatenates any
//! `detect_finished` notices from a startup snapshot. When the
//! initial fold finds at least one item it prints it and exits;
//! otherwise it watches the filesystem for changes that could
//! plausibly flip the projection and refolds on each debounced
//! event.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::{WfwArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::feedback_scan::scan_feedback;
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use crate::repo_state::RepoState;
use crate::worktree_facts::read_worktree_facts;
use clank_core::plan_view::{PlanView, project};
use clank_core::wait::{Role, StartupSnapshot, WaitItem, derive_work, detect_finished};

/// Exit code returned when `--timeout` elapses without producing
/// any work. The rest of the CLI uses anyhow for normal errors;
/// timeout is the one expected non-zero exit, so we surface it via
/// a sentinel error type instead of `process::exit` so `main` can
/// translate it cleanly.
#[derive(Debug)]
pub struct WfwTimeout;
impl std::fmt::Display for WfwTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wfw timed out")
    }
}
impl std::error::Error for WfwTimeout {}

pub async fn run(args: WfwArgs) -> anyhow::Result<()> {
    let author = AgentLabel::parse(&args.author)
        .map_err(|e| anyhow::anyhow!("invalid --author `{}`: {e}", args.author))?;
    let role: Role = args.role.into();
    let timeout = parse_timeout(&args.timeout)?;

    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let initial_state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

    let plan_filter = match args.plan.as_deref() {
        Some(raw) => {
            let stem = parse_arg(raw, &basename)?;
            let key = PlanKey::parse(&stem)
                .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
            let is_active = initial_state.fold.plans.contains_key(&key);
            let finished_at = initial_state
                .fold
                .finished_plans
                .iter()
                .rev()
                .find(|fp| fp.plan == key)
                .map(|fp| fp.finalized_at.clone());
            if !is_active && finished_at.is_none() {
                anyhow::bail!(
                    "plan `{basename}/{stem}.md` not active. active: {}",
                    active_summary(&initial_state, &basename)
                );
            }
            if !is_active {
                // Already-finished plan and explicit `--plan`: emit
                // a one-shot Finished notice and exit. No watch loop.
                // This is the "agent resumed with stale state" path.
                let item = WaitItem::Finished {
                    plan: key.clone(),
                    finalized_at: finished_at.expect("checked just above"),
                };
                emit(&[item], args.json);
                return Ok(());
            }
            Some(key)
        }
        None => None,
    };

    let snapshot = StartupSnapshot::capture(&initial_state.fold, plan_filter.as_ref());

    if let Some(items) = derive_from_state(
        &repo,
        &initial_state,
        &plan_filter,
        &snapshot,
        &author,
        role,
    )
    .await?
    {
        emit(&items, args.json);
        return Ok(());
    }

    let watch_ctx = WatchContext::resolve(&repo)?;
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = build_watcher(tx)?;
    watch_ctx.attach(&mut watcher)?;

    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        // Heartbeat refold cadence. We refold whenever an FS event
        // fires AND on this fixed interval, even with no events.
        // The heartbeat is the safety net for the finalize-race
        // codex caught: `clank finish` writes the snapshot files
        // before committing, so the FS event wakes wfw, the
        // refold lands BEFORE the commit, sees nothing, and the
        // post-commit git-ref event may never fire reliably. The
        // periodic refold catches that case. 1500ms gives finalize
        // notifications a snappy ceiling without burning much CPU
        // on a warm cache.
        const HEARTBEAT: Duration = Duration::from_millis(1500);
        let wait = match deadline {
            None => HEARTBEAT,
            Some(end) => match end.checked_duration_since(std::time::Instant::now()) {
                None => return Err(WfwTimeout.into()),
                Some(remaining) => remaining.min(HEARTBEAT),
            },
        };
        let event_received = match rx.recv_timeout(wait) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Could be deadline-expiry or heartbeat-tick. Distinguish.
                if let Some(end) = deadline {
                    if std::time::Instant::now() >= end {
                        return Err(WfwTimeout.into());
                    }
                }
                false
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        };
        if event_received {
            // Debounce: drain bursts so one logical change → one
            // refold. Only meaningful on the FS-event branch; the
            // heartbeat tick has nothing to drain.
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
        }
        if let Some(items) =
            check_once(&repo, policy, &plan_filter, &snapshot, &author, role).await?
        {
            emit(&items, args.json);
            return Ok(());
        }
    }
}

fn active_summary(state: &RepoState, basename: &str) -> String {
    let names: Vec<String> = state
        .fold
        .plans
        .keys()
        .map(|k| format!("{basename}/{}.md", k.as_str()))
        .collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

async fn check_once(
    repo: &Path,
    policy: crate::rebuild::CachePolicy,
    plan_filter: &Option<PlanKey>,
    snapshot: &StartupSnapshot,
    author: &AgentLabel,
    role: Role,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    derive_from_state(repo, &state, plan_filter, snapshot, author, role).await
}

async fn derive_from_state(
    repo: &Path,
    state: &RepoState,
    plan_filter: &Option<PlanKey>,
    snapshot: &StartupSnapshot,
    author: &AgentLabel,
    role: Role,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    // Plans to derive work over. With `--plan` and the plan still
    // active, restrict to it. With `--plan` and the plan gone
    // (finished or deleted mid-watch), skip the work derivation —
    // `detect_finished` will still surface the finalize notice
    // from the snapshot.
    let plans: Vec<PlanKey> = match plan_filter {
        Some(k) if state.fold.plans.contains_key(k) => vec![k.clone()],
        Some(_) => Vec::new(),
        None => state.fold.plans.keys().cloned().collect(),
    };

    let mut views: Vec<PlanView> = Vec::with_capacity(plans.len());
    for key in &plans {
        if let Some(view) = build_view(repo, state, key).await? {
            views.push(view);
        }
    }
    let mut items = derive_work(&views, author, role);
    items.extend(detect_finished(snapshot, &state.fold));
    if items.is_empty() {
        Ok(None)
    } else {
        Ok(Some(items))
    }
}

async fn build_view(
    repo: &Path,
    state: &RepoState,
    plan: &PlanKey,
) -> anyhow::Result<Option<PlanView>> {
    let ps = match state.fold.plans.get(plan) {
        Some(ps) => ps,
        None => return Ok(None),
    };
    let reviewable: Vec<CommitSha> = ps
        .commits
        .iter()
        .filter(|e| e.touched_plan || e.touched_code)
        .map(|e| e.sha.clone())
        .collect();
    let feedback = scan_feedback(repo, plan, &reviewable)?;
    let plan_path = format!(".clank/plans/{}.md", plan.as_str());
    let worktree = read_worktree_facts(repo, &plan_path, state.head.as_ref()).await?;
    Ok(project(&state.fold, plan, &feedback, &worktree))
}

fn emit(items: &[WaitItem], json: bool) {
    if json {
        let envelope = serde_json::json!({
            "items": items.iter().map(render_json).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string(&envelope).expect("serialize wfw envelope")
        );
    } else {
        for item in items {
            println!("{}", render_human(item));
        }
    }
}

fn short(sha: &CommitSha) -> &str {
    &sha.as_str()[..sha.as_str().len().min(7)]
}

fn render_json(item: &WaitItem) -> serde_json::Value {
    match item {
        WaitItem::Master {
            plan,
            sha,
            next,
            reason,
        } => serde_json::json!({
            "kind": "master",
            "plan": plan.as_str(),
            "plan_path": format!(".clank/plans/{}.md", plan.as_str()),
            "sha": sha.as_str(),
            "next": next,
            "reason": reason,
        }),
        WaitItem::Reviewer {
            plan,
            sha,
            feedback_path,
        } => serde_json::json!({
            "kind": "reviewer",
            "plan": plan.as_str(),
            "plan_path": format!(".clank/plans/{}.md", plan.as_str()),
            "sha": sha.as_str(),
            "feedback_path": feedback_path,
        }),
        WaitItem::Finished { plan, finalized_at } => serde_json::json!({
            "kind": "finished",
            "plan": plan.as_str(),
            "plan_path": format!(".clank/plans/{}.md", plan.as_str()),
            "finalized_at": finalized_at.as_str(),
        }),
    }
}

fn render_human(item: &WaitItem) -> String {
    match item {
        WaitItem::Master {
            plan,
            sha,
            next,
            reason,
        } => format!(
            "master   {plan}  {sha}  next={next:?}  reason={reason}",
            plan = plan.as_str(),
            sha = short(sha),
            next = next,
            reason = reason,
        ),
        WaitItem::Reviewer {
            plan,
            sha,
            feedback_path,
        } => format!(
            "review   {plan}  {sha}  write {feedback_path}",
            plan = plan.as_str(),
            sha = short(sha),
            feedback_path = feedback_path,
        ),
        WaitItem::Finished { plan, finalized_at } => format!(
            "finished {plan}  {sha}",
            plan = plan.as_str(),
            sha = short(finalized_at),
        ),
    }
}

/// Watch-time facts captured up-front so the loop wakes for the
/// right git refs regardless of whether `repo` is a main worktree,
/// a linked worktree, or sits on top of a submodule gitdir.
struct WatchContext {
    repo_root: PathBuf,
    /// Worktree-specific git dir (`.git/worktrees/<name>/` for a
    /// linked worktree; `<repo>/.git/` for the main worktree).
    /// `HEAD` here is the one this process resolves.
    git_dir: PathBuf,
    /// Common git dir (always `<repo>/.git/` for the main worktree;
    /// usually `<main>/.git/` for linked worktrees). Holds the
    /// shared `refs/` and `packed-refs`.
    git_common_dir: PathBuf,
}

impl WatchContext {
    fn resolve(repo: &Path) -> anyhow::Result<Self> {
        let git_dir = git_resolve_dir(repo, "--git-dir")?;
        let git_common_dir = git_resolve_dir(repo, "--git-common-dir")?;
        Ok(Self {
            repo_root: repo.to_path_buf(),
            git_dir,
            git_common_dir,
        })
    }

    fn attach(&self, watcher: &mut RecommendedWatcher) -> anyhow::Result<()> {
        // `.clank/{plans,feedback,finished}` may not exist yet on a
        // brand-new repo or before any reviewer has weighed in.
        // `notify` refuses to watch a missing path, so create them
        // first — they're Clank-managed dirs anyway. The git refs
        // we tolerate as missing (e.g. `packed-refs` only appears
        // after `git gc`).
        for sub in [".clank/plans", ".clank/feedback", ".clank/finished"] {
            let p = self.repo_root.join(sub);
            if let Err(e) = std::fs::create_dir_all(&p) {
                anyhow::bail!("ensure `{}` exists: {e}", p.display());
            }
        }

        let mut watched: HashSet<PathBuf> = HashSet::new();
        let mut try_watch =
            |path: PathBuf, mode: RecursiveMode, tolerate_missing: bool| -> anyhow::Result<()> {
                if !path.exists() {
                    if tolerate_missing {
                        return Ok(());
                    }
                    anyhow::bail!("required watch path `{}` does not exist", path.display());
                }
                if !watched.insert(path.clone()) {
                    return Ok(());
                }
                watcher
                    .watch(&path, mode)
                    .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", path.display()))
            };

        // `.git/HEAD` only changes on checkout/symbolic-ref. Local
        // commits don't touch it. The canonical "HEAD moved" signal
        // for the current worktree is the reflog `logs/HEAD` — git
        // appends a line to it on every commit, rebase, reset, etc.
        // Watch both so we wake on both branch switches AND ordinary
        // commits, including code-only commits that don't touch any
        // `.clank/` paths.
        try_watch(
            self.git_dir.join("HEAD"),
            RecursiveMode::NonRecursive,
            false,
        )?;
        try_watch(
            self.git_dir.join("logs/HEAD"),
            RecursiveMode::NonRecursive,
            // Pristine repos with no commits yet have no logs/HEAD.
            // Tolerate that — the file appears on the first commit
            // and notify auto-rewatches via the parent .git dir.
            true,
        )?;
        // The common refs tree carries every branch, tag, and
        // remote ref — needed for picking up commits made via a
        // separate `git` invocation (especially from another linked
        // worktree).
        try_watch(
            self.git_common_dir.join("refs"),
            RecursiveMode::Recursive,
            false,
        )?;
        try_watch(
            self.git_common_dir.join("packed-refs"),
            RecursiveMode::NonRecursive,
            true,
        )?;
        try_watch(
            self.repo_root.join(".clank/plans"),
            RecursiveMode::Recursive,
            false,
        )?;
        try_watch(
            self.repo_root.join(".clank/feedback"),
            RecursiveMode::Recursive,
            false,
        )?;
        try_watch(
            self.repo_root.join(".clank/finished"),
            RecursiveMode::Recursive,
            false,
        )?;
        Ok(())
    }
}

fn git_resolve_dir(repo: &Path, flag: &str) -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", flag])
        .output()
        .map_err(|e| anyhow::anyhow!("git rev-parse {flag}: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git rev-parse {flag} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let p = PathBuf::from(&raw);
    let absolute = if p.is_absolute() { p } else { repo.join(p) };
    Ok(dunce::canonicalize(&absolute).unwrap_or(absolute))
}

fn build_watcher(tx: mpsc::Sender<()>) -> anyhow::Result<RecommendedWatcher> {
    Ok(notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if !matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    return;
                }
                let _ = tx.send(());
            }
        },
    )?)
}

fn parse_timeout(raw: &str) -> anyhow::Result<Option<Duration>> {
    let trimmed = raw.trim();
    if trimmed == "0" || trimmed.is_empty() {
        return Ok(None);
    }
    let (num, unit) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid --timeout `{raw}` (expected e.g. 30s, 5m, 1h)"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n
            .checked_mul(60)
            .ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
        "h" => n
            .checked_mul(3600)
            .ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
        other => anyhow::bail!("invalid --timeout unit `{other}` (use s, m, or h)"),
    };
    Ok(Some(Duration::from_secs(secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timeout_zero_is_indefinite() {
        assert!(parse_timeout("0").unwrap().is_none());
        assert!(parse_timeout("").unwrap().is_none());
    }

    #[test]
    fn parse_timeout_units() {
        assert_eq!(parse_timeout("30s").unwrap(), Some(Duration::from_secs(30)));
        assert_eq!(parse_timeout("30").unwrap(), Some(Duration::from_secs(30)));
        assert_eq!(parse_timeout("5m").unwrap(), Some(Duration::from_secs(300)));
        assert_eq!(
            parse_timeout("1h").unwrap(),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn parse_timeout_rejects_garbage() {
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("5x").is_err());
    }
}
