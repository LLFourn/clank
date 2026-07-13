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
use std::process::Stdio;
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
            // Multiple open plans proactively pull master back, like
            // the tag correction: keyed to the plan in progress (the
            // one whose flow the overlap interrupts).
            WaitItem::MultiplePlansOpen {
                in_progress, sha, ..
            } => Some(HookFiring {
                event: HookEvent::MasterWork,
                plan: in_progress.clone(),
                sha: sha.clone(),
                gate: None,
                next: Some("multiple-plans-open".to_string()),
            }),
            WaitItem::Idle { .. }
            | WaitItem::AdHocReview { .. }
            | WaitItem::AdHocRevise { .. }
            | WaitItem::PromoteFromQueue { .. }
            | WaitItem::Blocked { .. }
            | WaitItem::Unblocked { .. }
            // Observer items never fire hooks: the observer path is
            // side-effect-free by contract (wait-for-observer-mode)
            // and never calls firings anyway.
            | WaitItem::ForCommit { .. }
            | WaitItem::ForFinished { .. }
            | WaitItem::ForBlocked { .. }
            // External wake sources fire no lifecycle hooks (MVP —
            // extra-wait-events).
            | WaitItem::GithubEvent { .. }
            | WaitItem::CommandEvent { .. }
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

    // Observer mode (`--for`, wait-for-observer-mode): a pure
    // cross-repo observer — no identity, no role, no work
    // projection, no lifecycle hooks. Branches BEFORE identity
    // resolution: the observed repo has no session binding for the
    // caller, and --author/--role are deliberately ignored.
    if let Some(event) = args.r#for {
        return run_observer(&repo, event, timeout, poll_mode, args.no_cache, args.json).await;
    }

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

    let explicit_role: Option<Role> = args.role.map(Into::into);
    // Extra wake sources: the agent config's `wait_events` plus any
    // `--event` items (config first, CLI appended). Parse errors are
    // arm-time hard errors naming the offending input.
    let event_sources = resolve_wait_event_sources(&repo, &author, &args.events)?;
    // Arm-time derivation errors stay HARD (a wait that can never
    // project is a bug to surface); the loop re-derives per wake and
    // is fail-soft there (wait-reloads-config-per-refold).
    let mut inputs = derive_wait_inputs(&repo, &author, explicit_role)?;

    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let initial_state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;

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
            let status =
                initial_state
                    .fold
                    .derive_status(&reviews, &inputs.work_policy, head.as_ref());
            let mut items = status.work_for(&author, inputs.role);
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
            if inputs.role == Role::Master {
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
                // `--peek` is a pure probe: report work-presence, fire NO
                // lifecycle hooks (the Stop hook may peek on every stop).
                if !args.peek {
                    for firing in &firings_from_items(&items) {
                        hook_config::run_hook(&repo, &inputs.hook_config, firing);
                    }
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
    if !initial_suppress_all && inputs.role == Role::Master && actionable == 0 {
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
        // The idle hook is a side effect; `--peek` must not fire it.
        if !args.peek {
            hook_config::run_idle_hook(&repo, &inputs.hook_config);
        }
    }

    // `--peek`: the initial pass found no actionable work → report empty
    // and return immediately, never entering the (blocking) watcher loop.
    if args.peek {
        emit(&[], args.json);
        return Ok(());
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

    // ONE supervised async loop (extra-wait-events, codex cdcf111):
    // the watcher's sync channel is bridged into a tokio::select over
    // repo wakes, external source items, the heartbeat tick, and the
    // deadline. Sources spawn only when the wait actually PARKS (the
    // initial pass above returns without them) and are cancelled AND
    // joined on every return path below.
    let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            if wake_tx.send(()).is_err() {
                break;
            }
        }
    });
    let mut sources = EventSources::spawn(&event_sources);
    // Once the source channel closes (every source task finished, or
    // there were none), its `recv()` is permanently ready with `None`
    // — a guard disables that select branch so it can't hot-loop the
    // wait (codex 1bbb61d).
    let mut sources_open = true;

    let outcome: anyhow::Result<Vec<WaitItem>> = async {
        loop {
            // External items drained this beat (merged into whatever
            // the repo projection yields — one emitted result).
            let mut external: Vec<WaitItem> = Vec::new();
            tokio::select! {
                _ = tokio::time::sleep(tick) => {}
                w = wake_rx.recv() => {
                    if w.is_none() {
                        anyhow::bail!("filesystem watcher disconnected");
                    }
                    // Debounce: let a burst settle, then drain it —
                    // one logical change, one refold.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    while wake_rx.try_recv().is_ok() {}
                }
                item = sources.rx.recv(), if sources_open => {
                    match item {
                        Some(item) => external.push(item),
                        None => sources_open = false,
                    }
                }
                _ = async {
                    match deadline {
                        Some(end) => tokio::time::sleep_until(end.into()).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    return Err(WaitTimeout.into());
                }
            }
            // Drain items queued before/at this beat (whichever branch
            // woke us) so a repo-branch win doesn't strand a ready
            // external item (codex 1bbb61d).
            drain_external(&mut sources.rx, &mut sources_open, &mut external);

            // Re-derive the projection inputs EVERY wake: config.json is a
            // wake dir, and a parked wait projecting with the team captured
            // at arm time computes stale work after any roster change
            // (wait-reloads-config-per-refold). Fail-soft mid-loop: a
            // transient read failure keeps the last-known-good inputs and
            // retries next wake — it must not kill a parked wait.
            if let Ok(fresh) = derive_wait_inputs(&repo, &author, explicit_role) {
                inputs = fresh;
            }

            // Compute this beat's REPO-side result (None = park). Any
            // return then funnels through the single boundary below so a
            // final drain catches items that arrived DURING the refold
            // (`await`) — none is ever dropped (codex 907acd5).
            let br = check_blocks(&repo, &author);
            let has_answer = br
                .items
                .iter()
                .any(|i| matches!(i, WaitItem::Unblocked { .. }));
            let repo_items: Option<Vec<WaitItem>> = if has_answer {
                Some(br.items.clone())
            } else if br.suppress_all {
                None
            } else {
                let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display())
                    })?;
                let reviews =
                    crate::fs_plan_state_lookup::FsPlanStateLookup::new(&repo, state.head.as_ref());
                // Commit-tag correction is derived (see initial-pass note).
                let head = crate::git_io::head_commit_at(&repo, &state);
                let status = state
                    .fold
                    .derive_status(&reviews, &inputs.work_policy, head.as_ref());
                let mut items = status.work_for(&author, inputs.role);
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
                if inputs.role == Role::Master {
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
                        hook_config::run_hook(&repo, &inputs.hook_config, firing);
                    }
                    Some(items)
                } else {
                    // Same actionable-count gate as the initial pass — see
                    // the comment above the initial gate. Watch-loop variant.
                    let actionable_in_loop = state
                        .fold
                        .plans
                        .iter()
                        .filter(|(k, _)| !br.suppressed_plans.contains(k))
                        .count();
                    if inputs.role == Role::Master && actionable_in_loop == 0 {
                        let queue = match crate::cli::queue::scan_queue_no_dups(&repo) {
                            Ok(q) => q,
                            Err(e) => {
                                eprintln!("wait: {e}");
                                Vec::new()
                            }
                        };
                        // Same single-sourced rule as the initial pass:
                        // promote the first unsuppressed queued item (+
                        // Blocked context), but PARK on Blocked-only
                        // (wait-ignores-queue-only-blocks).
                        let items = queue_promote_outcome(&br.items, &br.suppressed_plans, &queue);
                        wake_worthy(&items).then_some(items)
                    } else {
                        None
                    }
                }
            };

            // THE single return boundary: one last drain catches items
            // produced during the refold, then repo + external merge into
            // ONE result. Park only when BOTH are empty (codex 907acd5).
            drain_external(&mut sources.rx, &mut sources_open, &mut external);
            if let Some(items) = combine_beat_result(repo_items, external) {
                return Ok(items);
            }
        }
    }
    .await;

    // Every return path — items, timeout, error — cancels AND joins
    // the source tasks; command children die by process-group kill
    // (extra-wait-events).
    sources.shutdown().await;
    let items = outcome?;
    emit(&items, args.json);
    Ok(())
}

