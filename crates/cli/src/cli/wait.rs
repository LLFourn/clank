//! `clank wait` — block until the calling agent has wait-surface
//! items to print.
//!
//! Two roles. `--role master` watches for plans where the gate
//! has moved on without master, and additionally receives
//! `Finished` notices when a watched plan transitions into
//! `finished_plans` (the notice is what fires the
//! `plan_finalized` hook). `--role reviewers` watches for plans
//! whose latest reviewable commit needs an opinion from
//! `--author` — and ONLY that: a finish is a notification with no
//! reviewer action, so it never wakes a reviewer
//! (`finish-does-not-wake-reviewers`).
//!
//! `wait` folds the repo, runs `RepoState::derive_status` (which
//! threads `wait::compute_gate` over every plan), filters the
//! result through `RepoState::work_for` for the agent's
//! perspective, then — for master only — concatenates any
//! `detect_finished` notices from a startup snapshot. When the
//! initial fold finds at least one item it prints it and exits;
//! otherwise it watches the filesystem for changes that could
//! plausibly flip the projection and refolds on each debounced
//! event.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use super::{WaitArgs, resolve_repo};
use crate::cli::block::scan_blocks;
use crate::hook_config::{self, HookFiring};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use clank_core::Role;
use clank_core::vocab::HookEvent;
use clank_core::wait::{StartupSnapshot, WaitItem, detect_finished};

/// Exit code returned when `--timeout` elapses without producing
/// any work. The rest of the CLI uses anyhow for normal errors;
/// timeout is the one expected non-zero exit, so we surface it via
/// a sentinel error type instead of `process::exit` so `main` can
/// translate it cleanly.
#[derive(Debug)]
pub struct WaitTimeout;
impl std::fmt::Display for WaitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("wait timed out")
    }
}
impl std::error::Error for WaitTimeout {}

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
            // A broken HEAD tag proactively pulls master back to amend
            // (commit-tag-fixup-is-first-class-state) — previously it
            // yielded no firing, so the bad commit woke the REVIEWER
            // while the master correction sat passive. Keyed to the
            // first real plan the violation implicates (the touched
            // plan, then a named-but-untouched one); the pure
            // unknown-tag case names no real plan, so it has no hook
            // key and stays a self-poll pull.
            WaitItem::FixCommitTag { sha, violation } => violation
                .untagged_touched
                .iter()
                .chain(violation.extra_named.iter())
                .next()
                .map(|plan| HookFiring {
                    event: HookEvent::MasterWork,
                    plan: plan.clone(),
                    sha: sha.clone(),
                    gate: None,
                    next: Some("fix-commit-tag".to_string()),
                }),
            WaitItem::Idle { .. }
            | WaitItem::AdHocReview { .. }
            | WaitItem::AdHocRevise { .. }
            | WaitItem::PromoteFromQueue { .. }
            | WaitItem::Blocked { .. }
            | WaitItem::Unblocked { .. }
            // PR-review items surface via the stop-hook's `clank wait`
            // pull (like ad-hoc/queue items) rather than a dedicated
            // proactive OS hook — HookFiring is plan+sha keyed and PR
            // items are pr+round keyed.
            | WaitItem::PrReviewer { .. }
            | WaitItem::PrMaster { .. } => None,
        })
        .collect()
}

