//! `clank wfw` — block until the calling agent has work to do.
//!
//! Two roles. `--role master` watches for plans where the gate has
//! moved on without master (ChangesRequested, Approved, or
//! Approved+dirty). `--role reviewers` watches for plans whose
//! latest reviewable commit needs an opinion from `--author`.
//!
//! `wfw` runs the same projection `clank status` does, then filters
//! it through `clank_core::work::derive_work` for the agent's
//! perspective. When the initial fold finds work it prints it and
//! exits; otherwise it watches the filesystem for changes that
//! could plausibly flip the projection and refolds on each
//! debounced event.

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
use clank_core::work::{Role, WorkItem, derive_work};

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
    let plan_filter = match args.plan.as_deref() {
        Some(raw) => Some({
            let stem = parse_arg(raw, &basename)?;
            PlanKey::parse(&stem)
                .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?
        }),
        None => None,
    };

    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };

    if let Some(items) = check_once(&repo, policy, &plan_filter, &author, role).await? {
        emit(&items, args.json);
        return Ok(());
    }

    let watch_ctx = WatchContext::resolve(&repo)?;
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = build_watcher(tx)?;
    watch_ctx.attach(&mut watcher)?;

    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        let wait = match deadline {
            Some(end) => end.checked_duration_since(std::time::Instant::now()),
            None => Some(Duration::from_secs(3600)),
        };
        let wait = match wait {
            Some(d) => d,
            None => return Err(WfwTimeout.into()),
        };
        match rx.recv_timeout(wait) {
            Ok(()) => {
                // Drain any further events that landed inside the
                // debounce window — `notify` fires once per OS event
                // and we only want one refold per logical change.
                while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
                if let Some(items) =
                    check_once(&repo, policy, &plan_filter, &author, role).await?
                {
                    emit(&items, args.json);
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(WfwTimeout.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

async fn check_once(
    repo: &Path,
    policy: crate::rebuild::CachePolicy,
    plan_filter: &Option<PlanKey>,
    author: &AgentLabel,
    role: Role,
) -> anyhow::Result<Option<Vec<WorkItem>>> {
    let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let plans: Vec<PlanKey> = match plan_filter {
        Some(k) if state.fold.plans.contains_key(k) => vec![k.clone()],
        Some(_) => return Ok(None),
        None => state.fold.plans.keys().cloned().collect(),
    };

    let mut views: Vec<PlanView> = Vec::with_capacity(plans.len());
    for key in &plans {
        if let Some(view) = build_view(repo, &state, key).await? {
            views.push(view);
        }
    }
    let items = derive_work(&views, author, role);
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

fn emit(items: &[WorkItem], json: bool) {
    if json {
        for item in items {
            let v = render_json(item);
            println!("{}", serde_json::to_string(&v).expect("serialize WorkItem"));
        }
    } else {
        for item in items {
            println!("{}", render_human(item));
        }
    }
}

fn render_json(item: &WorkItem) -> serde_json::Value {
    match item {
        WorkItem::MasterAction { plan, sha, next } => serde_json::json!({
            "kind": "master",
            "plan": plan.as_str(),
            "plan_path": format!(".clank/plans/{}.md", plan.as_str()),
            "sha": sha.as_str(),
            "next": next,
        }),
        WorkItem::ReviewerAction {
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
    }
}

fn render_human(item: &WorkItem) -> String {
    match item {
        WorkItem::MasterAction { plan, sha, next } => format!(
            "master  {plan}  {sha}  next={next:?}",
            plan = plan.as_str(),
            sha = &sha.as_str()[..sha.as_str().len().min(7)],
            next = next
        ),
        WorkItem::ReviewerAction {
            plan,
            sha,
            feedback_path,
        } => format!(
            "review  {plan}  {sha}  write {feedback_path}",
            plan = plan.as_str(),
            sha = &sha.as_str()[..sha.as_str().len().min(7)],
            feedback_path = feedback_path,
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
        let mut watched: HashSet<PathBuf> = HashSet::new();
        let mut try_watch = |path: PathBuf, mode: RecursiveMode| -> anyhow::Result<()> {
            if !path.exists() {
                return Ok(());
            }
            if !watched.insert(path.clone()) {
                return Ok(());
            }
            watcher
                .watch(&path, mode)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", path.display()))
        };

        try_watch(self.git_dir.join("HEAD"), RecursiveMode::NonRecursive)?;
        try_watch(
            self.git_common_dir.join("refs"),
            RecursiveMode::Recursive,
        )?;
        try_watch(
            self.git_common_dir.join("packed-refs"),
            RecursiveMode::NonRecursive,
        )?;
        try_watch(self.repo_root.join(".clank/plans"), RecursiveMode::Recursive)?;
        try_watch(
            self.repo_root.join(".clank/feedback"),
            RecursiveMode::Recursive,
        )?;
        try_watch(
            self.repo_root.join(".clank/finished"),
            RecursiveMode::Recursive,
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
    let absolute = if p.is_absolute() {
        p
    } else {
        repo.join(p)
    };
    Ok(dunce::canonicalize(&absolute).unwrap_or(absolute))
}

fn build_watcher(tx: mpsc::Sender<()>) -> anyhow::Result<RecommendedWatcher> {
    Ok(notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            let _ = tx.send(());
        }
    })?)
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
        "m" => n.checked_mul(60).ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
        "h" => n.checked_mul(3600).ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
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
        assert_eq!(parse_timeout("1h").unwrap(), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn parse_timeout_rejects_garbage() {
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("5x").is_err());
    }
}