/// One watcher-loop beat, shared by the work loop and the observer
/// loop: block on the next FS event or heartbeat tick, enforce the
/// deadline, and debounce event bursts (one logical change → one
/// refold). `Ok(())` per beat; `Err(WaitTimeout)` on deadline expiry.
fn next_beat(
    rx: &mpsc::Receiver<()>,
    deadline: Option<std::time::Instant>,
    tick: Duration,
) -> anyhow::Result<()> {
    let wait = match deadline {
        None => tick,
        Some(end) => match end.checked_duration_since(std::time::Instant::now()) {
            None => return Err(WaitTimeout.into()),
            Some(remaining) => remaining.min(tick),
        },
    };
    match rx.recv_timeout(wait) {
        Ok(()) => {
            // Debounce: drain bursts. Only meaningful on the FS-event
            // branch; the heartbeat tick has nothing to drain.
            while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
            Ok(())
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Could be deadline-expiry or heartbeat-tick. Distinguish.
            if let Some(end) = deadline
                && std::time::Instant::now() >= end
            {
                return Err(WaitTimeout.into());
            }
            Ok(())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("filesystem watcher disconnected")
        }
    }
}

/// The `--for` observer loop (wait-for-observer-mode): block until a
/// repo event lands, as a pure observer — no identity, no role, no
/// work projection, and (like `--peek`) NO lifecycle side effects.
/// The baseline is captured synchronously before the watcher loop, so
/// only events strictly after startup fire; anything landing between
/// capture and the watcher attaching is caught by the first heartbeat
/// refold.
async fn run_observer(
    repo: &Path,
    event: crate::cli::WaitFor,
    timeout: Option<Duration>,
    poll_mode: bool,
    no_cache: bool,
    json: bool,
) -> anyhow::Result<()> {
    let policy = if no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    let baseline = ObserverBaseline::capture(repo, &state);

    let (tx, rx) = mpsc::channel::<()>();
    let _watcher = crate::repo_watch::RepoStateWatcher::attach(repo, poll_mode, tx)?;
    let tick: Duration = if poll_mode {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(1500)
    };
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    loop {
        next_beat(&rx, deadline, tick)?;
        let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
        let items = observe_events(&baseline, repo, &state, event);
        if !items.is_empty() {
            emit(&items, json);
            return Ok(());
        }
    }
}