pub async fn run(args: WaitArgs) -> anyhow::Result<()> {
    let timeout = parse_timeout(&args.timeout)?;
    // One env read for the entire process, here at the CLI
    // boundary. Downstream takes a plain `bool`.
    let poll_mode = args.effective_poll();

    let repo = resolve_repo(args.repo.as_deref())?;

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

    // Resolve --role from the roster when omitted: `resolve_role`
    // returns Master for the roster's master and Reviewer for any
    // tier member. Errors if the repo has no master configured.
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

    let config = crate::cli::config::load(&repo);
    let hook_config = config.hooks.clone();
    // Reviewer tiers from the team resolver. wait is a workflow
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

    let snapshot = StartupSnapshot::capture(&initial_state.fold);

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
            // A broken HEAD tag is now a derived, dominating state
            // (commit-tag-fixup-is-first-class-state): derive_status
            // yields the correction and work_for routes master to the
            // FixCommitTag item / withholds reviewer wakes — no bespoke
            // side-check.
            let head = crate::git_io::head_commit_at(&repo, &initial_state);
            let status = initial_state
                .fold
                .derive_status(&reviews, &work_policy, head.as_ref());
            let mut items = status.work_for(&author, role);
            if !br.suppressed_plans.is_empty() {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => {
                        !br.suppressed_plans.contains(plan)
                    }
                    _ => true,
                });
            }
            // Finished is a NOTIFICATION, not work, and only MASTER
            // acts on it (the finalization-lifecycle role + home of
            // the plan_finalized hook). A reviewer has no action on a
            // finish, so injecting it into the work stream would wake
            // an idle reviewer via the `!items.is_empty()` gate below
            // for nothing (`finish-does-not-wake-reviewers`).
            if role == Role::Master {
                items.extend(detect_finished(&snapshot, &initial_state.fold));
            }
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
    if !initial_suppress_all && role == Role::Master && actionable == 0 {
        let queue = match crate::cli::queue::scan_queue_no_dups(&repo) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("wait: {e}");
                Vec::new()
            }
        };
        // Promote the first unsuppressed queued item (+ any Blocked
        // context). If every queued item is suppressed by a pending
        // plan-scoped block, the outcome is Blocked-only — NOT
        // wake-worthy — so we PARK rather than return on a
        // non-actionable human block (wait-ignores-queue-only-blocks).
        // Repo-wide blocks short-circuit upstream (`initial_suppress_all`).
        let items = queue_promote_outcome(&initial_block_items, &initial_suppressed_plans, &queue);
        if wake_worthy(&items) {
            emit(&items, args.json);
            return Ok(());
        }
        hook_config::run_idle_hook(&repo, &hook_config);
    }

    let (tx, rx) = mpsc::channel::<()>();
    // The shared core watcher: `.clank/` gate dirs + gitdir (gitdir
    // skipped in poll mode — the heartbeat tick below covers git then).
    // Same producer `clank status` uses, so the panel and this loop wake
    // on the same changes.
    let _watcher = crate::repo_watch::RepoStateWatcher::attach(&repo, poll_mode, tx)?;

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
                None => return Err(WaitTimeout.into()),
                Some(remaining) => remaining.min(tick),
            },
        };
        let event_received = match rx.recv_timeout(wait) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Could be deadline-expiry or heartbeat-tick. Distinguish.
                if let Some(end) = deadline {
                    if std::time::Instant::now() >= end {
                        return Err(WaitTimeout.into());
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
            // Commit-tag correction is derived (see initial-pass note).
            let head = crate::git_io::head_commit_at(&repo, &state);
            let status = state
                .fold
                .derive_status(&reviews, &work_policy, head.as_ref());
            let mut items = status.work_for(&author, role);
            if !br.suppressed_plans.is_empty() {
                items.retain(|item| match item {
                    WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => {
                        !br.suppressed_plans.contains(plan)
                    }
                    _ => true,
                });
            }
            // Master-only Finished notice (watch loop); see the
            // initial-pass rationale above
            // (`finish-does-not-wake-reviewers`).
            if role == Role::Master {
                items.extend(detect_finished(&snapshot, &state.fold));
            }
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
            if role == Role::Master && actionable_in_loop == 0 {
                let queue = match crate::cli::queue::scan_queue_no_dups(&repo) {
                    Ok(q) => q,
                    Err(e) => {
                        eprintln!("wait: {e}");
                        Vec::new()
                    }
                };
                // Same single-sourced rule as the initial pass: promote
                // the first unsuppressed queued item (+ Blocked context),
                // but PARK on Blocked-only (all queued items suppressed)
                // rather than waking on a non-actionable human block
                // (wait-ignores-queue-only-blocks).
                let items = queue_promote_outcome(&br.items, &br.suppressed_plans, &queue);
                if wake_worthy(&items) {
                    emit(&items, args.json);
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

/// A pending human `Block` is attachable CONTEXT — co-surfaced beside
/// real work — but NEVER wake-worthy on its own. A wait result is
/// wake-worthy iff it carries at least one non-`Blocked` item
/// (actionable Master/Reviewer work, `PromoteFromQueue`, `Unblocked`,
/// `Finished`, a commit-tag fix, …). Returning on `Blocked`-alone wakes
/// an agent that has nothing to do and churns the stop-hook / wait loop
/// on a non-actionable condition (wait-ignores-queue-only-blocks) — the
/// caller PARKS instead. Single source so neither the initial pass nor
/// the watch-loop pass can re-introduce the emit-on-Blocked bug.
fn wake_worthy(items: &[WaitItem]) -> bool {
    items.iter().any(|i| !matches!(i, WaitItem::Blocked { .. }))
}

/// The master-with-no-actionable-active-work outcome: any pending-Blocked
/// context, plus a `PromoteFromQueue` for the first queue item NOT
/// suppressed by a plan-scoped block (a block on the top queued item
/// must not hide a lower-priority unblocked one). If EVERY queued item
/// is suppressed — or the queue is empty — the result is Blocked-only
/// (or empty), which [`wake_worthy`] rejects, so the caller parks.
/// Shared by both the initial pass and the watch-loop pass.
fn queue_promote_outcome(
    block_items: &[WaitItem],
    suppressed_plans: &std::collections::BTreeSet<PlanKey>,
    queue: &[crate::cli::queue::QueueEntry],
) -> Vec<WaitItem> {
    let mut items = block_items.to_vec();
    if let Some(first) = queue
        .iter()
        .find(|q| !suppressed_plans.iter().any(|k| k.as_str() == q.name))
    {
        items.push(WaitItem::PromoteFromQueue {
            name: first.name.clone(),
            priority: first.priority,
        });
    }
    items
}

/// `clank wait --json` envelope. The stop-hook parses this; the
/// contract is keys + values (not key order). Typed in place of the
/// former ad-hoc `json!` (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct WaitEnvelope<'a> {
    items: Vec<WaitJsonItem<'a>>,
}

fn emit(items: &[WaitItem], json: bool) {
    if json {
        let envelope = WaitEnvelope {
            items: items.iter().map(render_json).collect(),
        };
        println!(
            "{}",
            serde_json::to_string(&envelope).expect("serialize wait envelope")
        );
    } else {
        for item in items {
            println!("{}", render_human(item));
        }
    }
}

/// 12 hex chars (48 bits), not git's display default of 7: agents
/// COMPOSE `feedback write --commit <sha>` from this hint, and
/// `CommitRef::resolve_against` errors on an ambiguous prefix — at
/// 12 chars a collision is effectively impossible, so the hint
/// always resolves (`wfw-output-is-a-minimal-hint`, ruthless
/// 201e498 concern 2). The full sha stays in the json field.
fn short(sha: &CommitSha) -> &str {
    &sha.as_str()[..sha.as_str().len().min(12)]
}

/// Typed rows for `clank wait --json` (typed-json-not-json-macro,
/// replacing per-variant `json!`). `#[serde(tag = "kind")]` emits the
/// discriminant alongside the fields. Borrows from the source
/// `WaitItem` (no clones); the computed `plan_path` is the one owned
/// field. Same keys + values as the prior shape — key order is
/// irrelevant (the stop-hook parses the JSON).
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
enum WaitJsonItem<'a> {
    #[serde(rename = "master")]
    Master {
        plan: &'a str,
        plan_path: String,
        sha: &'a str,
        next: &'a clank_core::wait::MasterNext,
        reason: &'a clank_core::vocab::WaitingReason,
        gate: &'a str,
    },
    #[serde(rename = "reviewer")]
    Reviewer {
        plan: &'a str,
        plan_path: String,
        sha: &'a str,
        feedback_path: &'a str,
    },
    #[serde(rename = "finished")]
    Finished {
        plan: &'a str,
        plan_path: String,
        finalized_at: &'a str,
    },
    #[serde(rename = "idle")]
    Idle { prompt: &'a str },
    #[serde(rename = "adhoc_review")]
    AdHocReview {
        sha: &'a str,
        feedback_path: &'a str,
    },
    #[serde(rename = "adhoc_revise")]
    AdHocRevise { sha: &'a str },
    #[serde(rename = "fix_commit_tag")]
    FixCommitTag {
        sha: &'a str,
        unknown: &'a [String],
        untagged_touched: Vec<&'a str>,
        extra_named: Vec<&'a str>,
    },
    #[serde(rename = "promote_from_queue")]
    PromoteFromQueue { name: &'a str, priority: u16 },
    #[serde(rename = "blocked")]
    Blocked {
        agent: &'a str,
        name: &'a str,
        plan: Option<&'a str>,
        question: &'a str,
    },
    #[serde(rename = "unblocked")]
    Unblocked {
        name: &'a str,
        plan: Option<&'a str>,
        answer: &'a str,
    },
    #[serde(rename = "pr_reviewer")]
    PrReviewer { pr: u32, round: u64 },
    #[serde(rename = "pr_master")]
    PrMaster {
        pr: u32,
        round: u64,
        next: &'a clank_core::wait::PrMasterNext,
    },
}

fn plan_path(plan: &PlanKey) -> String {
    crate::init_facts::plan_md_rel(plan.as_str())
}

fn render_json(item: &WaitItem) -> WaitJsonItem<'_> {
    match item {
        WaitItem::Master {
            plan,
            sha,
            next,
            reason,
            gate,
        } => WaitJsonItem::Master {
            plan: plan.as_str(),
            plan_path: plan_path(plan),
            sha: sha.as_str(),
            next,
            reason,
            gate: gate.as_str(),
        },
        WaitItem::Reviewer {
            plan,
            sha,
            feedback_path,
        } => WaitJsonItem::Reviewer {
            plan: plan.as_str(),
            plan_path: plan_path(plan),
            sha: sha.as_str(),
            feedback_path,
        },
        WaitItem::Finished { plan, finalized_at } => WaitJsonItem::Finished {
            plan: plan.as_str(),
            plan_path: plan_path(plan),
            finalized_at: finalized_at.as_str(),
        },
        WaitItem::Idle { prompt } => WaitJsonItem::Idle { prompt },
        WaitItem::AdHocReview { sha, feedback_path } => WaitJsonItem::AdHocReview {
            sha: sha.as_str(),
            feedback_path,
        },
        WaitItem::AdHocRevise { sha } => WaitJsonItem::AdHocRevise { sha: sha.as_str() },
        WaitItem::FixCommitTag { sha, violation } => WaitJsonItem::FixCommitTag {
            sha: sha.as_str(),
            unknown: &violation.unknown,
            untagged_touched: violation
                .untagged_touched
                .iter()
                .map(|p| p.as_str())
                .collect(),
            extra_named: violation.extra_named.iter().map(|p| p.as_str()).collect(),
        },
        WaitItem::PromoteFromQueue { name, priority } => WaitJsonItem::PromoteFromQueue {
            name,
            priority: *priority,
        },
        WaitItem::Blocked {
            agent,
            name,
            plan,
            question,
        } => WaitJsonItem::Blocked {
            agent,
            name,
            plan: plan.as_deref(),
            question,
        },
        WaitItem::Unblocked { name, plan, answer } => WaitJsonItem::Unblocked {
            name,
            plan: plan.as_deref(),
            answer,
        },
        WaitItem::PrReviewer { pr, round } => WaitJsonItem::PrReviewer {
            pr: *pr,
            round: *round,
        },
        WaitItem::PrMaster { pr, round, next } => WaitJsonItem::PrMaster {
            pr: *pr,
            round: *round,
            next,
        },
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
        WaitItem::FixCommitTag { sha, violation } => {
            let mut parts: Vec<String> = Vec::new();
            if !violation.untagged_touched.is_empty() {
                let names = violation
                    .untagged_touched
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                parts.push(format!("touched but not tagged: {names}"));
            }
            if !violation.extra_named.is_empty() {
                let names = violation
                    .extra_named
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                parts.push(format!("tagged but not touched: {names}"));
            }
            if !violation.unknown.is_empty() {
                parts.push(format!(
                    "names no active plan: {}",
                    violation.unknown.join(",")
                ));
            }
            format!(
                "fix-commit-tag  {}  {} — make the commit's [tag]s EQUAL the active \
                 plans whose files it touches: re-tag to the right plan(s), or REMOVE \
                 the tag if the commit is ad-hoc (not plan work)",
                short(sha),
                parts.join("; ")
            )
        }
        WaitItem::PromoteFromQueue { name, priority } => {
            format!("promote  {name}  (priority {priority:03})")
        }
        WaitItem::Blocked {
            agent,
            name,
            plan,
            // The question is for the HUMAN (who reads the block
            // elsewhere); the woken agent only needs "not your
            // move". It stays a json field.
            question: _,
        } => {
            let scope = plan.as_deref().unwrap_or("repo");
            format!("blocked  {agent}/{name}  scope={scope}  (awaiting human)")
        }
        WaitItem::Unblocked { name, plan, answer } => {
            let scope = plan.as_deref().unwrap_or("repo");
            format!("answer   {name}  scope={scope}  {answer}")
        }
        WaitItem::PrReviewer { pr, round } => {
            format!("pr-review  #{pr}  round {round}  review the pending comments")
        }
        WaitItem::PrMaster { pr, round, next } => {
            format!("pr-review  #{pr}  round {round}  master: {next:?}")
        }
    }
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

    // ── wait-ignores-queue-only-blocks ─────────────────────────
    // A pending human Block is attachable context, never wake-worthy
    // alone: Blocked-only parks (times out), but a block must not hide
    // a lower-priority unblocked queue item.

    fn blocked(name: &str, plan: Option<&str>) -> WaitItem {
        WaitItem::Blocked {
            agent: "claude".into(),
            name: name.into(),
            plan: plan.map(Into::into),
            question: "?".into(),
        }
    }

    fn qentry(name: &str, priority: u16) -> crate::cli::queue::QueueEntry {
        crate::cli::queue::QueueEntry {
            priority,
            name: name.into(),
            path: std::path::PathBuf::new(),
        }
    }

    fn suppressed(names: &[&str]) -> std::collections::BTreeSet<PlanKey> {
        names.iter().map(|n| PlanKey::parse(n).unwrap()).collect()
    }

    #[test]
    fn wake_worthy_rejects_blocked_alone_and_empty() {
        assert!(!wake_worthy(&[]), "empty is not wake-worthy");
        assert!(
            !wake_worthy(&[blocked("d", Some("p"))]),
            "a pending Block alone must not wake"
        );
        // Anything non-Blocked alongside makes it wake-worthy.
        assert!(wake_worthy(&[
            blocked("d", Some("p")),
            WaitItem::PromoteFromQueue {
                name: "x".into(),
                priority: 1
            },
        ]));
        assert!(wake_worthy(&[WaitItem::Unblocked {
            name: "d".into(),
            plan: Some("p".into()),
            answer: "go".into(),
        }]));
    }

    #[test]
    fn queue_only_block_parks_does_not_wake() {
        // The Frostsnap repro: one queued plan (`simctl-up`), one
        // pending block scoped to it → every queued item suppressed →
        // Blocked-only → caller parks (wait times out).
        let items = queue_promote_outcome(
            &[blocked("simctl-up-design-decisions", Some("simctl-up"))],
            &suppressed(&["simctl-up"]),
            &[qentry("simctl-up", 500)],
        );
        assert!(
            !wake_worthy(&items),
            "queue-only pending block must park, got {items:?}"
        );
    }

    #[test]
    fn block_on_top_queue_item_still_promotes_lower_unblocked() {
        // First queued plan blocked, second unblocked → promote the
        // second (a block must not hide lower-priority work).
        let items = queue_promote_outcome(
            &[blocked("d", Some("blocked-plan"))],
            &suppressed(&["blocked-plan"]),
            &[qentry("blocked-plan", 400), qentry("free-plan", 500)],
        );
        assert!(wake_worthy(&items));
        assert!(
            items.iter().any(|i| matches!(
                i,
                WaitItem::PromoteFromQueue { name, .. } if name == "free-plan"
            )),
            "lower-priority unblocked item must promote, got {items:?}"
        );
    }

    #[test]
    fn empty_queue_with_no_blocks_parks() {
        let items = queue_promote_outcome(&[], &suppressed(&[]), &[]);
        assert!(!wake_worthy(&items));
    }

    // ── commit-tag-fixup-is-first-class-state ──────────────────

    #[test]
    fn fix_commit_tag_fires_master_hook_keyed_to_touched_plan() {
        // Previously FixCommitTag yielded no firing — the bad commit
        // woke the REVIEWER while the master correction sat passive.
        // Now it proactively pulls master back, keyed to the touched
        // plan.
        let item = WaitItem::FixCommitTag {
            sha: sha("aaaa"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["plan".into()],
                untagged_touched: vec![PlanKey::parse("real").unwrap()],
                extra_named: vec![],
            },
        };
        let firings = firings_from_items(std::slice::from_ref(&item));
        assert_eq!(firings.len(), 1, "a proactive master firing");
        assert_eq!(firings[0].event, HookEvent::MasterWork);
        assert_eq!(firings[0].plan.as_str(), "real");
        assert_eq!(firings[0].next.as_deref(), Some("fix-commit-tag"));
    }

    #[test]
    fn fix_commit_tag_unknown_only_has_no_hook_key() {
        // T = ∅ unknown-tag case names no real plan → no HookFiring
        // (nothing to key it to), as before.
        let item = WaitItem::FixCommitTag {
            sha: sha("aaaa"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["ghost".into()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        };
        assert!(firings_from_items(std::slice::from_ref(&item)).is_empty());
    }

    #[test]
    fn fix_commit_tag_human_line_names_all_three_kinds() {
        let item = WaitItem::FixCommitTag {
            sha: sha("aaaa"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["plan".into()],
                untagged_touched: vec![PlanKey::parse("real").unwrap()],
                extra_named: vec![PlanKey::parse("extra").unwrap()],
            },
        };
        let line = render_human(&item);
        assert!(line.contains("fix-commit-tag"));
        assert!(line.contains("real"), "names the touched-but-untagged plan");
        assert!(
            line.contains("extra"),
            "names the tagged-but-untouched plan"
        );
        assert!(line.contains("plan"), "names the unknown tag");
    }

    #[test]
    fn fix_commit_tag_hint_offers_dropping_the_tag() {
        // The unknown-tag case (committed `[plan]` for a plan that doesn't
        // exist) is usually an ad-hoc commit that shouldn't be tagged at
        // all — the hint must surface REMOVING the tag as an explicit
        // option, not bury it (commit-tag-guidance).
        let item = WaitItem::FixCommitTag {
            sha: sha("bbbb"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["nope".into()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        };
        let line = render_human(&item).to_lowercase();
        assert!(line.contains("remove"), "offers removing the tag: {line}");
        assert!(line.contains("ad-hoc"), "names the ad-hoc case: {line}");
    }

    // ── minimal-hint rendering (wfw-output-is-a-minimal-hint) ──

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }

    #[test]
    fn hint_sha_is_twelve_chars() {
        // Agents compose `feedback write --commit <sha>` from the
        // hint; 12 hex chars can't realistically be ambiguous
        // (ruthless 201e498 concern 2). Full sha stays in json.
        assert_eq!(short(&sha("abc")).len(), 12);
    }

    #[test]
    fn human_lines_carry_no_tutorial() {
        let promote = render_human(&WaitItem::PromoteFromQueue {
            name: "some-plan".into(),
            priority: 300,
        });
        assert_eq!(promote, "promote  some-plan  (priority 300)");

        let blocked = render_human(&WaitItem::Blocked {
            agent: "claude".into(),
            name: "q".into(),
            plan: Some("foo".into()),
            question: "should we?".into(),
        });
        assert_eq!(blocked, "blocked  claude/q  scope=foo  (awaiting human)");
        assert!(
            !blocked.contains("should we?"),
            "block question is for the human, not the woken agent"
        );
    }

    #[test]
    fn json_keeps_structured_fields_including_full_sha_and_question() {
        // Parity = same DATA, different format (ruthless concern
        // 3): json keeps the structured fields a consumer needs —
        // the FULL sha and the block question — while tutorial
        // STRINGS exist in neither view.
        let j = serde_json::to_value(render_json(&WaitItem::Reviewer {
            plan: PlanKey::parse("foo").unwrap(),
            sha: sha("abc"),
            feedback_path: ".clank/agents/x/feedback/abc.md".into(),
        }))
        .unwrap();
        assert_eq!(j["sha"].as_str().unwrap().len(), 40, "full sha in json");
        let j = serde_json::to_value(render_json(&WaitItem::Blocked {
            agent: "claude".into(),
            name: "q".into(),
            plan: Some("foo".into()),
            question: "should we?".into(),
        }))
        .unwrap();
        assert_eq!(j["question"], "should we?", "question stays a json field");
    }

    // ── typed-json-not-json-macro: wait --json wire contract ──────
    //
    // Each typed `WaitJsonItem` must serialize to the same keys+values
    // the old per-variant `json!` produced — the `clank wait --json`
    // contract the stop-hook parses. `json!` here expresses the
    // EXPECTED value (the ban is on production output code); key order
    // is irrelevant — `to_value` equality is order-independent.
    #[test]
    fn wait_json_items_serialize_to_the_stable_shape() {
        use clank_core::vocab::WaitingReason;
        use clank_core::wait::{MasterNext, PrMasterNext};

        let cases = [
            (
                serde_json::to_value(render_json(&WaitItem::Master {
                    plan: PlanKey::parse("foo").unwrap(),
                    sha: sha("abc"),
                    next: MasterNext::Revise,
                    reason: WaitingReason::AddressCommitChanges,
                    gate: clank_core::vocab::CommitGateState::ChangesRequested,
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "master",
                    "plan": "foo",
                    "plan_path": ".clank/plans/foo.md",
                    "sha": sha("abc").as_str(),
                    "next": MasterNext::Revise,
                    "reason": WaitingReason::AddressCommitChanges,
                    "gate": "changes_requested",
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::Reviewer {
                    plan: PlanKey::parse("foo").unwrap(),
                    sha: sha("abc"),
                    feedback_path: ".clank/agents/x/feedback/abc.md".into(),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "reviewer",
                    "plan": "foo",
                    "plan_path": ".clank/plans/foo.md",
                    "sha": sha("abc").as_str(),
                    "feedback_path": ".clank/agents/x/feedback/abc.md",
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::Finished {
                    plan: PlanKey::parse("foo").unwrap(),
                    finalized_at: sha("abc"),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "finished",
                    "plan": "foo",
                    "plan_path": ".clank/plans/foo.md",
                    "finalized_at": sha("abc").as_str(),
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::Idle {
                    prompt: "go".into(),
                }))
                .unwrap(),
                serde_json::json!({ "kind": "idle", "prompt": "go" }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::AdHocReview {
                    sha: sha("abc"),
                    feedback_path: "p".into(),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "adhoc_review",
                    "sha": sha("abc").as_str(),
                    "feedback_path": "p",
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::AdHocRevise { sha: sha("abc") }))
                    .unwrap(),
                serde_json::json!({ "kind": "adhoc_revise", "sha": sha("abc").as_str() }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::FixCommitTag {
                    sha: sha("abc"),
                    violation: clank_core::wait::HeadTagViolation {
                        unknown: vec!["ghost".into()],
                        untagged_touched: vec![PlanKey::parse("real").unwrap()],
                        extra_named: vec![PlanKey::parse("extra").unwrap()],
                    },
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "fix_commit_tag",
                    "sha": sha("abc").as_str(),
                    "unknown": ["ghost"],
                    "untagged_touched": ["real"],
                    "extra_named": ["extra"],
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::PromoteFromQueue {
                    name: "some-plan".into(),
                    priority: 300,
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "promote_from_queue",
                    "name": "some-plan",
                    "priority": 300,
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::Blocked {
                    agent: "claude".into(),
                    name: "q".into(),
                    plan: Some("foo".into()),
                    question: "should we?".into(),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "blocked",
                    "agent": "claude",
                    "name": "q",
                    "plan": "foo",
                    "question": "should we?",
                }),
            ),
            (
                // `plan: None` must serialize to `null`, matching the
                // old `&Option<String>` value.
                serde_json::to_value(render_json(&WaitItem::Unblocked {
                    name: "q".into(),
                    plan: None,
                    answer: "yes".into(),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "unblocked",
                    "name": "q",
                    "plan": null,
                    "answer": "yes",
                }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::PrReviewer { pr: 7, round: 2 }))
                    .unwrap(),
                serde_json::json!({ "kind": "pr_reviewer", "pr": 7, "round": 2 }),
            ),
            (
                serde_json::to_value(render_json(&WaitItem::PrMaster {
                    pr: 7,
                    round: 2,
                    next: PrMasterNext::Submit,
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "pr_master",
                    "pr": 7,
                    "round": 2,
                    "next": PrMasterNext::Submit,
                }),
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn wait_json_envelope_wraps_items_under_items_key() {
        let item = WaitItem::Idle {
            prompt: "go".into(),
        };
        let envelope = WaitEnvelope {
            items: vec![render_json(&item)],
        };
        let got = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            got,
            serde_json::json!({ "items": [{ "kind": "idle", "prompt": "go" }] }),
        );
    }
}
