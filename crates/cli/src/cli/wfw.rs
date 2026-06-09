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
//! `wfw` folds the repo, runs `RepoState::derive_status` (which
//! threads `wait::compute_gate` over every plan), filters the
//! result through `RepoState::work_for` for the agent's
//! perspective, then concatenates any `detect_finished` notices
//! from a startup snapshot. When the initial fold finds at least
//! one item it prints it and exits; otherwise it watches the
//! filesystem for changes that could plausibly flip the
//! projection and refolds on each debounced event.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{WfwArgs, repo_basename, resolve_repo};
use crate::cli::block::scan_blocks;
use crate::cli::plan_resolve::parse_arg;
use crate::hook_config::{self, HookFiring};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use crate::repo_state::RepoState;
use clank_core::Role;
use clank_core::vocab::HookEvent;
use clank_core::wait::{StartupSnapshot, WaitItem, detect_finished};

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

fn firings_from_items(items: &[WaitItem]) -> Vec<HookFiring> {
    items
        .iter()
        .filter_map(|item| match item {
            WaitItem::Master {
                plan,
                sha,
                next,
                gate,
                ..
            } => Some(HookFiring {
                event: HookEvent::MasterWork,
                plan: plan.clone(),
                sha: sha.clone(),
                gate: Some(*gate),
                next: Some(format!("{next:?}").to_lowercase()),
            }),
            WaitItem::Reviewer { plan, sha, .. } => Some(HookFiring {
                event: HookEvent::ReviewerWork,
                plan: plan.clone(),
                sha: sha.clone(),
                gate: None,
                next: None,
            }),
            WaitItem::Finished { plan, finalized_at } => Some(HookFiring {
                event: HookEvent::PlanFinalized,
                plan: plan.clone(),
                sha: finalized_at.clone(),
                gate: None,
                next: None,
            }),
            WaitItem::Idle { .. }
            | WaitItem::AdHocReview { .. }
            | WaitItem::AdHocRevise { .. }
            | WaitItem::PromoteFromQueue { .. }
            | WaitItem::Blocked { .. }
            | WaitItem::Unblocked { .. } => None,
        })
        .collect()
}