/// The observer's delta anchor: repo facts at startup that `--for`
/// events are measured against. Deliberately NOT
/// [`StartupSnapshot`]: that one only watches plans active at capture
/// time, but an observer must fire for a plan that is introduced AND
/// finalized after it starts.
struct ObserverBaseline {
    head: Option<CommitSha>,
    /// Every (plan, finalize sha) pair already in history.
    finalized: std::collections::BTreeSet<(PlanKey, CommitSha)>,
    /// Unanswered (agent, name) blocks already pending.
    unanswered_blocks: std::collections::BTreeSet<(String, String)>,
}

impl ObserverBaseline {
    fn capture(repo: &Path, state: &crate::repo_state::RepoState) -> Self {
        let finalized = state
            .fold
            .finished_plans
            .iter()
            .map(|fp| (fp.plan.clone(), fp.finalized_at.clone()))
            .collect();
        let unanswered_blocks = scan_blocks(repo)
            .into_iter()
            .filter(|b| b.answer.is_none())
            .map(|b| (b.agent, b.name))
            .collect();
        Self {
            head: state.head.clone(),
            finalized,
            unanswered_blocks,
        }
    }
}

/// Project one refold round against the baseline: the observer items
/// that have occurred since. Empty = keep blocking.
fn observe_events(
    baseline: &ObserverBaseline,
    repo: &Path,
    state: &crate::repo_state::RepoState,
    event: crate::cli::WaitFor,
) -> Vec<WaitItem> {
    use crate::cli::WaitFor;
    let mut items = Vec::new();
    match event {
        WaitFor::Commit => {
            if let Some(h) = state.head.as_ref()
                && baseline.head.as_ref() != Some(h)
            {
                let subject = crate::git_io::commit_subject_at(repo, h).unwrap_or_default();
                items.push(WaitItem::ForCommit {
                    sha: h.clone(),
                    subject,
                });
            }
        }
        WaitFor::Finished | WaitFor::Stopped => {
            for fp in &state.fold.finished_plans {
                let key = (fp.plan.clone(), fp.finalized_at.clone());
                if !baseline.finalized.contains(&key) {
                    items.push(WaitItem::ForFinished {
                        plan: fp.plan.clone(),
                        sha: fp.finalized_at.clone(),
                    });
                }
            }
            if event == WaitFor::Stopped {
                for b in scan_blocks(repo) {
                    if b.answer.is_none()
                        && !baseline
                            .unanswered_blocks
                            .contains(&(b.agent.clone(), b.name.clone()))
                    {
                        items.push(WaitItem::ForBlocked {
                            agent: b.agent,
                            name: b.name,
                        });
                    }
                }
            }
        }
    }
    items
}

/// The projection inputs derived from repo config — ONE derivation
/// shared by the arm-time pass and every watch-loop wake, so the two
/// can't drift (wait-reloads-config-per-refold). Author identity is
/// deliberately not here (fixed at arm time by design); an explicit
/// `--role` is an INPUT and is never re-resolved away.
struct WaitInputs {
    role: Role,
    work_policy: clank_core::wait::WorkPolicy,
    hook_config: std::collections::BTreeMap<HookEvent, Option<String>>,
}

fn derive_wait_inputs(
    repo: &Path,
    author: &AgentLabel,
    explicit_role: Option<Role>,
) -> anyhow::Result<WaitInputs> {
    // Resolve --role from the roster when omitted: `resolve_role`
    // returns Master for the roster's master and Reviewer for any
    // tier member. Errors if the repo has no master configured.
    let role = match explicit_role {
        Some(r) => r,
        None => crate::agent_store::resolve_role(repo, author)?,
    };
    let config = crate::cli::config::load(repo);
    // Reviewer tiers from the team resolver. wait is a workflow
    // command, so it hard-errors when no team is configured
    // (`teams-based-agent-registration` render-vs-workflow boundary).
    let tiers = crate::agent_store::load_reviewer_tiers(repo)?;
    Ok(WaitInputs {
        role,
        work_policy: clank_core::wait::WorkPolicy {
            plan_feedback: config.review.plan_feedback,
            adhoc_feedback: config.review.adhoc_feedback,
            commit_reviewers: tiers.commit,
            plan_reviewers: tiers.plan,
            final_reviewers: tiers.final_,
        },
        hook_config: config.hooks,
    })
}

/// The merged extra-wake-source list (extra-wait-events): the agent
/// config's `wait_events` first, then every CLI `--event` item — the
/// SAME JSON shape, one grammar. Missing config → config sources are
/// simply none (an unbound label can still `--event`).
pub fn resolve_wait_event_sources(
    repo: &Path,
    author: &AgentLabel,
    cli_items: &[String],
) -> anyhow::Result<Vec<clank_core::agent_config::WaitEventSource>> {
    let mut sources = crate::agent_store::load_agent_config(repo, author)?
        .map(|c| c.wait_events)
        .unwrap_or_default();
    for raw in cli_items {
        let item: clank_core::agent_config::WaitEventSource = serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("invalid --event `{raw}`: {e}"))?;
        sources.push(item);
    }
    // Validate durations at ARM time so a bad `poll_interval` fails
    // loud here, not silently substituting the default deep in the
    // poll loop (codex ef1861a).
    for source in &sources {
        if let clank_core::agent_config::WaitEventSource::Github(g) = source
            && let Some(pi) = g.poll_interval.as_deref()
        {
            parse_duration_str(pi)
                .map_err(|e| anyhow::anyhow!("invalid poll_interval `{pi}` for {}: {e}", g.repo))?;
        }
    }
    Ok(sources)
}

/// Merge a beat's repo-side result with its drained external items
/// into the single emitted result (extra-wait-events, codex fc7a4ff):
/// repo work carries external items along; external-only still
/// returns; both empty parks (`None`). Pure — the deterministic proof
/// that a beat with BOTH ready emits ONE combined result.
fn combine_beat_result(
    repo_items: Option<Vec<WaitItem>>,
    external: Vec<WaitItem>,
) -> Option<Vec<WaitItem>> {
    match repo_items {
        Some(mut items) => {
            items.extend(external);
            Some(items)
        }
        None if !external.is_empty() => Some(external),
        None => None,
    }
}

/// Drain every ready external item into `out`, flipping `open` to
/// false when the source channel has disconnected (all sources
/// finished) so the select branch that reads it stays disabled — the
/// fuse that prevents a closed channel from hot-looping the wait
/// (codex 1bbb61d). No-op once closed.
fn drain_external(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<WaitItem>,
    open: &mut bool,
    out: &mut Vec<WaitItem>,
) {
    if !*open {
        return;
    }
    loop {
        match rx.try_recv() {
            Ok(item) => out.push(item),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                *open = false;
                break;
            }
        }
    }
}

/// External wake sources, spawned when the wait PARKS (the initial
/// pass never spawns them) and torn down on every return path:
/// [`EventSources::shutdown`] aborts AND joins each task, and command
/// children die by process-group kill via a drop guard — abort-safe,
/// so a shell wrapper's grandchildren can't outlive the wait
/// (extra-wait-events).
struct EventSources {
    rx: tokio::sync::mpsc::UnboundedReceiver<WaitItem>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl EventSources {
    fn spawn(sources: &[clank_core::agent_config::WaitEventSource]) -> Self {
        use clank_core::agent_config::WaitEventSource;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        for source in sources {
            match source {
                WaitEventSource::Command(c) => {
                    tasks.push(tokio::spawn(run_command_source(c.clone(), tx.clone())));
                }
                WaitEventSource::Github(g) => {
                    tasks.push(tokio::spawn(crate::cli::github_events::run_github_source(
                        g.clone(),
                        tx.clone(),
                    )));
                }
            }
        }
        Self { rx, tasks }
    }

    async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

/// Kill the child's PROCESS GROUP on drop — grandchildren included.
/// Sync `kill` subprocess, not libc: Drop must be sync and this runs
/// on task abort too (the drop is the cancellation-safety story).
struct GroupKill(Option<u32>);
impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pgid) = self.0.take() {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(format!("-{pgid}"))
                .output();
        }
    }
}

/// Last-`cap`-bytes ring for a command source's interleaved output —
/// bounded by construction, so a chatty child can't balloon memory.
struct RingTail {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl RingTail {
    fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }
    fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.buf.len() == self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(b);
        }
    }
    fn into_string(self) -> String {
        String::from_utf8_lossy(&Vec::from(self.buf)).into_owned()
    }
}