pub async fn run(args: WfwArgs) -> anyhow::Result<()> {
    let timeout = parse_timeout(&args.timeout)?;
    // One env read for the entire process, here at the CLI
    // boundary. Downstream takes a plain `bool`.
    let poll_mode = args.effective_poll();

    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

    // Resolve --author via the shared identity resolver when
    // omitted. Same precedence rule as `clank auto`: explicit
    // flag > CLANK_AGENT env > session lookup. Error with an
    // actionable bootstrap hint if nothing resolves.
    let author = match args.author.as_deref() {
        Some(raw) => {
            AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?
        }
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };

    // Resolve --role from the team when omitted: `resolve_role`
    // returns Master for the team's master and Reviewer for any
    // tier member (`teams-based-agent-registration`). Errors if
    // the repo has no team configured.
    let role: Role = match args.role {
        Some(explicit) => explicit.into(),
        None => crate::agent_store::resolve_role(&repo, &author)?,
    };

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

    let config = crate::cli::config::load(&repo);
    let hook_config = config.hooks.clone();
    // Reviewer tiers from the team resolver. wfw is a workflow
    // command, so it hard-errors when no team is configured
    // (`teams-based-agent-registration` render-vs-workflow
    // boundary).
    let (commit_reviewers, gate_reviewers) = crate::agent_store::load_reviewer_tiers(&repo)?;
    let work_policy = clank_core::wait::WorkPolicy {
        plan_feedback: config.review.plan_feedback,
        adhoc_feedback: config.review.adhoc_feedback,
        commit_reviewers,
        gate_reviewers,
    };

    let snapshot = StartupSnapshot::capture(&initial_state.fold, plan_filter.as_ref());

    let initial_suppress_all;
    let initial_block_items: Vec<WaitItem>;
    let initial_suppressed_plans;
    {
        let br = check_blocks(&repo, &author);
        initial_suppress_all = br.suppress_all;
        let has_answer = br
            .items
            .iter()
            .any(|i| matches!(i, WaitItem::Unblocked { .. }));
        if has_answer {
            emit(&br.items, args.json);
            return Ok(());
        }

        if !br.suppress_all {
            let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(
                &repo,
                initial_state.head.as_ref(),
            );
            let status = initial_state.fold.derive_status(&reviews, &work_policy);
            let mut items = status.work_for(&author, role);
            if let Some(ref pf) = plan_filter {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. }
                    | WaitItem::Reviewer { plan, .. }
                    | WaitItem::Finished { plan, .. } => plan == pf,
                    _ => false,
                });
            }
            if !br.suppressed_plans.is_empty() {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => {
                        !br.suppressed_plans.contains(plan)
                    }
                    _ => true,
                });
            }
            items.extend(detect_finished(&snapshot, &initial_state.fold));
            if !items.is_empty() {
                // Co-surface pending Blocked entries alongside
                // actionable items so a partial-block situation
                // (plan A blocked, plan B actionable) shows BOTH.
                // Codex caught the omission on 0a3c039.
                let blocked_items: Vec<_> = br
                    .items
                    .iter()
                    .filter(|i| matches!(i, WaitItem::Blocked { .. }))
                    .cloned()
                    .collect();
                items.extend(blocked_items);
                for firing in &firings_from_items(&items) {
                    hook_config::run_hook(&repo, &hook_config, firing);
                }
                emit(&items, args.json);
                return Ok(());
            }
        } // !suppress_all
        initial_block_items = br.items;
        initial_suppressed_plans = br.suppressed_plans;
    }

    // Master with no actionable plans (either no plans at all, or
    // every active plan suppressed by per-plan blocks) should be
    // pointed at the queue. The pre-fix gate checked
    // `fold.plans.is_empty()` only, so a master whose only active
    // plan was agent-blocked got no signal at all.
    let actionable = initial_state
        .fold
        .plans
        .iter()
        .filter(|(k, _)| !initial_suppressed_plans.contains(k))
        .count();
    if !initial_suppress_all && role == Role::Master && plan_filter.is_none() && actionable == 0 {
        let queue = match crate::cli::queue::scan_queue_no_dups(&repo) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("wfw: {e}");
                Vec::new()
            }
        };
        // Scan to the first queue item NOT suppressed by a plan-scoped
        // block. A block on the top-priority queued plan must not hide
        // lower-priority unblocked items — that would defeat the
        // queue. Repo-wide blocks short-circuit upstream
        // (`initial_suppress_all`) and don't reach this branch.
        let first_unsuppressed = queue.iter().find(|q| {
            !initial_suppressed_plans
                .iter()
                .any(|k| k.as_str() == q.name)
        });
        if let Some(first) = first_unsuppressed {
            let mut items = initial_block_items.clone();
            items.push(WaitItem::PromoteFromQueue {
                name: first.name.clone(),
                priority: first.priority,
            });
            emit(&items, args.json);
            return Ok(());
        }
        // Codex 62f22c2: when the queue is non-empty but every item
        // is suppressed by a plan-scoped block, we still emit the
        // Blocked items so the agent sees what's holding the queue.
        // (Empty-queue + plan-blocked behavior unchanged from today
        // — wfw parks. The change is scoped to "queue had items we
        // filtered out," matching the acceptance "plan-scoped blocks
        // continue to emit alongside the now-filtered promote scan.")
        if !queue.is_empty() && !initial_block_items.is_empty() {
            emit(&initial_block_items, args.json);
            return Ok(());
        }
        hook_config::run_idle_hook(&repo, &hook_config);
    }

    let watch_ctx = WatchContext::resolve(&repo, poll_mode)?;
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = build_watcher(tx)?;
    watch_ctx.attach(&mut watcher)?;

    // Refold cadence. Native mode uses a 1.5s heartbeat as the
    // finalize-race safety net (the watcher carries the load).
    // Polling mode shortens to 500ms — it IS the primary git-
    // change signal because the gitdir watch is skipped.
    let tick: Duration = if poll_mode {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(1500)
    };
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        let wait = match deadline {
            None => tick,
            Some(end) => match end.checked_duration_since(std::time::Instant::now()) {
                None => return Err(WfwTimeout.into()),
                Some(remaining) => remaining.min(tick),
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
        {
            let br = check_blocks(&repo, &author);
            let has_answer = br
                .items
                .iter()
                .any(|i| matches!(i, WaitItem::Unblocked { .. }));
            if has_answer {
                emit(&br.items, args.json);
                return Ok(());
            }
            if br.suppress_all {
                continue;
            }

            let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
                .await
                .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
            let reviews =
                crate::fs_plan_state_lookup::FsPlanStateLookup::new(&repo, state.head.as_ref());
            let status = state.fold.derive_status(&reviews, &work_policy);
            let mut items = status.work_for(&author, role);
            if let Some(ref pf) = plan_filter {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. }
                    | WaitItem::Reviewer { plan, .. }
                    | WaitItem::Finished { plan, .. } => plan == pf,
                    _ => false,
                });
            }
            if !br.suppressed_plans.is_empty() {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => {
                        !br.suppressed_plans.contains(plan)
                    }
                    _ => true,
                });
            }
            items.extend(detect_finished(&snapshot, &state.fold));
            if !items.is_empty() {
                // Co-surface pending Blocked entries (codex caught
                // on 0a3c039).
                let blocked_items: Vec<_> = br
                    .items
                    .iter()
                    .filter(|i| matches!(i, WaitItem::Blocked { .. }))
                    .cloned()
                    .collect();
                items.extend(blocked_items);
                for firing in &firings_from_items(&items) {
                    hook_config::run_hook(&repo, &hook_config, firing);
                }
                emit(&items, args.json);
                return Ok(());
            }
            // Same actionable-count gate as the initial pass — see
            // the comment above the initial gate. Watch-loop variant.
            let actionable_in_loop = state
                .fold
                .plans
                .iter()
                .filter(|(k, _)| !br.suppressed_plans.contains(k))
                .count();
            if role == Role::Master && plan_filter.is_none() && actionable_in_loop == 0 {
                let queue = match crate::cli::queue::scan_queue_no_dups(&repo) {
                    Ok(q) => q,
                    Err(e) => {
                        eprintln!("wfw: {e}");
                        Vec::new()
                    }
                };
                // Same scan-to-first-unsuppressed semantic as the
                // initial promote site above. Blocks on the top
                // priority queued plan must not hide lower-priority
                // unblocked items.
                let first_unsuppressed = queue
                    .iter()
                    .find(|q| !br.suppressed_plans.iter().any(|k| k.as_str() == q.name));
                if let Some(first) = first_unsuppressed {
                    let mut items = br.items.clone();
                    items.push(WaitItem::PromoteFromQueue {
                        name: first.name.clone(),
                        priority: first.priority,
                    });
                    emit(&items, args.json);
                    return Ok(());
                }
                // Codex 62f22c2: queue had items but every one was
                // suppressed — surface the Blocked items so the agent
                // sees the holding cause. Empty-queue path unchanged.
                if !queue.is_empty() && !br.items.is_empty() {
                    emit(&br.items, args.json);
                    return Ok(());
                }
            }
        }
    }
}