/// One command source: spawn the argv in its own process group, stream
/// stdout+stderr into a 1 KiB ring, and deliver a `CommandEvent` when
/// it completes — the completion IS the wake. Spawn/exec failures
/// degrade to a stderr warning, never the wait.
async fn run_command_source(
    src: clank_core::agent_config::CommandSource,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
) {
    use tokio::io::AsyncReadExt;
    let Some((program, rest)) = src.command.split_first() else {
        eprintln!("wait: command source `{}` has an empty argv", src.name);
        return;
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wait: command source `{}` failed to spawn: {e}", src.name);
            return;
        }
    };
    let _group = GroupKill(child.id());
    // Read stdout AND stderr concurrently INLINE — no detached tasks,
    // so aborting this source task cancels the reads too (codex
    // 907acd5). The ROOT child's exit is completion (codex fc7a4ff): a
    // descendant that inherited a pipe keeps it open past root exit, so
    // we must NOT block for EOF — Phase 1 races reads against
    // `child.wait()` and breaks on exit; Phase 2 best-effort drains
    // whatever bytes are already buffered (bounded), then the group
    // guard kills any lingering descendants.
    let mut ring = RingTail::new(1024);
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let mut obuf = [0u8; 1024];
    let mut ebuf = [0u8; 1024];
    // Phase 1: until the root child exits.
    let status = loop {
        tokio::select! {
            s = child.wait() => break s,
            r = async { out.as_mut().unwrap().read(&mut obuf).await }, if out.is_some() => {
                match r {
                    Ok(0) | Err(_) => out = None,
                    Ok(n) => ring.push(&obuf[..n]),
                }
            }
            r = async { err.as_mut().unwrap().read(&mut ebuf).await }, if err.is_some() => {
                match r {
                    Ok(0) | Err(_) => err = None,
                    Ok(n) => ring.push(&ebuf[..n]),
                }
            }
        }
    };
    // Phase 2: root exited — grab already-buffered bytes, then STOP.
    // Two bounds together, so no descendant behavior can stall the
    // wake (codex a69ddd8): each read has a short quiet-timeout (a
    // pipe held open but silent returns fast) AND the whole drain runs
    // under one ABSOLUTE deadline (a descendant writing CONTINUOUSLY —
    // e.g. a background `yes` — can't keep the reads succeeding
    // forever). Whichever fires first, we emit and the group guard
    // kills what's left. The ring is bounded, so cancelling mid-read
    // loses nothing that matters.
    async fn drain_until_quiet<R: AsyncReadExt + Unpin>(pipe: &mut Option<R>, ring: &mut RingTail) {
        while let Some(p) = pipe.as_mut() {
            let mut buf = [0u8; 1024];
            match tokio::time::timeout(Duration::from_millis(20), p.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => ring.push(&buf[..n]),
                _ => break, // EOF, error, or quiet within the budget
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_millis(100), async {
        drain_until_quiet(&mut out, &mut ring).await;
        drain_until_quiet(&mut err, &mut ring).await;
    })
    .await;
    let exit_code = status.ok().and_then(|s| s.code());
    let output_tail = ring.into_string();
    let _ = tx.send(WaitItem::CommandEvent {
        name: src.name,
        exit_code,
        output_tail,
    });
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
/// always resolves (`wait-output-is-a-minimal-hint`, ruthless
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
    #[serde(rename = "multiple_plans_open")]
    MultiplePlansOpen {
        in_progress: &'a str,
        new_plans: Vec<&'a str>,
        sha: &'a str,
    },
    #[serde(rename = "promote_from_queue")]
    PromoteFromQueue { name: &'a str, priority: u16 },
    #[serde(rename = "github_event")]
    GithubEvent {
        repo: &'a str,
        event: &'a str,
        detail: Option<&'a str>,
        number: Option<u64>,
        title: Option<&'a str>,
        actor: Option<&'a str>,
        url: Option<&'a str>,
    },
    #[serde(rename = "command_event")]
    CommandEvent {
        name: &'a str,
        exit_code: Option<i32>,
        output_tail: &'a str,
    },
    #[serde(rename = "for_commit")]
    ForCommit { sha: &'a str, subject: &'a str },
    #[serde(rename = "for_finished")]
    ForFinished {
        plan: &'a str,
        plan_path: String,
        sha: &'a str,
    },
    #[serde(rename = "for_blocked")]
    ForBlocked { agent: &'a str, name: &'a str },
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
        WaitItem::MultiplePlansOpen {
            in_progress,
            new_plans,
            sha,
        } => WaitJsonItem::MultiplePlansOpen {
            in_progress: in_progress.as_str(),
            new_plans: new_plans.iter().map(|p| p.as_str()).collect(),
            sha: sha.as_str(),
        },
        WaitItem::PromoteFromQueue { name, priority } => WaitJsonItem::PromoteFromQueue {
            name,
            priority: *priority,
        },
        WaitItem::GithubEvent {
            repo,
            event,
            detail,
            number,
            title,
            actor,
            url,
        } => WaitJsonItem::GithubEvent {
            repo,
            event,
            detail: detail.as_deref(),
            number: *number,
            title: title.as_deref(),
            actor: actor.as_deref(),
            url: url.as_deref(),
        },
        WaitItem::CommandEvent {
            name,
            exit_code,
            output_tail,
        } => WaitJsonItem::CommandEvent {
            name,
            exit_code: *exit_code,
            output_tail,
        },
        WaitItem::ForCommit { sha, subject } => WaitJsonItem::ForCommit {
            sha: sha.as_str(),
            subject,
        },
        WaitItem::ForFinished { plan, sha } => WaitJsonItem::ForFinished {
            plan: plan.as_str(),
            plan_path: plan_path(plan),
            sha: sha.as_str(),
        },
        WaitItem::ForBlocked { agent, name } => WaitJsonItem::ForBlocked { agent, name },
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
        WaitItem::MultiplePlansOpen {
            in_progress,
            new_plans,
            sha,
        } => {
            let news: Vec<&str> = new_plans.iter().map(|p| p.as_str()).collect();
            let remedies = multi_plan_open_remedies(in_progress.as_str(), &news)
                .into_iter()
                .map(|r| format!("    {r}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "multiple-plans-open  {}  {} opened while `{}` is unfinished — resolve by ONE of:\n{}",
                short(sha),
                backtick_list(&news),
                in_progress.as_str(),
                remedies,
            )
        }
        WaitItem::PromoteFromQueue { name, priority } => {
            format!("promote  {name}  (priority {priority:03})")
        }
        WaitItem::GithubEvent {
            repo,
            event,
            detail,
            number,
            title,
            actor,
            url,
        } => {
            // Segments joined uniformly so absent optionals leave no
            // dangling separators; the URL rides along — it's the
            // actionable pointer (external-wake-hints-carry-payload).
            let detail = detail
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            let mut parts = vec![format!("github   {event}{detail}"), repo.clone()];
            if let Some(n) = number {
                parts.push(format!("#{n}"));
            }
            if let Some(t) = title.as_deref().filter(|t| !t.is_empty()) {
                parts.push(t.to_string());
            }
            if let Some(a) = actor.as_deref() {
                parts.push(format!("by {a}"));
            }
            if let Some(u) = url.as_deref() {
                parts.push(u.to_string());
            }
            parts.join("  ")
        }
        WaitItem::CommandEvent {
            name,
            exit_code,
            output_tail,
        } => {
            let exit = match exit_code {
                Some(c) => format!("exit {c}"),
                None => "signaled".to_string(),
            };
            // One line: the tail rides in json; the human line shows a
            // single-line snippet.
            let snippet = output_tail.lines().last().unwrap_or("");
            format!("command  {name}  {exit}  {snippet}")
        }
        WaitItem::ForCommit { sha, subject } => {
            format!("for-commit    {}  {subject}", short(sha))
        }
        WaitItem::ForFinished { plan, sha } => {
            format!(
                "for-finished  {plan}  {sha}",
                plan = plan.as_str(),
                sha = short(sha),
            )
        }
        WaitItem::ForBlocked { agent, name } => {
            format!("for-blocked   {agent}/{name}  (awaiting human)")
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

fn backtick_list(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The three numbered remedies for the multiple-open-plans warning
/// (soft-disallow-multiple-plans). ONE source for both `clank wait`'s
/// human render and the stop-hook nudge — the command ORDER is
/// load-bearing: stash only takes the plan whose commits sit at the
/// tip (`build_rewrite_preview` ranges intro..HEAD and foreign
/// commits refuse unconditionally), so new plans push newest-first
/// and the in-progress plan can only be pushed once they're gone.
pub(crate) fn multi_plan_open_remedies(in_progress: &str, new_plans: &[&str]) -> Vec<String> {
    // `new_plans` arrives intro-ordered (oldest first); the tip plan
    // is the newest, so the push sequence reverses it.
    let push_new = new_plans
        .iter()
        .rev()
        .map(|n| format!("`clank stash push {n}`"))
        .collect::<Vec<_>>()
        .join(", then ");
    // The plan to resume after the swap: the newest (the one whose
    // commit tripped the warning).
    let resume = new_plans.last().copied().unwrap_or("?");
    let drop_new = new_plans
        .iter()
        .map(|n| format!("`clank purge --drop {n}`"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "1. park `{in_progress}`, continue `{resume}`: {push_new}, then \
             `clank stash push {in_progress} --for {resume}`, then \
             `clank stash pop {resume}` (stash only takes the plan at the tip, \
             so the new one goes first)"
        ),
        format!(
            "2. finish `{in_progress}` first: {push_new}, reduce \
             `{in_progress}`'s scope if needed, finish it, then \
             `clank stash pop {resume}`"
        ),
        format!(
            "3. fold the new work into `{in_progress}`: {drop_new} and roll the \
             work into `{in_progress}`, widening its scope"
        ),
    ]
}

/// Parse a duration string (`30s`, `5m`, `1h`; `0`/empty = None).
/// Shared by `--timeout` and github `poll_interval` so there's ONE
/// grammar (extra-wait-events).
pub(crate) fn parse_duration_str(raw: &str) -> anyhow::Result<Option<Duration>> {
    parse_timeout(raw)
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

    // ── extra-wait-events: command source field contracts ──────

    fn command_source(name: &str, argv: &[&str]) -> clank_core::agent_config::CommandSource {
        clank_core::agent_config::CommandSource {
            name: name.into(),
            command: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    async fn run_one_command(src: clank_core::agent_config::CommandSource) -> WaitItem {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        run_command_source(src, tx).await;
        rx.recv()
            .await
            .expect("a command source emits exactly one item")
    }

    #[tokio::test]
    async fn command_event_carries_exit_code_and_bounded_tail() {
        let item =
            run_one_command(command_source("ok", &["sh", "-c", "printf hello; exit 7"])).await;
        match item {
            WaitItem::CommandEvent {
                name,
                exit_code,
                output_tail,
            } => {
                assert_eq!(name, "ok");
                assert_eq!(exit_code, Some(7));
                assert_eq!(output_tail, "hello");
            }
            other => panic!("expected CommandEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_event_tail_is_bounded_to_the_last_kib() {
        // 100 KiB of output → exactly the last 1024 bytes survive.
        let item = run_one_command(command_source(
            "chatty",
            &["sh", "-c", "yes x | head -c 100000"],
        ))
        .await;
        match item {
            WaitItem::CommandEvent { output_tail, .. } => {
                assert_eq!(output_tail.len(), 1024, "ring keeps exactly the cap");
                assert!(output_tail.bytes().all(|b| b == b'x' || b == b'\n'));
            }
            other => panic!("expected CommandEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn signal_killed_command_reports_null_exit_code() {
        let item = run_one_command(command_source("selfkill", &["sh", "-c", "kill -9 $$"])).await;
        match item {
            WaitItem::CommandEvent { exit_code, .. } => {
                assert_eq!(exit_code, None, "a signaled child has no exit code");
            }
            other => panic!("expected CommandEvent, got {other:?}"),
        }
    }

    #[test]
    fn ring_tail_bounds_raw_bytes_even_for_non_utf8() {
        // codex 907acd5: the ring's invariant is RAW-byte bounded (the
        // memory bound); the string is lossy-decoded from those last
        // ≤cap bytes. 2000 bytes of 0xFF (invalid UTF-8) → the last
        // 1024 raw bytes → 1024 U+FFFD replacement chars, no more.
        let mut r = RingTail::new(1024);
        r.push(&[0xFFu8; 2000]);
        let out = r.into_string();
        assert_eq!(
            out.chars().count(),
            1024,
            "decoded from exactly cap raw bytes"
        );
        assert!(out.chars().all(|c| c == '\u{FFFD}'));
    }

    #[test]
    fn combine_beat_result_merges_repo_and_external_into_one() {
        // codex fc7a4ff: the deterministic proof that a beat with BOTH
        // a repo item and an external item ready emits ONE result
        // containing both — no wall-clock racing.
        let repo = || {
            Some(vec![WaitItem::Idle {
                prompt: "repo".into(),
            }])
        };
        let ext = || {
            vec![WaitItem::CommandEvent {
                name: "cmd".into(),
                exit_code: Some(0),
                output_tail: String::new(),
            }]
        };
        // Both ready → one result with BOTH.
        let combined = combine_beat_result(repo(), ext()).expect("wake");
        assert_eq!(combined.len(), 2);
        assert!(matches!(combined[0], WaitItem::Idle { .. }));
        assert!(matches!(combined[1], WaitItem::CommandEvent { .. }));
        // External-only still wakes.
        assert_eq!(combine_beat_result(None, ext()).unwrap().len(), 1);
        // Repo-only wakes.
        assert_eq!(combine_beat_result(repo(), Vec::new()).unwrap().len(), 1);
        // Both empty → park.
        assert!(combine_beat_result(None, Vec::new()).is_none());
    }

    #[test]
    fn drain_external_fuses_a_closed_channel() {
        // codex 907acd5: the DETERMINISTIC proof the source branch gets
        // disabled — a disconnected channel flips `open` to false (so
        // the select branch stays off and can't hot-loop), while a live
        // channel drains its queued items and stays open.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(WaitItem::CommandEvent {
            name: "a".into(),
            exit_code: Some(0),
            output_tail: String::new(),
        })
        .unwrap();
        let mut open = true;
        let mut out = Vec::new();
        drain_external(&mut rx, &mut open, &mut out);
        assert_eq!(out.len(), 1, "queued item drained");
        assert!(open, "a live channel stays open");

        drop(tx); // all senders gone → disconnected
        drain_external(&mut rx, &mut open, &mut out);
        assert!(!open, "a closed channel fuses the branch");
        // Once fused, further drains are a no-op (branch disabled).
        out.clear();
        drain_external(&mut rx, &mut open, &mut out);
        assert!(out.is_empty() && !open);
    }

    #[test]
    fn ring_tail_keeps_only_the_last_cap_bytes() {
        let mut r = RingTail::new(4);
        r.push(b"abcdefg");
        assert_eq!(r.into_string(), "defg");
    }

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

    #[test]
    fn multiple_plans_open_line_orders_the_stash_swap_correctly() {
        // The command ORDER is the load-bearing part: stash only
        // takes the tip plan, so the NEW plan's push must come before
        // the in-progress plan's (soft-disallow-multiple-plans).
        let item = WaitItem::MultiplePlansOpen {
            in_progress: PlanKey::parse("old-plan").unwrap(),
            new_plans: vec![PlanKey::parse("new-plan").unwrap()],
            sha: sha("aaaa"),
        };
        let line = render_human(&item);
        assert!(line.contains("multiple-plans-open"));
        assert!(line.contains("`old-plan` is unfinished"), "{line}");
        let push_new = line
            .find("stash push new-plan")
            .expect("remedy pushes the new plan");
        let push_old = line
            .find("stash push old-plan --for new-plan")
            .expect("remedy pushes the in-progress plan with --for");
        assert!(push_new < push_old, "new plan is stashed FIRST: {line}");
        assert!(line.contains("stash pop new-plan"), "{line}");
        for n in ["1.", "2.", "3."] {
            assert!(line.contains(n), "all three remedies present: {line}");
        }
        assert!(
            line.contains("purge --drop new-plan"),
            "fold-in remedy drops the new plan: {line}"
        );
    }

    // ── minimal-hint rendering (wait-output-is-a-minimal-hint) ──

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
    fn github_hint_carries_the_full_payload_and_degrades_cleanly() {
        // external-wake-hints-carry-payload: the one-liner carries the
        // detail and the URL (the actionable pointer)…
        let full = WaitItem::GithubEvent {
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(12),
            title: Some("Add the widget".into()),
            actor: Some("hubot".into()),
            url: Some("https://github.com/o/r/pull/12".into()),
        };
        assert_eq!(
            render_human(&full),
            "github   pr_comment (review)  o/r  #12  Add the widget  by hubot  \
             https://github.com/o/r/pull/12"
        );
        // …and absent optionals leave no dangling separators.
        let sparse = WaitItem::GithubEvent {
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: None,
            title: None,
            actor: None,
            url: None,
        };
        assert_eq!(render_human(&sparse), "github   issue_opened  o/r");
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
            (
                serde_json::to_value(render_json(&WaitItem::MultiplePlansOpen {
                    in_progress: PlanKey::parse("old").unwrap(),
                    new_plans: vec![PlanKey::parse("new").unwrap()],
                    sha: sha("abc"),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "multiple_plans_open",
                    "in_progress": "old",
                    "new_plans": ["new"],
                    "sha": sha("abc").as_str(),
                }),
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    // ── wait --for observer items (wait-for-observer-mode) ─────

    #[test]
    fn observer_items_render_distinct_kinds_and_one_line_humans() {
        // Distinct for_* kinds so a consumer can't mistake a foreign
        // observation for its own work items.
        let commit = WaitItem::ForCommit {
            sha: sha("abc"),
            subject: "did things".into(),
        };
        assert_eq!(
            serde_json::to_value(render_json(&commit)).unwrap(),
            serde_json::json!({
                "kind": "for_commit",
                "sha": sha("abc").as_str(),
                "subject": "did things",
            })
        );
        assert_eq!(
            render_human(&commit),
            format!("for-commit    {}  did things", short(&sha("abc")))
        );

        let fin = WaitItem::ForFinished {
            plan: PlanKey::parse("foo").unwrap(),
            sha: sha("abc"),
        };
        assert_eq!(
            serde_json::to_value(render_json(&fin)).unwrap(),
            serde_json::json!({
                "kind": "for_finished",
                "plan": "foo",
                "plan_path": ".clank/plans/foo.md",
                "sha": sha("abc").as_str(),
            })
        );
        assert_eq!(
            render_human(&fin),
            format!("for-finished  foo  {}", short(&sha("abc")))
        );

        let blocked = WaitItem::ForBlocked {
            agent: "codex".into(),
            name: "q".into(),
        };
        assert_eq!(
            serde_json::to_value(render_json(&blocked)).unwrap(),
            serde_json::json!({ "kind": "for_blocked", "agent": "codex", "name": "q" })
        );
        assert_eq!(
            render_human(&blocked),
            "for-blocked   codex/q  (awaiting human)"
        );
    }

    #[test]
    fn observer_items_never_fire_hooks() {
        // The observer path is side-effect-free by contract; even if
        // its items ever flowed through the firing projection, they
        // must map to none.
        let items = [
            WaitItem::ForCommit {
                sha: sha("abc"),
                subject: "s".into(),
            },
            WaitItem::ForFinished {
                plan: PlanKey::parse("foo").unwrap(),
                sha: sha("abc"),
            },
            WaitItem::ForBlocked {
                agent: "a".into(),
                name: "n".into(),
            },
        ];
        assert!(firings_from_items(&items).is_empty());
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