/// Check blocks and return (block_items, suppressed_plans). If a
/// `Unblocked` is found for the calling agent, it is returned as
/// the sole item with `suppress_all = true` so the caller exits
/// immediately. Pending blocks emit `Blocked` items and
/// suppress either all work (repo-scope) or specific plans.
struct BlockResult {
    items: Vec<WaitItem>,
    suppress_all: bool,
    suppressed_plans: std::collections::BTreeSet<PlanKey>,
}

fn check_blocks(repo: &Path, author: &AgentLabel) -> BlockResult {
    let blocks = scan_blocks(repo);
    let mut result = BlockResult {
        items: Vec::new(),
        suppress_all: false,
        suppressed_plans: std::collections::BTreeSet::new(),
    };

    for b in &blocks {
        if b.agent == author.as_str() {
            if let Some(ref answer) = b.answer {
                result.items = vec![WaitItem::Unblocked {
                    name: b.name.clone(),
                    plan: b.plan.clone(),
                    answer: answer.clone(),
                }];
                result.suppress_all = true;
                return result;
            }
        }
    }

    for b in &blocks {
        if b.answer.is_some() {
            continue;
        }
        result.items.push(WaitItem::Blocked {
            agent: b.agent.clone(),
            name: b.name.clone(),
            plan: b.plan.clone(),
            question: b.question.clone(),
        });
        match &b.plan {
            None => result.suppress_all = true,
            Some(plan_str) => {
                if let Ok(pk) = PlanKey::parse(plan_str) {
                    result.suppressed_plans.insert(pk);
                }
            }
        }
    }

    result
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
            gate,
        } => serde_json::json!({
            "kind": "master",
            "plan": plan.as_str(),
            "plan_path": format!(".clank/plans/{}.md", plan.as_str()),
            "sha": sha.as_str(),
            "next": next,
            "reason": reason,
            "gate": gate.as_str(),
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
        WaitItem::Idle { prompt } => serde_json::json!({
            "kind": "idle",
            "prompt": prompt,
        }),
        WaitItem::AdHocReview { sha, feedback_path } => serde_json::json!({
            "kind": "adhoc_review",
            "sha": sha.as_str(),
            "feedback_path": feedback_path,
        }),
        WaitItem::AdHocRevise { sha } => serde_json::json!({
            "kind": "adhoc_revise",
            "sha": sha.as_str(),
        }),
        WaitItem::PromoteFromQueue { name, priority } => serde_json::json!({
            "kind": "promote_from_queue",
            "name": name,
            "priority": priority,
        }),
        WaitItem::Blocked {
            agent,
            name,
            plan,
            question,
        } => serde_json::json!({
            "kind": "blocked",
            "agent": agent,
            "name": name,
            "plan": plan,
            "question": question,
        }),
        WaitItem::Unblocked { name, plan, answer } => serde_json::json!({
            "kind": "unblocked",
            "name": name,
            "plan": plan,
            "answer": answer,
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
            ..
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
        WaitItem::Idle { prompt } => format!("idle     {prompt}"),
        WaitItem::AdHocReview { sha, .. } => {
            format!("adhoc-review  {}  write feedback", short(sha),)
        }
        WaitItem::AdHocRevise { sha } => format!("adhoc-revise  {}  address changes", short(sha)),
        WaitItem::PromoteFromQueue { name, priority } => {
            format!("promote  {name}  (priority {priority:03}) — run `clank queue promote {name}`")
        }
        WaitItem::Blocked {
            agent,
            name,
            plan,
            question,
        } => {
            let scope = plan.as_deref().unwrap_or("repo");
            format!("blocked  {agent}/{name}  scope={scope}  {question}")
        }
        WaitItem::Unblocked { name, plan, answer } => {
            let scope = plan.as_deref().unwrap_or("repo");
            format!("answer   {name}  scope={scope}  {answer}")
        }
    }
}

/// Watch-time facts captured up-front. `.clank/` is always
/// watched natively. `git_dir` is watched only in native mode;
/// polling mode skips it and relies on the loop's periodic
/// refold tick.
struct WatchContext {
    /// `<repo>/.clank`. Watched recursively in both modes.
    clank_root: PathBuf,
    /// Worktree-specific git dir (`.git/worktrees/<name>/` for
    /// a linked worktree; `<repo>/.git/` for the main worktree).
    /// `Some` in native mode, `None` in polling mode (no watch
    /// attached at all — periodic refold handles git changes).
    git_dir: Option<PathBuf>,
}

impl WatchContext {
    fn resolve(repo: &Path, poll_mode: bool) -> anyhow::Result<Self> {
        let git_dir = if poll_mode {
            None
        } else {
            Some(git_resolve_dir(repo, "--git-dir")?)
        };
        Ok(Self {
            clank_root: repo.join(".clank"),
            git_dir,
        })
    }

    fn attach(&self, watcher: &mut RecommendedWatcher) -> anyhow::Result<()> {
        // `<repo>/.clank` may not exist yet on a brand-new repo.
        // `notify` refuses to watch a missing path, so create it
        // first — Clank manages the directory anyway.
        if let Err(e) = std::fs::create_dir_all(&self.clank_root) {
            anyhow::bail!("ensure `{}` exists: {e}", self.clank_root.display());
        }

        watcher
            .watch(&self.clank_root, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", self.clank_root.display()))?;
        // Gitdir watch is native-mode-only. Polling mode keeps
        // `.clank/` reactive but treats git movement as a
        // periodic-poll concern — empirically required under
        // the Codex tool sandbox where native gitdir events
        // never reach notify.
        if let Some(git_dir) = &self.git_dir {
            watcher
                .watch(git_dir, RecursiveMode::Recursive)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", git_dir.display()))?;
        }
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
            // Any successful event wakes us — including
            // `EventKind::Any` and `EventKind::Other`. Codex's tool
            // sandbox empirically delivers directory-level events
            // without a precise kind classification; filtering on
            // Create/Modify/Remove drops them. The cost of waking
            // on Access events too is one extra refold per touch;
            // the refold reads HEAD and the fold cache cheaply,
            // and the 200ms debounce drain coalesces bursts.
            if res.is_ok() {
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
