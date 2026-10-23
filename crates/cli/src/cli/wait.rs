//! `clank wait` — block until the calling agent has wait-surface
//! items to print.
//!
//! Two roles, DERIVED from `--author`'s roster entry — never
//! supplied, because which side of the workflow an agent plays is
//! not orthogonal to who it is.
//!
//! The roster's master watches for plans where the gate has moved on
//! without master. A reviewer watches for plans whose latest reviewable
//! commit needs its opinion.
//!
//! `wait` folds the repo, runs `RepoState::derive_status` (which
//! threads `wait::compute_gate` over every plan), filters the
//! result through `RepoState::work_for` for the agent's
//! perspective. When the initial fold finds at least one item it prints it and exits;
//! otherwise it watches the filesystem for changes that could
//! plausibly flip the projection and refolds on each debounced
//! event.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use super::{WaitArgs, resolve_repo};
use crate::cli::block::scan_blocks;
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use clank_core::Role;
use clank_core::wait::WaitItem;

/// A wait launched inside a harness is allowed to emit only while that
/// exact tool/session still holds the label. A terminal wait with no
/// session remains a direct, unclaimed observer.
#[derive(Debug, Clone)]
pub(crate) struct WaitBinding(
    Option<(clank_core::Tool, clank_core::SessionId)>,
    WaitPolicy,
);

#[derive(Debug, Clone)]
enum WaitPolicy {
    Manual,
    // Check effective auto-mode on existing monitor notes/local beats too,
    // including manual/global edits. Manual waits never inherit this policy.
    AutomaticGrok { home: Option<std::path::PathBuf> },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct WaitSuperseded(String);

#[derive(Debug, thiserror::Error)]
#[error("wait --peek found no work for this agent")]
pub(crate) struct WaitNoWork;

/// Whether `error` says this wait's session no longer holds its label.
pub(crate) fn superseded(error: &anyhow::Error) -> bool {
    error.is::<WaitSuperseded>()
}

impl WaitBinding {
    /// The pair this wait speaks for, as the monitor's hello wants it:
    /// both halves or neither, never a half.
    pub(crate) fn holder(&self) -> Option<crate::cli::monitor::Holder> {
        self.0
            .as_ref()
            .map(|(tool, session)| crate::cli::monitor::Holder {
                tool: *tool,
                session: session.as_str().to_string(),
            })
    }

    pub(crate) fn capture(args: &WaitArgs) -> anyhow::Result<Self> {
        let detected = crate::agent_env::detect_session_from_env()?;
        if let Some(expected) = args.deliver_to_session.as_deref() {
            let Some((_, session)) = detected.as_ref() else {
                return Err(WaitSuperseded(format!(
                    "wait cannot deliver to session {expected}: no session was detected in this process"
                ))
                .into());
            };
            if session.as_str() != expected {
                return Err(WaitSuperseded(format!(
                    "wait cannot deliver to session {expected}: this process belongs to session {}",
                    session.as_str()
                ))
                .into());
            }
        }
        Ok(Self(detected, WaitPolicy::Manual))
    }

    fn ensure_current(&self, repo: &Path, author: &AgentLabel) -> anyhow::Result<()> {
        if self.0.is_none() && matches!(self.1, WaitPolicy::Manual) {
            return Ok(());
        }
        // Auto policy shares the binding check's read. No second config read
        // is added to the existing idle monitor notes/local beats.
        let loaded = crate::agent_store::load_agent_config(repo, author);
        if let WaitPolicy::AutomaticGrok { home } = &self.1 {
            let cfg = loaded
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
            anyhow::ensure!(
                crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref())
                    == clank_core::AutoMode::On,
                "automatic Grok wait cancelled: Clank auto mode is off"
            );
        }
        let Some((tool, session)) = &self.0 else {
            return Ok(());
        };
        let cfg = loaded.ok().flatten();
        let bound = cfg
            .as_ref()
            .and_then(|cfg| cfg.session.as_ref())
            .is_some_and(|current| current.tool == *tool && &current.id == session);
        if !bound {
            let current = cfg
                .and_then(|cfg| cfg.session)
                .map(|s| format!("{} session {}", s.tool.as_str(), s.id.as_str()))
                .unwrap_or_else(|| "no current session".into());
            return Err(WaitSuperseded(format!(
                "wait for {author} cannot continue: this {} session {} does not own the label ({current}); run `clank as {author}` inside the intended session",
                tool.as_str(),
                session.as_str(),
            ))
            .into());
        }
        Ok(())
    }
}

pub async fn run(args: WaitArgs) -> anyhow::Result<()> {
    if args.turn_end {
        return super::turn_child::run(args).await;
    }
    if let Some((repo, author, binding)) = automatic_grok(&args, &crate::agent_env::Process)? {
        return run_automatic_grok(args, &repo, &author, &binding).await;
    }
    let json = args.json;
    let peek = args.peek;
    let repo = resolve_repo(args.repo.as_deref())?;
    let boundary = if args.r#for.is_some() {
        None
    } else {
        let author = match args.author.as_deref() {
            Some(raw) => AgentLabel::parse(raw)?,
            None => crate::agent_env::resolve_identity_from_env(&repo)?,
        };
        let binding = match WaitBinding::capture(&args) {
            Ok(binding) => binding,
            Err(e) if e.is::<WaitSuperseded>() => return superseded_error(e, json),
            Err(e) => return Err(e),
        };
        Some((author, binding))
    };
    let mut source_cwd = None;
    let result = match items_with_context(args, &mut source_cwd).await {
        Err(e) if e.is::<WaitSuperseded>() => return superseded_error(e, json),
        other => other?,
    };
    if let Some(items) = result {
        if let Some((author, binding)) = boundary {
            if let Err(e) = binding.ensure_current(&repo, &author) {
                return superseded_error(e, json);
            }
        }
        if items.is_empty() {
            if peek {
                return Err(WaitNoWork.into());
            }
            anyhow::bail!("wait ended without work; the waiter returned an empty result");
        }
        emit_with_context(&items, json, source_cwd.as_deref());
    }
    Ok(())
}

/// Native session identity is enough. The launch cwd fixes the repository;
/// `--repo` can restate it but cannot move this automatic waiter elsewhere.
pub(super) fn automatic_grok(
    args: &WaitArgs,
    env: &dyn crate::agent_env::Ambient,
) -> anyhow::Result<Option<(PathBuf, AgentLabel, WaitBinding)>> {
    if args.peek || args.r#for.is_some() || args.die_with_owner {
        return Ok(None);
    }
    let Some(raw) = env
        .var(crate::agent_env::ENV_GROK_SESSION)
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let session = clank_core::SessionId::parse(&raw)?;
    let cwd = env
        .cwd()
        .ok_or_else(|| anyhow::anyhow!("Grok wait has no working directory"))?;
    let bound_repo = crate::cli::resolve_cwd_repo(&cwd)?;
    let author = crate::agent_env::resolve_identity_for_hook_in(
        &bound_repo,
        clank_core::Tool::Grok,
        &session,
        env,
    )?;
    let cfg = crate::agent_store::load_agent_config(&bound_repo, &author)?;
    if crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), env.home().as_deref())
        == clank_core::AutoMode::Off
    {
        return Ok(None);
    }
    let repo = match args.repo.as_deref() {
        Some(path) => resolve_repo(Some(path))?,
        None => bound_repo.clone(),
    };
    anyhow::ensure!(
        repo == bound_repo,
        "automatic Grok wait cannot leave its session's repository"
    );
    anyhow::ensure!(
        args.author
            .as_deref()
            .is_none_or(|label| label == author.as_str()),
        "automatic Grok wait cannot override its session's agent"
    );
    let binding = WaitBinding(
        Some((clank_core::Tool::Grok, session)),
        WaitPolicy::AutomaticGrok { home: env.home() },
    );
    binding.ensure_current(&repo, &author)?;
    Ok(Some((repo, author, binding)))
}

async fn run_automatic_grok(
    args: WaitArgs,
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.events.is_empty(),
        "automatic Grok wait uses monitor-hosted sources; configure wait_events or use auto off for invocation-only --event sources"
    );
    loop {
        binding.ensure_current(repo, author)?;
        let attached = crate::cli::monitor::attach_or_spawn_with(
            repo,
            crate::cli::monitor::Who::Agent {
                author: author.to_string(),
                holder: binding.holder(),
                has_cli_sources: false,
                turn_end: None,
            },
            crate::cli::monitor::spawn_monitor,
        )
        .await;
        let crate::cli::monitor::Attached::Shared(conn) = attached else {
            anyhow::bail!(
                "automatic Grok wait could not subscribe to its repository monitor: {attached:?}"
            );
        };
        tracing::debug!(%author, "monitor subscription registered");
        let wake = shared_wait_within(conn, repo, author, binding, crate::cli::monitor::BEAT_GRACE)
            .await?;
        match wake {
            Shared::Woke(items, cwd) => {
                return deliver_grok_wake(
                    repo,
                    author,
                    binding,
                    &items,
                    args.json,
                    cwd.as_deref(),
                    &mut std::io::stdout(),
                );
            }
            // Reattach only after a real disconnect or the existing monitor
            // liveness deadline. No polling, helper or Grok registration timer.
            Shared::Alone(why) => tracing::debug!(%why, "reconnecting Grok wait"),
        }
    }
}

/// Grok's delivery boundary: the wake is printed, and only once the
/// output landed, while this session still holds the label, is what it
/// carried recorded as delivered.
fn deliver_grok_wake(
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
    items: &[WaitItem],
    json: bool,
    cwd: Option<&Path>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    binding.ensure_current(repo, author)?;
    emit_checked(items, json, cwd, out)?;
    let mirrored = super::wake_render::mirror(items, cwd);
    super::presentation::record_delivered(
        repo,
        author,
        &super::wake_render::carried(&mirrored),
        || binding.ensure_current(repo, author).is_ok(),
    );
    Ok(())
}

/// A token for one wait invocation. Monotonic within a process and
/// time-seeded across them: it only has to differ from every other
/// invocation's.
fn new_invocation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    nanos ^ (NEXT.fetch_add(1, Ordering::Relaxed) << 48)
}

fn superseded_error(error: anyhow::Error, json: bool) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&SupersededEnvelope {
                items: [],
                superseded: true,
            })?
        );
    }
    Err(error)
}

/// How many times one invocation reached the source-start boundary.
///
/// A test needs this because the CHILD cannot answer it. A source that
/// is spawned and then torn down before its command execs leaves
/// exactly what never spawning leaves — nothing — so a marker file
/// cannot tell the two apart, and one of them is a side-effect-free
/// `--peek` while the other is not. Measured: with the peek return
/// moved below the spawn, the marker assertion caught the variant that
/// left the tasks detached and MISSED the variant that shut them down
/// (codex on fbb84e3).
///
/// Scoped to one call rather than process-wide, so concurrent tests
/// cannot contaminate each other.
#[derive(Clone, Default, Debug)]
pub struct SourceStarts(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl SourceStarts {
    pub fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn record(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What this wait wakes with — the same items `run` prints — or `None`
/// when an observer (`--for`) already reported its own.
pub async fn items(args: WaitArgs) -> anyhow::Result<Option<Vec<WaitItem>>> {
    items_observing(args, &SourceStarts::default()).await
}

/// [`items`] with the source-start boundary observed.
pub async fn items_observing(
    args: WaitArgs,
    starts: &SourceStarts,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    items_with_policy(args, starts, &mut None, Local::ANY).await
}

async fn items_with_context(
    args: WaitArgs,
    source_cwd: &mut Option<PathBuf>,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    items_with_policy(args, &SourceStarts::default(), source_cwd, Local::ANY).await
}

/// Whether owed items end a wait. Items it declines are owed again at the
/// next change, not lost.
pub(crate) type Accept<'a> = &'a mut (dyn FnMut(&[WaitItem]) -> bool + Send);

/// How a wait decides it is done with what it found.
pub(crate) struct Local<'a> {
    /// Whether it may share the monitor's watch rather than its own.
    pub(crate) share: bool,
    pub(crate) accept: Option<Accept<'a>>,
}

impl Local<'_> {
    const ANY: Local<'static> = Local {
        share: true,
        accept: None,
    };

    fn takes(&mut self, items: &[WaitItem]) -> bool {
        !items.is_empty() && self.accept.as_mut().is_none_or(|accept| accept(items))
    }
}

/// A wait that never shares, ending only on items `accept` takes: the
/// local fold a report's child falls back to when no monitor can decide.
pub(crate) async fn items_alone(
    args: WaitArgs,
    source_cwd: &mut Option<PathBuf>,
    accept: Accept<'_>,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    items_with_policy(
        args,
        &SourceStarts::default(),
        source_cwd,
        Local {
            share: false,
            accept: Some(accept),
        },
    )
    .await
}

async fn items_with_policy(
    args: WaitArgs,
    starts: &SourceStarts,
    source_cwd: &mut Option<PathBuf>,
    mut local: Local<'_>,
) -> anyhow::Result<Option<Vec<WaitItem>>> {
    *source_cwd = std::env::current_dir().ok();
    // Before ANY setup: the fold and the watcher attach can block for
    // seconds, and an owner that dies in that window must still reap
    // this process (remove-wait-timeout).
    if args.die_with_owner {
        crate::owner_sentinel::spawn_stdin_sentinel();
    }
    // One env read for the entire process, here at the CLI
    // boundary. Downstream takes a plain `bool`.
    let poll_mode = args.effective_poll();

    let repo = resolve_repo(args.repo.as_deref())?;

    // Observer mode (`--for`, wait-for-observer-mode): a pure
    // cross-repo observer — no identity, no role, no work
    // projection, no lifecycle hooks. Branches BEFORE identity
    // resolution: the observed repo has no session binding for the
    // caller, and --author is deliberately ignored.
    if let Some(event) = args.r#for {
        run_observer(&repo, event, poll_mode, args.no_cache, args.json).await?;
        return Ok(None);
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

    // Extra wake sources: the agent config's `wait_events` plus any
    // `--event` items (config first, CLI appended). Parse errors are
    // arm-time hard errors naming the offending input.
    let event_sources = resolve_wait_event_sources(&repo, &author, &args.events)?;
    // Arm-time derivation errors stay HARD (a wait that can never
    // project is a bug to surface); the loop re-derives per wake and
    // is fail-soft there (wait-reloads-config-per-refold).
    let mut inputs = derive_wait_inputs(&repo, &author)?;

    let binding = WaitBinding::capture(&args)?;
    binding.ensure_current(&repo, &author)?;

    let events_dir = crate::agent_store::agents_root(&repo)
        .join(author.as_str())
        .join("events");
    // Mint once before native acceptance; preserve it through shared fallback.
    let invocation = (!args.peek).then(|| new_invocation().to_string());
    let (source_tx, mut source_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timers = if args.peek {
        None
    } else {
        let mut timers = crate::cli::timer_source::TimerSet::new(
            &event_sources,
            &events_dir,
            invocation.as_deref(),
            source_tx.clone().into(),
        )?;
        timers.arm();
        Some(timers)
    };
    let invocation_keys = timers
        .as_ref()
        .map(|timers| timers.invocation_keys().clone())
        .unwrap_or_default();
    let mut timer_backlog =
        crate::cli::timer_source::Backlog::new(events_dir.clone(), invocation_keys);
    if args.peek {
        timer_backlog = timer_backlog.inspection();
    }

    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };
    let initial_state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    binding.ensure_current(&repo, &author)?;

    // A broken HEAD tag is a derived, dominating state
    // (commit-tag-fixup-is-first-class-state): derive_status yields
    // the correction and work_for routes master to the FixCommitTag
    // item / withholds reviewer wakes — no bespoke side-check.
    let initial_status = {
        let reviews =
            crate::fs_plan_state_lookup::FsPlanStateLookup::new(&repo, initial_state.head.as_ref());
        let head = crate::git_io::head_commit_at(&repo, &initial_state);
        initial_state
            .fold
            .derive_status(&reviews, &inputs.work_policy, head.as_ref())
    };
    {
        let items = present_with_blocks(check_blocks(&repo, &author), || {
            let mut owed = items_for(
                &repo,
                &author,
                &inputs,
                &initial_status,
                &initial_state.fold,
            );
            owed.events
                .extend(timer_backlog.owed(&mut Default::default()));
            if args.peek {
                // The hook's SessionStart actually emits this catch-up.
                // Peek inspects custody without starting producers or receipts.
                owed.events
                    .extend(crate::cli::event_source::owed_from_sources(
                        &repo,
                        &author,
                        &event_sources,
                        &mut Default::default(),
                    ));
            }
            owed
        });
        if (args.peek || local.takes(&items)) && !items.is_empty() {
            binding.ensure_current(&repo, &author)?;
            return Ok(Some(items));
        }
    }

    // `--peek`: the initial pass found no actionable work → report empty
    // and return immediately, never entering the (blocking) watcher loop.
    if args.peek {
        return Ok(Some(Vec::new()));
    }

    // Share the repo's watch when a monitor is already doing it, which
    // is the whole point of `a-repo-is-watched-once-not-once-per-waiter`:
    // every parked wait used to attach its own watcher and refold on
    // every change, measured at 98 of them for 71 agents.
    //
    // The monitor hosts configured sources. Invocation sources remain
    // local, so share only when that host covers this wait's complete
    // source scope.
    //
    // Derived BEFORE the decision, and the same value is what the park
    // below spawns: a source the decision cannot see is a source
    // nobody runs.
    //
    // `--peek` never reaches here: it returned above, which is what
    // keeps it side-effect-free.
    //
    // Sharing is an optimisation, never a dependency: a monitor that
    // dies mid-park sends this wait down to the local watcher below
    // rather than back to the hook empty-handed. Returning the failure
    // here would have been a LOST WAKE — the hook sees a failed wait,
    // does not re-arm, and the agent is never woken again.
    let sources = WakeSources {
        events: event_sources,
    };
    let shared = match local.share {
        true => share(&repo, &author, &binding, &sources).await,
        false => None,
    };
    if let Some(conn) = shared {
        match shared_wait(conn, &repo, &author, &binding).await? {
            Shared::Woke(items, cwd) => {
                *source_cwd = cwd;
                return Ok(Some(items));
            }
            Shared::Alone(why) => tracing::debug!("watching {} locally: {why}", repo.display()),
        }
    }

    let (tx, rx) = mpsc::channel::<()>();
    // The shared core watcher: `.clank/` gate dirs + gitdir (gitdir
    // skipped in poll mode — the heartbeat tick below covers git then).
    // Same producer `clank status` uses, so the panel and this loop wake
    // on the same changes.
    let timer_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _watcher = crate::repo_watch::RepoStateWatcher::attach_with_timer_invalidation(
        &repo,
        poll_mode,
        tx,
        Some(timer_dirty.clone()),
    )?;
    // Close the admission race: ack may have happened after the initial fold
    // but before this watcher existed. Later writes leave dirty set for a wake.
    if let Some(timers) = &mut timers {
        timers.refresh();
    }
    timer_backlog.invalidate();

    // Refold cadence. Native mode uses a 1.5s heartbeat as the
    // finalize-race safety net (the watcher carries the load).
    // Polling mode shortens to 500ms — it IS the primary git-
    // change signal because the gitdir watch is skipped.
    let tick: Duration = if poll_mode {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(1500)
    };

    // ONE supervised async loop (extra-wait-events, codex cdcf111):
    // the watcher's sync channel is bridged into a tokio::select over
    // repo wakes, external source items, and the heartbeat tick.
    // Sources spawn only when the wait actually PARKS (the
    // initial pass above returns without them) and are cancelled AND
    // joined on every return path below.
    let mut beat = Beat::bridge(rx);
    // The source-start boundary. Recorded here rather than inside
    // `EventSources::spawn`, because the claim under test is about this
    // PATH reaching it, not about that function's own behaviour.
    starts.record();
    let spawned = EventSources::spawn(
        &sources.events,
        Some(&events_dir),
        invocation.as_deref(),
        source_tx.into(),
    );
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
            let timer_deadline = timers.as_ref().and_then(|timers| timers.next_deadline());
            let existing_wake = tokio::select! {
                _ = crate::cli::timer_source::deadline(timer_deadline) => {
                    if let Some(timers) = &mut timers { timers.due(); }
                    timer_backlog.invalidate();
                    false
                }
                _ = tokio::time::sleep(tick) => { true }
                w = beat.rx.recv() => {
                    if w.is_none() {
                        anyhow::bail!("filesystem watcher disconnected");
                    }
                    // Debounce: let a burst settle, then drain it —
                    // one logical change, one refold.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    while beat.rx.try_recv().is_ok() {}
                    true
                }
                item = source_rx.recv(), if sources_open => {
                    match item {
                        // The local path delivers from the channel
                        // whatever the log holds, so provenance is not
                        // its question.
                        Some((item, _)) => external.push(item),
                        None => sources_open = false,
                    }
                    true
                }
            };
            let timers_changed = timer_dirty.swap(false, std::sync::atomic::Ordering::AcqRel);
            if poll_mode || timers_changed {
                if let Some(timers) = &mut timers {
                    timers.refresh();
                    timers.retry_one_shots();
                }
                timer_backlog.invalidate();
            } else if existing_wake && let Some(timers) = &mut timers {
                timers.retry();
            }
            // The same beat that notices repo changes also notices a
            // new incarnation. No second hook-side poller is needed.
            binding.ensure_current(&repo, &author)?;
            // Drain items queued before/at this beat (whichever branch
            // woke us) so a repo-branch win doesn't strand a ready
            // external item (codex 1bbb61d).
            drain_external(&mut source_rx, &mut sources_open, &mut external);

            // Re-derive the projection inputs EVERY wake: config.json is a
            // wake dir, and a parked wait projecting with the team captured
            // at arm time computes stale work after any roster change
            // (wait-reloads-config-per-refold). Fail-soft mid-loop only for
            // transient failures: an identity successfully resolved absent
            // from the roster has been revoked and must stop this wait rather
            // than retain its last-known role.
            match derive_wait_inputs(&repo, &author) {
                Ok(fresh) => inputs = fresh,
                Err(err)
                    if matches!(
                        err.downcast_ref::<crate::agent_store::RoleResolutionError>(),
                        Some(crate::agent_store::RoleResolutionError::NotRegistered { .. })
                    ) =>
                {
                    return Err(err);
                }
                Err(_) => {}
            }

            // Compute this beat's REPO-side result (None = park). Any
            // return then funnels through the single boundary below so a
            // final drain catches items that arrived DURING the refold
            // (`await`) — none is ever dropped (codex 907acd5).
            let state = crate::rebuild::rebuild_repo_with_policy(&repo, policy)
                .await
                .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
            let reviews =
                crate::fs_plan_state_lookup::FsPlanStateLookup::new(&repo, state.head.as_ref());
            let head = crate::git_io::head_commit_at(&repo, &state);
            let status = state
                .fold
                .derive_status(&reviews, &inputs.work_policy, head.as_ref());
            // THE single return boundary: one last drain catches items
            // produced during the refold, and then BOTH routes meet the
            // block decision TOGETHER. Park only when it admits nothing
            // (codex 907acd5; codex 344280f for the together).
            drain_external(&mut source_rx, &mut sources_open, &mut external);
            // Native emissions are signals to reread accepted custody, so the
            // same row cannot enter through both the sink and discovery.
            let had_timers = external
                .iter()
                .any(|item| matches!(item, WaitItem::TimerEvent { .. }));
            external.retain(|item| !matches!(item, WaitItem::TimerEvent { .. }));
            if had_timers {
                timer_backlog.invalidate();
            }
            external.extend(timer_backlog.owed(&mut Default::default()));
            // Nothing is lost by draining into the decision. It
            // returns empty only when `events` is empty, and these
            // items ARE events — which matters because a source
            // running memory-only (`events_dir: None`) has no WAL to
            // re-read them from.
            let items = owed_for(&repo, &author, &inputs, &status, &state.fold, &mut external);
            if local.takes(&items) {
                binding.ensure_current(&repo, &author)?;
                return Ok(items);
            }
        }
    }
    .await;

    // Every return path — items, timeout, error — cancels AND joins
    // the source tasks; command children die by process-group kill
    // (extra-wait-events).
    spawned.shutdown().await;
    Ok(Some(outcome?))
}

/// The watcher's sync channel, bridged into async.
///
/// The bridge is not a convenience. `mpsc::Receiver::recv_timeout` is
/// SYNCHRONOUS, so awaiting a beat that way parks a runtime worker in
/// blocking code — where `abort()` cannot reach it and a multi-thread
/// runtime's drop waits for it. A test that aborted such a task hung
/// in teardown AFTER its assertions had passed, which is the failure
/// mode `attach_cost`'s doc predicted: the test's own timeout future
/// is never polled, so it hangs instead of failing.
///
/// One reader thread does the blocking recv and forwards; every await
/// here is cancel-safe.
pub(crate) struct Beat {
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl Beat {
    pub(crate) fn bridge(rx: mpsc::Receiver<()>) -> Self {
        let (tx, arx) = tokio::sync::mpsc::unbounded_channel::<()>();
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                if tx.send(()).is_err() {
                    break;
                }
            }
        });
        Self { rx: arx }
    }

    /// One beat: the next FS event or the heartbeat tick, whichever
    /// lands first, with event bursts debounced to one refold. Ends on
    /// a wake, a real error, or owner death — never a clock
    /// (remove-wait-timeout).
    pub(crate) async fn next(&mut self, tick: Duration) -> anyhow::Result<()> {
        tokio::select! {
            _ = tokio::time::sleep(tick) => Ok(()),
            w = self.rx.recv() => {
                if w.is_none() {
                    anyhow::bail!("filesystem watcher disconnected");
                }
                // Let a burst settle, then drain it — one logical
                // change, one refold.
                tokio::time::sleep(Duration::from_millis(200)).await;
                while self.rx.try_recv().is_ok() {}
                Ok(())
            }
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
    let mut beat = Beat::bridge(rx);
    loop {
        beat.next(tick).await?;
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
/// events are measured against. An observer must fire for a plan that
/// is introduced AND finalized after it starts.
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
/// deliberately not here (fixed at arm time by design); the ROLE is,
/// because it derives from that identity and must follow a roster
/// that changes under a parked wait.
pub(crate) struct WaitInputs {
    role: Role,
    pub(crate) work_policy: clank_core::wait::WorkPolicy,
}

pub(crate) fn derive_wait_inputs(repo: &Path, author: &AgentLabel) -> anyhow::Result<WaitInputs> {
    // Role is DERIVED from identity, never supplied. Which side of the
    // workflow you play is not orthogonal to who you are, so a
    // caller cannot hold a role the roster contradicts — the state
    // that left a master's wait blocked forever on reviewer work.
    //
    // Re-derived every refold, so a roster change corrects a parked
    // wait on its next wake rather than stranding it.
    let role = crate::agent_store::resolve_role(repo, author)?;
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
    })
}

/// The repo's monitor, if this wait may use it.
///
/// Sharing is the point of `a-repo-is-watched-once-not-once-per-waiter`:
/// every parked wait used to attach its own watcher and refold on every
/// change, measured at 98 of them for 71 agents.
///
/// `None` when this wait has ANY extra source. The monitor delivers the
/// projection but does not yet run `wait_events`, so a wait with github
/// or command sources must keep watching for itself — sharing would
/// mean it simply never heard from them. Moving those to the monitor is
/// the next step; until then this guard is what makes sharing safe
/// rather than lossy.
///
/// `--peek` never reaches here: it returns before this, which is what
/// keeps it side-effect-free.
/// Everything besides the repo's own state that can wake this wait.
///
/// One value, because two decisions consume it: whether this wait may
/// share the monitor's watch, and what the park spawns. A source the
/// share decision cannot see is a source nobody runs once the wait
/// shares.
struct WakeSources {
    events: Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
}

impl WakeSources {
    /// Whether the monitor can cover everything that wakes this wait.
    ///
    /// Destructured on purpose: a source kind added later cannot be
    /// forgotten here, because the pattern stops compiling until it is
    /// named. That is the invariant this type exists to hold.
    fn may_share(&self) -> bool {
        let Self { events } = self;
        // An author's CONFIG sources the monitor runs, once per
        // repo, and routes back to that author. A `--event` source
        // belongs to ONE INVOCATION and is never pooled — a monitor
        // that ran one would deliver this invocation's events to
        // whoever else happened to be this author.
        events
            .iter()
            .all(|(_, origin)| matches!(origin, crate::cli::event_source::SourceOrigin::Config))
    }
}

async fn share(
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
    sources: &WakeSources,
) -> Option<crate::cli::monitor::Conn> {
    share_with(
        repo,
        author,
        binding,
        sources,
        crate::cli::monitor::spawn_monitor,
    )
    .await
}

/// [`share`] with the starting supplied.
///
/// The seam is here rather than in a test's fixture because a fixture
/// cannot be trusted to hold: a monitor that stopped admitting would
/// send this straight to the real `spawn_monitor`, and the test's
/// assertion would still pass — on a process that had taken the temp
/// repo's lease and outlived the test. Measured, not imagined: two
/// sockets under `/tmp/clank-socks` from exactly that.
async fn share_with(
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
    sources: &WakeSources,
    spawn: impl Fn(&Path) -> std::io::Result<Box<dyn crate::cli::monitor::Started>>,
) -> Option<crate::cli::monitor::Conn> {
    if !sources.may_share() {
        return None;
    }
    match crate::cli::monitor::attach_or_spawn_with(
        repo,
        crate::cli::monitor::Who::Agent {
            author: author.to_string(),
            holder: binding.holder(),
            // The monitor runs no `--event` source, and a wait that
            // declared one never reaches here: `sources.is_empty()`
            // above already sent it to watch for itself.
            has_cli_sources: false,
            turn_end: None,
        },
        spawn,
    )
    .await
    {
        crate::cli::monitor::Attached::Shared(conn) => {
            // One debug event at the accepted-subscription boundary, useful
            // when diagnosing a parked wait. No new idle work or timer.
            tracing::debug!(author = %author, "monitor subscription registered");
            Some(conn)
        }
        crate::cli::monitor::Attached::Alone(why) => {
            tracing::debug!("watching {} locally: {why}", repo.display());
            None
        }
    }
}

/// How a shared wait ended.
///
/// No silent variant, for the reason [`crate::cli::monitor::Attached`]
/// has none: a wait that stops being woken is the one outcome this
/// plan may not produce. Sharing can fail — the monitor can die
/// mid-park — and every way it fails is a reason to watch locally, so
/// that is what the type says. A supersession is NOT in here: being
/// told this session no longer owns the label is a refusal to answer
/// at all, and stays an error.
#[derive(Debug)]
enum Shared {
    /// Work, decided at the monitor's fold.
    Woke(Vec<WaitItem>, Option<PathBuf>),
    /// The sharing ended. The string says why, for the log.
    Alone(String),
}

/// A wait that shares the repo's monitor instead of watching itself.
///
/// Items arrive already decided: the monitor derives them through
/// `owed_for`, which is `present_with_blocks`, so a block's blast
/// radius is settled in the one place that settles it.
///
/// Silence ends it: a wedged monitor and a dead one look the same to a
/// client, and the beat is what distinguishes either from a quiet repo.
async fn shared_wait(
    conn: crate::cli::monitor::Conn,
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
) -> anyhow::Result<Shared> {
    shared_wait_within(conn, repo, author, binding, crate::cli::monitor::BEAT_GRACE).await
}

/// [`shared_wait`] with the grace supplied, so silence is testable
/// without waiting the production twenty seconds.
async fn shared_wait_within(
    conn: crate::cli::monitor::Conn,
    repo: &Path,
    author: &AgentLabel,
    binding: &WaitBinding,
    grace: Duration,
) -> anyhow::Result<Shared> {
    let mut notes = bridge_notes(conn, grace)?;
    // The sharing ends exactly one way: the reader is gone. A failed
    // read is the REASON, not a second ending — giving it an arm of
    // its own would make two code paths for one rule, and each would
    // then mask the other's absence.
    let mut why = "its reader stopped without saying why".to_string();
    loop {
        match notes.recv().await {
            Some(Ok(note)) => {
                // Ownership is a LIFECYCLE check, not a condition on
                // delivery: every note is this wait's chance to notice
                // it has been superseded, exactly as the local loop
                // checks on every beat. Checking only when items
                // arrive left a rebound session parked forever
                // whenever the repo happened to owe nothing — healthy
                // beats, no work, no exit (codex on 90985a6).
                binding.ensure_current(repo, author)?;
                // A fold that owes this author nothing, and the beat
                // that says the monitor is alive, are both just "keep
                // waiting".
                match note {
                    crate::cli::monitor::Note::Items { items, source_cwd } if !items.is_empty() => {
                        return Ok(Shared::Woke(items, source_cwd));
                    }
                    crate::cli::monitor::Note::Refused { why } => {
                        anyhow::bail!("monitor refused wait: {why}")
                    }
                    // Terminal: a newer wait of this session holds the
                    // repo's delivery now. Watching locally instead would
                    // deliver the same work a second time.
                    crate::cli::monitor::Note::Superseded => {
                        return Err(WaitSuperseded(
                            "a newer wait of this session was admitted by the monitor".into(),
                        )
                        .into());
                    }
                    _ => {}
                }
            }
            Some(Err(e)) => why = e,
            None => break,
        }
    }
    // Silence or a broken frame: stop sharing and watch for ourselves
    // rather than wait on a monitor that has stopped.
    Ok(Shared::Alone(format!(
        "the monitor for {} stopped answering ({why})",
        repo.display()
    )))
}

/// The monitor's notes, bridged onto the async loop.
///
/// `Conn` reads a BLOCKING socket, and a park that holds the runtime
/// thread is what `Beat::bridge` exists to avoid on the local path. It
/// is also what leaves the loop above with no await point: nothing can
/// bound a future that never yields, so a monitor's silence becomes a
/// hang instead of a fallback.
///
/// Dropping the receiver shuts down the socket and immediately releases the
/// reader thread, including when a different source wins the wait's select.
pub(crate) fn bridge_notes(
    mut conn: crate::cli::monitor::Conn,
    grace: Duration,
) -> std::io::Result<Notes> {
    let socket = conn.cancellation_socket()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            let note = conn.note(std::time::Instant::now() + grace);
            let ended = note.is_err();
            if tx.send(note.map_err(|e| e.to_string())).is_err() || ended {
                return;
            }
        }
    });
    Ok(Notes { rx, socket })
}

pub(crate) struct Notes {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<crate::cli::monitor::Note, String>>,
    socket: std::os::unix::net::UnixStream,
}

impl Notes {
    pub(crate) async fn recv(&mut self) -> Option<Result<crate::cli::monitor::Note, String>> {
        self.rx.recv().await
    }
}

impl Drop for Notes {
    fn drop(&mut self) {
        // Cancelled selects close the connection and wake its blocked reader
        // immediately, so a replacement never waits for the monitor heartbeat.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

/// What `author` is owed at one fold, block decision included.
///
/// The monitor derives for every connected author from a SHARED fold,
/// and must take the same decision a park takes:
/// `present_with_blocks` is the one place a block's blast radius is
/// decided (`a-block-pauses-the-review-cycle-not-the-world`), so
/// sharing the fold must not become a way around it — a blocked agent
/// would start receiving review-cycle work again, which is the bug
/// that plan existed to fix.
pub(crate) fn owed_for(
    repo: &Path,
    author: &AgentLabel,
    inputs: &WaitInputs,
    status: &clank_core::wait::WorkStatus,
    fold: &clank_core::repo_state::RepoState,
    external: &mut Vec<WaitItem>,
) -> Vec<WaitItem> {
    // Deciding BEFORE the merge left the channel route outside the
    // decision: a blocked park returned a source item with no `Blocked`
    // context to explain the silence around it, and an answered one
    // returned the answer with the company it had just promised not to
    // have (codex on 344280f). The monitor has to come through the same
    // door for the same reason — a source event that woke an agent
    // straight off the channel would arrive without the block context
    // the local path gives it (codex on ca2ea69).
    present_with_blocks(check_blocks(repo, author), || {
        beat_owed(items_for(repo, author, inputs, status, fold), external)
    })
}

/// The merged extra-wake-source list (extra-wait-events): the agent
/// config's `wait_events` first, then every CLI `--event` item — the
/// SAME JSON shape, one grammar. Missing config → config sources are
/// simply none (an unbound label can still `--event`).
pub(crate) fn resolve_wait_event_sources(
    repo: &Path,
    author: &AgentLabel,
    cli_items: &[String],
) -> anyhow::Result<
    Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
> {
    use crate::cli::event_source::SourceOrigin;
    // Provenance rides the merge (github-watch-prompts, codex
    // 16ea191): who declared a source decides who stores its prompt.
    let mut sources: Vec<_> = crate::agent_store::load_agent_config(repo, author)?
        .map(|c| c.wait_events)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s, SourceOrigin::Config))
        .collect();
    for raw in cli_items {
        let item: clank_core::agent_config::WaitEventSource = serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("invalid --event `{raw}`: {e}"))?;
        sources.push((item, SourceOrigin::Cli));
    }
    // Validate at ARM time so a bad source fails LOUD here, rather
    // than silently substituting a default deep in a poll loop
    // (codex ef1861a) or printing from inside a task that then
    // returns, leaving the wait armed without the source the
    // operator configured (codex 072e459).
    let mut timer_names = std::collections::HashSet::new();
    let mut timer_keys = std::collections::HashSet::new();
    // One task owns a storage key's log and lease. Two sources resolving
    // to one key would be two tasks, and a host keyed by it keeps one.
    let mut producer_keys = std::collections::HashSet::new();
    for (source, origin) in &sources {
        let produced_into = match source {
            clank_core::agent_config::WaitEventSource::Command(c) => Some(c.source_key()),
            clank_core::agent_config::WaitEventSource::Github(g) => {
                Some(crate::cli::github::supervisor::source_key(g))
            }
            clank_core::agent_config::WaitEventSource::Timer(_) => None,
        };
        if let Some(key) = produced_into {
            anyhow::ensure!(
                producer_keys.insert((*origin, key.clone())),
                "two `{}` sources share the storage key `{key}`; give one a different name",
                source_name(source)
            );
        }
        match source {
            clank_core::agent_config::WaitEventSource::Timer(timer) => {
                crate::cli::timer_source::parse(timer)?;
                let scope = match origin {
                    SourceOrigin::Config => None,
                    SourceOrigin::Cli => Some("validation"),
                };
                anyhow::ensure!(
                    timer_names.insert((scope, timer.name.clone()))
                        && timer_keys.insert(timer.source_key(scope)),
                    "duplicate timer name or storage key `{}` in one scope",
                    timer.name
                );
            }
            clank_core::agent_config::WaitEventSource::Github(g) => {
                if let Some(why) = g.refusal() {
                    anyhow::bail!("{why}");
                }
                if let Some(pi) = g.poll_interval.as_deref() {
                    parse_duration_str(pi).map_err(|e| {
                        anyhow::anyhow!("invalid poll_interval `{pi}` for {}: {e}", g.repo)
                    })?;
                }
            }
            clank_core::agent_config::WaitEventSource::Command(c) => {
                if c.command.is_empty() {
                    anyhow::bail!(
                        "command source `{}` has an empty command: there is nothing to run",
                        c.name
                    );
                }
                if c.reply_command
                    .as_ref()
                    .is_some_and(|argv| argv.is_empty() || argv[0].trim().is_empty())
                {
                    anyhow::bail!("command source `{}` has an empty reply_command", c.name);
                }
                if let Some(why) = c.receipt_refusal() {
                    anyhow::bail!("{why}");
                }
                crate::cli::command_source::stop_grace(c)?;
                if c.receipt_command.is_some() && *origin == SourceOrigin::Cli {
                    anyhow::bail!(
                        "command source `{}` receipt_command is supported only for configured sources, not invocation-only --event sources",
                        c.name
                    );
                }
            }
        }
    }
    Ok(sources)
}

/// One beat's items from BOTH routes, merged before anything decides.
///
/// Named so the composition can be asserted without a watcher, and so
/// the loop reads as two steps: merge, then decide. The channel items
/// join `events` because that is their class: an event that arrived on
/// its own schedule, which a pending block has nothing to say about.
///
/// No dedupe here. The channel is the ONLY route an external row takes
/// — the projection does not read the WAL — so a row cannot arrive
/// twice in one beat.
fn beat_owed(mut owed: Owed, external: &mut Vec<WaitItem>) -> Owed {
    owed.events.append(external);
    owed
}

/// Drain every ready external item into `out`, flipping `open` to
/// false when the source channel has disconnected (all sources
/// finished) so the select branch that reads it stays disabled — the
/// fuse that prevents a closed channel from hot-looping the wait
/// (codex 1bbb61d). No-op once closed.
fn drain_external(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::Emission>,
    open: &mut bool,
    out: &mut Vec<WaitItem>,
) {
    if !*open {
        return;
    }
    loop {
        match rx.try_recv() {
            Ok((item, _)) => out.push(item),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                *open = false;
                break;
            }
        }
    }
}

/// What a source calls itself, for a message about it.
fn source_name(source: &clank_core::agent_config::WaitEventSource) -> String {
    match source {
        clank_core::agent_config::WaitEventSource::Command(c) => c.name.clone(),
        clank_core::agent_config::WaitEventSource::Github(g) => g.repo.clone(),
        clank_core::agent_config::WaitEventSource::Timer(t) => t.name.clone(),
    }
}

#[cfg(test)]
fn spawn_local(
    sources: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    events_dir: Option<&std::path::Path>,
    invocation: Option<&str>,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<crate::cli::event_source::Emission>,
    EventSources,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        rx,
        EventSources::spawn(sources, events_dir, invocation, tx.into()),
    )
}

/// Source tasks belong to the lifecycle owner, not the delivery queue.
/// Local waits shut down and join them on return; monitor hosts retain them
/// until retirement. Aborting a command source retires its producer group.
pub(crate) struct EventSources {
    /// Each source's task by the storage key of the log it writes, so a
    /// host can stop one and leave the rest running.
    tasks: Vec<(String, tokio::task::JoinHandle<()>)>,
}

impl EventSources {
    #[cfg(test)]
    pub(crate) fn into_tasks(
        mut self,
    ) -> std::collections::BTreeMap<String, tokio::task::JoinHandle<()>> {
        let mut tasks = std::collections::BTreeMap::new();
        for (key, task) in std::mem::take(&mut self.tasks) {
            // Resolution refuses two sources of one key; were one to get
            // here, its task is stopped rather than detached.
            if let Some(overwritten) = tasks.insert(key, task) {
                overwritten.abort();
            }
        }
        tasks
    }

    /// `events_dir` is the agent's github event-WAL home
    /// (`.clank/agents/<label>/events`) — `None` (ad-hoc/unbound
    /// waits) runs github sources memory-only, exactly the pre-WAL
    /// behavior (github-offline-catchup).
    pub(crate) fn spawn(
        sources: &[(
            clank_core::agent_config::WaitEventSource,
            crate::cli::event_source::SourceOrigin,
        )],
        events_dir: Option<&std::path::Path>,
        invocation: Option<&str>,
        tx: crate::cli::event_source::EmissionSink,
    ) -> Self {
        use crate::cli::event_source::SourceOrigin;
        let mut tasks = Vec::new();
        for (source, origin) in sources {
            // A `--event` source is ONE INVOCATION's, so it needs that
            // invocation to scope its durable state. A pooled host —
            // the monitor — has no invocation to give and must never be
            // handed one of these; refusing loudly beats sharing a log
            // and a lease with somebody else's source.
            let scope = match (origin, invocation) {
                (SourceOrigin::Cli, None) => {
                    eprintln!(
                        "wait: refusing to run an invocation's source `{}` without an \
                         invocation to scope it to",
                        source_name(source)
                    );
                    continue;
                }
                (_, scope) => scope.unwrap_or_default(),
            };
            if let Some(task) = spawn_source(source, *origin, events_dir, scope, tx.clone(), false)
            {
                tasks.push((task.key, task.handle));
            }
        }
        Self { tasks }
    }

    async fn shutdown(mut self) {
        for (_, task) in &self.tasks {
            task.abort();
        }
        for (_, task) in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for EventSources {
    fn drop(&mut self) {
        // A lifecycle owner may cancel the wait future itself. Dropping join
        // handles detaches tasks; aborting them preserves process-group cleanup.
        // Normal returns still use shutdown to abort AND join.
        for (_, task) in &self.tasks {
            task.abort();
        }
    }
}

/// The storage key of the log `source` writes; a timer writes none
/// that a task owns.
pub(crate) fn task_key(
    source: &clank_core::agent_config::WaitEventSource,
    origin: crate::cli::event_source::SourceOrigin,
    scope: &str,
) -> Option<String> {
    use clank_core::agent_config::WaitEventSource;
    let key = match source {
        WaitEventSource::Timer(_) => return None,
        WaitEventSource::Command(c) => c.source_key(),
        WaitEventSource::Github(g) => crate::cli::github::supervisor::source_key(g),
    };
    Some(crate::cli::command_source::storage_key(key, origin, scope))
}

/// One source's running task.
pub(crate) struct SourceTask {
    pub(crate) key: String,
    pub(crate) handle: tokio::task::JoinHandle<()>,
    /// Asks the source to end gracefully; one without it is cancelled.
    pub(crate) stop: Option<tokio::sync::watch::Sender<bool>>,
}

/// Start `source`'s task. A `stoppable` command source ends on request,
/// draining its producer within its grace; anything else only by
/// cancellation. Timers run no task.
pub(crate) fn spawn_source(
    source: &clank_core::agent_config::WaitEventSource,
    origin: crate::cli::event_source::SourceOrigin,
    events_dir: Option<&std::path::Path>,
    scope: &str,
    tx: crate::cli::event_source::EmissionSink,
    stoppable: bool,
) -> Option<SourceTask> {
    use clank_core::agent_config::WaitEventSource;
    let key = task_key(source, origin, scope)?;
    let events_dir = events_dir.map(std::path::Path::to_path_buf);
    let (handle, stop) = match source {
        WaitEventSource::Timer(_) => return None,
        WaitEventSource::Command(c) => {
            let (stop, request) = match stoppable {
                true => {
                    let (stop, request) = StopRequest::channel();
                    (Some(stop), request)
                }
                false => (None, StopRequest::never()),
            };
            (
                tokio::spawn(run_command_source(
                    c.clone(),
                    tx,
                    events_dir,
                    key.clone(),
                    request,
                )),
                stop,
            )
        }
        WaitEventSource::Github(g) => (
            tokio::spawn(crate::cli::github::supervisor::run_github_source(
                g.clone(),
                tx,
                events_dir,
                origin,
                key.clone(),
            )),
            None,
        ),
    };
    Some(SourceTask { key, handle, stop })
}

/// Kill the child's PROCESS GROUP on drop — grandchildren included.
/// A synchronous signal through the typed process boundary also works on
/// task abort, without starting a cleanup subprocess.
///
/// Holds a validated [`crate::signal::Pgid`] rather than a
/// bare number. `kill -9 -1` is every process this user may signal, and
/// this is a tuple struct anybody can write — `GroupKill(Some(1))` was
/// one keystroke from the same catastrophe `Pgid` exists to make
/// unrepresentable, so it is spelled in that type too.
pub(crate) struct GroupKill(pub(crate) Option<crate::signal::Pgid>);

impl GroupKill {
    /// The guard for a child, or none when there is no group to name.
    pub(crate) fn over(child: Option<u32>) -> Self {
        Self(child.and_then(|pid| crate::signal::Pgid::new(pid as i32)))
    }

    /// End an explicitly bounded invocation without launching another process.
    pub(crate) fn terminate(&mut self) {
        if let Some(pgid) = self.0.take() {
            crate::signal::signal_group(pgid, libc::SIGKILL);
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Last-`cap`-bytes ring for a command source's interleaved output —
/// bounded by construction, so a chatty child can't balloon memory.
pub(crate) struct RingTail {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl RingTail {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.buf.len() == self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(b);
        }
    }
    pub(crate) fn into_string(self) -> String {
        String::from_utf8_lossy(&Vec::from(self.buf)).into_owned()
    }
}

/// One command source: a supervised producer streaming JSONL.
///
/// The argv is spawned into a group `source_group` owns and expected to STAY
/// up, printing one JSON object per line on stdout; stderr is
/// diagnostics. `clank wait` aborts its sources when it returns, so a
/// restart is the normal path between arms and not an exception —
/// what makes that lossless is the backend's own state directory,
/// plus a dedup index that never forgets an id it has ingested.
///
/// Spawn failures degrade to a warning and a backoff, never the wait;
/// a source that cannot run becomes an EVENT once the streak passes
/// `FAILURES_BEFORE_SURFACING`, because an agent waiting on a
/// producer that will never start should find out.
async fn run_command_source(
    src: clank_core::agent_config::CommandSource,
    tx: crate::cli::event_source::EmissionSink,
    events_dir: Option<std::path::PathBuf>,
    key: String,
    stop: StopRequest,
) {
    run_command_source_with(
        src,
        tx,
        events_dir,
        key,
        crate::cli::command_source::reap_previous_producer,
        &crate::cli::source_group::OWNERS,
        stop,
    )
    .await
}

/// [`run_command_source`] with the retirement supplied.
///
/// The seam is the RETIREMENT and not the spawning, because the
/// decision under test is what the caller does with its outcome: a
/// deferral must withhold the launch, and a launch withheld is only
/// observable as a child that did not start. Driving a real deferral
/// would mean arranging a process this test cannot look at, which is
/// not something to arrange on a developer's machine.
pub(crate) async fn run_command_source_with(
    src: clank_core::agent_config::CommandSource,
    tx: crate::cli::event_source::EmissionSink,
    events_dir: Option<std::path::PathBuf>,
    key: String,
    retire: impl Fn(&std::path::Path, &str) -> crate::cli::command_source::Retired,
    owners: &'static crate::cli::source_group::Owners,
    mut stop: StopRequest,
) {
    use crate::cli::command_source as cs;

    // `resolve_wait_event_sources` refuses empty commands at ARM time,
    // so reaching this means a caller that built sources another way.
    // Kept as the last line of defence, not as the diagnosis: an
    // operator learns about a bad source when the wait refuses to
    // arm, not from a line in a log nobody reads.
    if src.command.is_empty() {
        eprintln!("wait: command source `{}` has an empty argv", src.name);
        tx.report(
            &key,
            crate::cli::event_source::Lifecycle::Stopped {
                why: "its command is empty".to_string(),
            },
        );
        return;
    }
    // No events dir (an ad-hoc wait) means no durable dedup: the
    // source still runs, it just cannot remember. Same degradation
    // the github sources take, in a temp dir that dies with the wait.
    let scratch = events_dir.is_none().then(tempfile::tempdir);
    let store_dir = match (&events_dir, &scratch) {
        (Some(d), _) => d.clone(),
        (None, Some(Ok(tmp))) => tmp.path().to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };

    // ONE writer per source, and everyone else TAILS — the same
    // lifecycle `github/supervisor.rs` runs, for the same reasons.
    // Two waits would otherwise each spawn the user's command and
    // each allocate sequences from their own handle, so two events
    // would land on one seq and a single ack would cover both (codex
    // 43f5ed8). But a contender that simply RETURNED was its own
    // bug: nothing else tails command rows for that wait, so it
    // received neither the pending backlog nor anything the holder
    // logged afterwards, and it never took over when the holder left
    // (codex 072e459).
    //
    // Carried across the phases: what THIS process has already put in
    // front of its agent. A takeover must not re-present the rows it
    // presented while tailing.
    let mut presented: std::collections::HashSet<(u64, String)> = Default::default();
    let mut warned_tail = false;
    loop {
        match crate::cli::github::log::IngestLease::try_acquire(&store_dir, &key) {
            Ok(Some(lease)) => {
                run_leased_command_source(
                    &src,
                    &tx,
                    &store_dir,
                    &key,
                    &mut presented,
                    events_dir,
                    &retire,
                    std::sync::Arc::new(lease),
                    owners,
                    &mut stop,
                )
                .await;
                return;
            }
            Ok(None) => {
                if !warned_tail {
                    tx.report(&key, crate::cli::event_source::Lifecycle::Following);
                    eprintln!(
                        "wait: command source `{}` — another wait is already running this \
                         source for this agent; tailing its event log instead",
                        src.name
                    );
                    warned_tail = true;
                }
                // The holder may be this process, whose group deferred
                // its retirement and so still holds the lease.
                owners.retry_deferred(&cs::producer_path(&store_dir, &key));
                // `read_rows` is the read-only inspector: a tailer
                // must never quarantine a log it does not own.
                if let Ok(log) = crate::cli::github::log::EventLog::open(&store_dir, &key)
                    && let Ok(Some(read)) = log.read_rows()
                {
                    for row in read.rows {
                        if row.acked || !presented.insert((row.seq, row.ident.clone())) {
                            continue;
                        }
                        let mut item = row.item;
                        item.set_presentation(clank_core::wait::EventPresentation {
                            source: key.clone(),
                            seq: row.seq,
                            ident: row.ident,
                        });
                        cs::stamp_selector(&mut item, &key, row.seq);
                        if tx
                            .send((item, crate::cli::event_source::Durability::Logged))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                if pause_unless_stopped(&mut stop, cs::TAIL_INTERVAL).await {
                    return;
                }
            }
            Err(e) => {
                eprintln!(
                    "wait: command source `{}` cannot take its ingest lease: {e}",
                    src.name
                );
                tx.report(
                    &key,
                    crate::cli::event_source::Lifecycle::Stopped {
                        why: format!("cannot take its ingest lease: {e}"),
                    },
                );
                return;
            }
        }
    }
}

/// A retirement blocks for as long as its group's policy says, which
/// for a group asked to end is up to its grace: not on a runtime worker.
async fn retire_off_the_runtime(
    owned: crate::cli::source_group::Owned,
) -> Result<(), (crate::cli::source_group::Owned, String)> {
    settled(tokio::task::spawn_blocking(move || owned.retire())).await
}

async fn settled(
    retiring: tokio::task::JoinHandle<Result<(), (crate::cli::source_group::Owned, String)>>,
) -> Result<(), (crate::cli::source_group::Owned, String)> {
    retiring
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

/// A host's request that a command source end: its producer is asked
/// to end and given its grace while its output is still read, and the
/// source starts nothing more.
pub(crate) struct StopRequest(Option<tokio::sync::watch::Receiver<bool>>);

impl StopRequest {
    /// For a host that only ever cancels its sources outright.
    pub(crate) fn never() -> Self {
        Self(None)
    }

    /// The sender asks for the stop; dropping it asks too, since nobody
    /// is left to stop the source otherwise.
    pub(crate) fn channel() -> (tokio::sync::watch::Sender<bool>, Self) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (tx, Self(Some(rx)))
    }

    fn is_requested(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|rx| rx.has_changed().is_err() || *rx.borrow())
    }

    async fn requested(&mut self) {
        match &mut self.0 {
            None => std::future::pending().await,
            Some(rx) => {
                let _ = rx.wait_for(|stop| *stop).await;
            }
        }
    }
}

/// Sleep for `d`, unless a stop is asked for first. Whether one was.
async fn pause_unless_stopped(stop: &mut StopRequest, d: Duration) -> bool {
    tokio::select! {
        _ = stop.requested() => true,
        _ = tokio::time::sleep(d) => stop.is_requested(),
    }
}

/// A launched producer's group, until its retirement settles.
enum Run {
    Owned(crate::cli::source_group::Owned),
    /// Asked to end, and retiring beside the reader, which keeps taking
    /// the group's output until the group is gone or its grace is out.
    Stopping {
        retiring: tokio::task::JoinHandle<Result<(), (crate::cli::source_group::Owned, String)>>,
        by: tokio::time::Instant,
    },
}

impl Run {
    fn stopped(self, grace: Duration) -> Self {
        match self {
            Run::Owned(owned) => {
                owned.ask_to_end();
                Run::Stopping {
                    retiring: tokio::task::spawn_blocking(move || owned.retire()),
                    by: tokio::time::Instant::now() + grace,
                }
            }
            stopping => stopping,
        }
    }

    fn is_stopping(&self) -> bool {
        matches!(self, Run::Stopping { .. })
    }

    fn stopping_by(&self) -> Option<tokio::time::Instant> {
        match self {
            Run::Owned(_) => None,
            Run::Stopping { by, .. } => Some(*by),
        }
    }

    async fn settle(self) -> Result<(), (crate::cli::source_group::Owned, String)> {
        match self {
            Run::Owned(owned) => retire_off_the_runtime(owned).await,
            Run::Stopping { retiring, .. } => settled(retiring).await,
        }
    }
}

/// Give up a group whose retirement could not confirm it ended, after
/// one more attempt: if that defers too, the registry keeps it, lease
/// and all, for whoever takes the source next.
async fn leave_unended(owned: crate::cli::source_group::Owned) {
    let _ = tokio::task::spawn_blocking(move || drop(owned)).await;
}

/// The LEASE HOLDER's runtime: it alone spawns the producer and
/// writes the log.
#[allow(clippy::too_many_arguments)]
async fn run_leased_command_source(
    src: &clank_core::agent_config::CommandSource,
    tx: &crate::cli::event_source::EmissionSink,
    store_dir: &std::path::Path,
    key: &str,
    presented: &mut std::collections::HashSet<(u64, String)>,
    events_dir: Option<std::path::PathBuf>,
    retire: impl Fn(&std::path::Path, &str) -> crate::cli::command_source::Retired,
    lease: std::sync::Arc<crate::cli::github::log::IngestLease>,
    owners: &'static crate::cli::source_group::Owners,
    stop: &mut StopRequest,
) {
    use crate::cli::command_source as cs;
    use crate::cli::source_group;
    use tokio::io::AsyncBufReadExt;

    let src = src.clone();
    let grace = cs::stop_grace(&src).unwrap_or(cs::DEFAULT_STOP_GRACE);
    let store_dir = store_dir.to_path_buf();
    let key = key.to_string();
    let mut seen = cs::Seen::at(&store_dir, &key);
    // The SAME log github writes, so `clank events list/show/ack`
    // finds a command event by the same directory glob and an agent
    // has a supported way to finish handling one.
    let mut log = match crate::cli::github::log::EventLog::open(&store_dir, &key) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wait: command source `{}` cannot open its log: {e}",
                src.name
            );
            tx.report(
                &key,
                crate::cli::event_source::Lifecycle::Stopped {
                    why: format!("cannot open its log: {e}"),
                },
            );
            return;
        }
    };
    // The backend's OWN directory, which clank creates and never
    // reads. Whatever cursor or cache it keeps lives here — the
    // counterpart of clank no longer holding a resume point for it.
    let state_dir = events_dir
        .as_deref()
        .and_then(|d| d.parent())
        .map(|agent| cs::backend_state_dir(agent, &key));
    let reply_route =
        src.reply_command
            .as_ref()
            .and_then(|command| match std::env::current_dir() {
                Ok(cwd) => Some(clank_core::wait::CommandReplyRoute {
                    command: command.clone(),
                    cwd,
                    state_dir: state_dir.clone(),
                }),
                Err(e) => {
                    eprintln!(
                        "wait: source `{}` cannot capture reply routing: {e}",
                        src.name
                    );
                    None
                }
            });
    if let Some(dir) = state_dir.as_deref() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "wait: command source `{}` state dir {}: {e}",
                src.name,
                dir.display()
            );
        }
    }
    // LOAD, not read: this is what restores the sequence allocator
    // from the persisted records. `EventLog::open` starts it at 1,
    // and a supervisor that only ever read rows re-issued seq 1 on
    // every arm — two distinct events sharing a selector, with one
    // plain ack marking both handled (codex 43f5ed8). It is also the
    // path that quarantines a corrupt log, so the warning below can
    // now be fulfilled by the arm that prints it.
    let loaded = log.load();
    if loaded.log_quarantined {
        eprintln!(
            "wait: command source `{}` had a corrupt log — quarantined, starting fresh",
            src.name
        );
        // The index is DERIVED from the log. Quarantine moves the
        // log aside, so nothing is accounted for any more and every
        // id must be new again — otherwise a surviving `.seen` entry
        // suppresses the producer's replay of a pending event the
        // inbox no longer holds, and that event is unrecoverable
        // (codex 072e459).
        if let Err(e) = seen.forget_all() {
            eprintln!(
                "wait: command source `{}` could not clear its id index after \
                 quarantine ({e}); a replay may be refused for an event the log no \
                 longer has",
                src.name
            );
        }
    }
    // The WAL is AUTHORITATIVE for what has been ingested; the index
    // is derived. An id the log still vouches for has an event, so
    // recovering them repairs an index that a crash between the
    // append and its write left incomplete — without minting a
    // second event for an id the log already accounts for.
    seen.recover(loaded.feed_ids);
    // RE-PRESENT what is still owed, before the producer starts.
    // This is the other half of at-least-once: a stored event stays
    // the agent's until it is acked, and a wake that returned without
    // an ack must offer it again.
    for entry in loaded.unhandled {
        // A takeover has already presented some of these while it was
        // tailing; re-presenting is for a NEW arm, not a new phase of
        // the same one.
        if !presented.insert((entry.seq, entry.ident.clone())) {
            continue;
        }
        let mut item = entry.item;
        item.set_presentation(clank_core::wait::EventPresentation {
            source: key.clone(),
            seq: entry.seq,
            ident: entry.ident,
        });
        cs::stamp_selector(&mut item, &key, entry.seq);
        if tx
            .send((item, crate::cli::event_source::Durability::Logged))
            .is_err()
        {
            return;
        }
    }

    let mut failures: u32 = 0;
    let secrets = std::sync::Arc::new(crate::cli::source_diagnostic::host_secrets());

    source_group::retire_on_termination();
    let mut deferral_warned = false;
    // This host's own group, when its retirement could not confirm it
    // ended. Still owned, so nothing replaces it until it does.
    let mut unended: Option<source_group::Owned> = None;
    loop {
        // Closing is terminal: whatever the last pass did, nothing more
        // is started, retried or counted. A group still owned goes to the
        // shutdown that closed the registry, with its lease.
        if owners.closing() {
            return;
        }
        if stop.is_requested() {
            if let Some(owned) = unended.take() {
                leave_unended(owned).await;
            }
            return;
        }
        if let Some(owned) = unended.take()
            && let Err((owned, why)) = retire_off_the_runtime(owned).await
        {
            if !deferral_warned {
                eprintln!(
                    "wait: command source `{}` — {why}; not starting another until it is \
                     settled",
                    src.name
                );
                deferral_warned = true;
            }
            unended = Some(owned);
            pause_unless_stopped(stop, cs::TAIL_INTERVAL).await;
            continue;
        }
        // Holding the lease means the previous owner is GONE — a lease
        // lives as long as its process. What can survive it is the
        // CHILD, and starting a second one beside the first is how a
        // SIGKILLed owner's command sources accumulate.
        //
        // A launch may only follow a CONFIRMED retirement. Retrying
        // here rather than once before the loop is what makes a
        // deferral recoverable: the backlog above has already been
        // presented and the log keeps being read, so an agent still
        // gets its unacked rows while the launch waits.
        if let cs::Retired::Deferred(why) = retire(&store_dir, &key) {
            if !deferral_warned {
                tx.report(
                    &key,
                    crate::cli::event_source::Lifecycle::Settling { why: why.clone() },
                );
                eprintln!(
                    "wait: command source `{}` — {why}; not starting another until it is \
                     settled",
                    src.name
                );
                deferral_warned = true;
            }
            pause_unless_stopped(stop, cs::TAIL_INTERVAL).await;
            continue;
        }
        deferral_warned = false;

        let started = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&src.command[0]);
        cmd.args(&src.command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own directory, for whatever it needs to remember.
        match state_dir.as_deref() {
            Some(dir) => cmd.env("CLANK_SOURCE_STATE_DIR", dir),
            None => cmd.env_remove("CLANK_SOURCE_STATE_DIR"),
        };
        let mut launch_error = None;
        let launched = match source_group::launch(
            owners,
            &source_group::Arming::standard(),
            &store_dir,
            &key,
            lease.clone(),
            grace,
            &mut cmd,
        )
        .await
        {
            Ok(launched) => Some(launched),
            // This process is draining or exiting: nothing more may start,
            // and that is not the source failing.
            Err(source_group::LaunchError::Closed) => return,
            Err(e) => {
                eprintln!("wait: command source `{}` did not start: {e}", src.name);
                launch_error = Some(format!("{e:#}"));
                None
            }
        };
        let mut exit = None;
        let mut ended = match &launch_error {
            Some(why) => crate::cli::source_diagnostic::Ended::Launch(why.clone()),
            None => crate::cli::source_diagnostic::Ended::Unknown,
        };
        let tail = std::sync::Arc::new(std::sync::Mutex::new(
            crate::cli::source_diagnostic::Tail::default(),
        ));
        if let Some((owned, mut child)) = launched {
            let mut run = Run::Owned(owned);
            if let Some(pid) = child.id() {
                tx.report(
                    &key,
                    crate::cli::event_source::Lifecycle::Running {
                        pid,
                        since: crate::cli::github::log::now_epoch(),
                    },
                );
            }
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let reading = owners.reading(cs::producer_path(&store_dir, &key));
            // Diagnostics, not events: kept as this run's bounded tail for
            // its failure event, and echoed only to a person watching a
            // terminal. A piped wait's parent collects all of its stderr,
            // so an echo there would grow without bound.
            let stderr_reader = stderr.map(|err| {
                use std::io::IsTerminal;
                let echo = std::io::stderr()
                    .is_terminal()
                    .then(crate::cli::source_diagnostic::NonBlocking::terminal)
                    .flatten()
                    .map(|out| (src.name.clone(), out));
                crate::cli::source_diagnostic::Reader::spawn(crate::cli::source_diagnostic::drain(
                    err,
                    tail.clone(),
                    echo,
                    secrets.clone(),
                ))
            });
            // One cutoff for the run's output once its root exits, shared
            // by stdout and stderr: neither gets a fresh window after the
            // other's.
            let mut cutoff: Option<tokio::time::Instant> = None;
            if let Some(out) = stdout {
                let mut lines = tokio::io::BufReader::new(out).lines();
                // The root's EXIT ends a run, not the pipe's EOF. A
                // descendant that inherited stdout holds the pipe open
                // past the root's death, and waiting for EOF would
                // leave a dead producer un-restarted for as long as
                // that descendant lives (codex fc7a4ff, whose test
                // caught this the second time round).
                loop {
                    let raw = tokio::select! {
                        _ = stop.requested(), if !run.is_stopping() => {
                            run = run.stopped(grace);
                            continue;
                        }
                        line = lines.next_line() => match line {
                            Ok(Some(raw)) => raw,
                            _ => break,
                        },
                        _ = child.wait() => {
                            // Best-effort: take whatever is already
                            // buffered, then stop. Bounded, so a
                            // chatty descendant cannot hold the run
                            // open by writing; but during a drain a
                            // descendant still at work is owed its
                            // grace, output included, and the group
                            // is ended at the cutoff anyway.
                            let by = output_cutoff(owners, run.stopping_by());
                            cutoff = Some(by);
                            let _ = tokio::time::timeout_at(
                                by,
                                async {
                                    while let Ok(Some(raw)) = lines.next_line().await {
                                        if let Some(what) =
                                            cs::ingest_line_with_route(&src, &seen, &raw, reply_route.as_ref())
                                        {
                                            if !cs::apply(
                                                what,
                                                &key,
                                                &mut log,
                                                &mut seen,
                                                |item| tx.send((item, crate::cli::event_source::Durability::Logged)).is_ok(),
                                            ) {
                                                break;
                                            }
                                        }
                                    }
                                },
                            )
                            .await;
                            break;
                        }
                    };
                    let Some(what) =
                        cs::ingest_line_with_route(&src, &seen, &raw, reply_route.as_ref())
                    else {
                        continue;
                    };
                    if let cs::Ingested::Refuse(why) = &what {
                        eprintln!("wait: command source `{}` refused a line: {why}", src.name);
                    }
                    // The payload-before-checkpoint ordering lives in
                    // `apply`, where a test can inject a sink that
                    // fails and watch the resume point NOT move. A
                    // second copy of that ordering here would be a
                    // second place for it to be wrong.
                    if !cs::apply(what, &key, &mut log, &mut seen, |item| {
                        // `apply` only reaches its send after the
                        // append RETURNED a seq, so this is logged.
                        tx.send((item, crate::cli::event_source::Durability::Logged))
                            .is_ok()
                    }) {
                        return;
                    }
                }
            }
            drop(reading);
            // A producer can close its stdout and live on: the stop still
            // has to reach it.
            let status = loop {
                tokio::select! {
                    _ = stop.requested(), if !run.is_stopping() => run = run.stopped(grace),
                    status = child.wait() => break status.ok(),
                }
            };
            exit = status.and_then(|status| status.code());
            ended = crate::cli::source_diagnostic::Ended::of(status);
            if let Some(reader) = stderr_reader {
                let by = cutoff.unwrap_or_else(|| output_cutoff(owners, run.stopping_by()));
                reader.finish_by(by).await;
            }
            if stop.is_requested() {
                run = run.stopped(grace);
            }
            // The command's end is not its group's: a descendant it
            // left, and the anchor, are still there.
            if let Err((owned, why)) = run.settle().await {
                eprintln!(
                    "wait: command source `{}` — {why}; not starting another until it is settled",
                    src.name
                );
                unended = Some(owned);
            }
        }
        if stop.is_requested() {
            if let Some(owned) = unended.take() {
                leave_unended(owned).await;
            }
            return;
        }
        // A run, or a launch, that the registry's closing interrupted did
        // not fail, and there is nothing to restart it into.
        if owners.closing() {
            return;
        }
        // A run long enough to count as healthy clears the streak, so
        // a producer restarted nightly does not creep to the cap.
        if started.elapsed() >= cs::HEALTHY_RUN {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        if failures == cs::FAILURES_BEFORE_SURFACING {
            let rendered = tail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&secrets);
            let live = cs::report_failure(
                &src,
                &key,
                &mut log,
                &mut seen,
                &crate::cli::source_diagnostic::failure_text(
                    &src.command,
                    failures,
                    &ended,
                    &rendered,
                    &secrets,
                ),
                |item| {
                    tx.send((item, crate::cli::event_source::Durability::Logged))
                        .is_ok()
                },
            );
            if !live {
                return;
            }
        }
        let backoff = cs::backoff(failures);
        tx.report(
            &key,
            crate::cli::event_source::Lifecycle::BackingOff {
                until: crate::cli::github::log::now_epoch() + backoff.as_secs(),
                failures,
                exit,
                error: launch_error,
            },
        );
        pause_unless_stopped(stop, backoff).await;
    }
}

/// How long a run's output is still read once its root has exited: a
/// moment, unless its group was asked to end, when work it left may
/// still finish and print within its grace or a drain's cutoff.
fn output_cutoff(
    owners: &crate::cli::source_group::Owners,
    stopping_by: Option<tokio::time::Instant>,
) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    let drain = owners
        .drain_cutoff()
        .map(|cutoff| now + cutoff.saturating_duration_since(std::time::Instant::now()));
    drain
        .into_iter()
        .chain(stopping_by)
        .min()
        .unwrap_or(now + Duration::from_millis(100))
}

/// Check blocks for the calling agent. An `Unblocked` answer is
/// returned as the sole item so the caller exits immediately; any
/// pending block sets `suppress_all`, which pauses this agent's REVIEW
/// CYCLE until it is answered. Blocks stay repo-wide — there is no
/// per-plan bookkeeping — but the suppression is partial by class:
/// [`present_with_blocks`] withholds the review cycle and still
/// delivers events. It used to withhold everything, which turned a
/// question for a human into an agent that could not be reached at all
/// (`a-block-pauses-the-review-cycle-not-the-world`).
struct BlockResult {
    items: Vec<WaitItem>,
    suppress_all: bool,
}

/// What `author`'s blocks contribute to a wake, as the monitor derives it.
#[cfg(test)]
pub(crate) fn block_items(repo: &Path, author: &AgentLabel) -> Vec<WaitItem> {
    check_blocks(repo, author).items
}

/// Whether a block is pending for this agent: a question for a person
/// that no automatic action may talk over.
pub(crate) fn blocked(repo: &Path, author: &AgentLabel) -> bool {
    check_blocks(repo, author).suppress_all
}

fn check_blocks(repo: &Path, author: &AgentLabel) -> BlockResult {
    if let Some(owed) = crate::cli::block::owed_answers(repo, author)
        .into_iter()
        .next()
    {
        return BlockResult {
            items: vec![WaitItem::Unblocked {
                answer: owed.answer,
                carries: owed.carries,
            }],
            suppress_all: true,
        };
    }
    let mut result = BlockResult {
        items: Vec::new(),
        suppress_all: false,
    };

    for b in &scan_blocks(repo) {
        if b.answer.is_some() {
            continue;
        }
        result.items.push(WaitItem::Blocked {
            agent: b.agent.clone(),
            name: b.name.clone(),
            question: b.question.clone(),
        });
        // Every block is repo-wide: one pending block suppresses for
        // every plan, so there is no per-plan bookkeeping. WHAT it
        // suppresses is the review CYCLE — see `present_with_blocks`,
        // the only reader of this flag.
        result.suppress_all = true;
    }

    result
}

/// Everything this agent is woken for at one fold: its repo work, the
/// master's notices, the queue when the master has nothing active, and
/// its pending messages. The initial pass and every refold ask this one
/// function, so a message reaches the agent whichever path returns —
/// including `--peek`, which is the initial pass.
/// What a turn is owed, split by PROVENANCE — which is what decides
/// whether a pending block withholds it.
///
/// Split by where an item came FROM rather than by a predicate over
/// `WaitItem`, because provenance is the actual invariant and it is
/// checkable: `work_for` and the queue are the review cycle by
/// definition, and the message log and the external WALs are events
/// that arrived on their own schedule. A new
/// producer has to name a field to compile; a new item added to an
/// existing producer inherits that producer's side, which is the
/// answer and not an accident.
struct Owed {
    /// The cycle a block exists to pause.
    review: Vec<WaitItem>,
    /// Debt that accrued regardless of the question a block is asking.
    events: Vec<WaitItem>,
}

fn items_for(
    repo: &Path,
    author: &AgentLabel,
    inputs: &WaitInputs,
    status: &clank_core::wait::WorkStatus,
    fold: &clank_core::repo_state::RepoState,
) -> Owed {
    let mut review = status.work_for(author, inputs.role);
    if inputs.role == Role::Master && review.is_empty() && fold.active_plan.is_none() {
        let queue = crate::cli::queue::scan_queue_no_dups(repo).unwrap_or_else(|e| {
            eprintln!("wait: {e}");
            Vec::new()
        });
        review.extend(queue_promote_outcome(&queue));
    }
    let events = message_items(repo, author);
    Owed { review, events }
}

/// What this turn presents, with any pending block applied.
///
/// THE one place that decides what a block stops. Both callers — the
/// initial pass and the watcher loop — used to decide it here and
/// there, and each did it by skipping the whole projection wholesale.
/// That is how a block came to stop things it has nothing to say
/// about: a `clank send` message is reached only through `items_for`,
/// so a human answering the very question the agent is blocked on
/// could not reach it.
fn present_with_blocks(br: BlockResult, owed: impl FnOnce() -> Owed) -> Vec<WaitItem> {
    let Owed { review, mut events } = owed();
    if !br.suppress_all {
        let mut items = review;
        items.extend(events);
        return items;
    }
    // A block pauses the REVIEW CYCLE, and says nothing about debt
    // that accrued on its own schedule. Withholding that debt does not
    // discard it — the rows stay unacked and drain later — it only
    // defers the wake behind a human answering a question, and the
    // events whose whole value is timeliness are exactly the ones
    // deferred: a CI failure, a review submitted, a reply waited on.
    //
    // The control items LEAD, as context: an agent woken by a message
    // while its cycle is paused would otherwise have no way to know
    // why its plan work went quiet.
    //
    // Whether they may wake on their OWN is the one place a pending
    // block and an answered one differ. A pending block is not news —
    // only a human can discharge it — so presenting it alone would wake
    // the agent at every turn end until one did. An ANSWER is news, and
    // is the thing the agent has been waiting for, so it wakes with
    // nothing else ready.
    let answered = br
        .items
        .iter()
        .any(|i| matches!(i, WaitItem::Unblocked { .. }));
    if !answered && events.is_empty() {
        return Vec::new();
    }
    let mut items = br.items;
    items.append(&mut events);
    items
}

/// This agent's unacked messages, in the order they wake. A message is
/// not repo work — a pending one never hides the queue from the master.
///
/// Read from the agent's message LOG, not from a directory of files:
/// a message is an event like any other, so it is re-presented until it
/// is acked rather than until a file is deleted.
fn message_items(repo: &Path, author: &AgentLabel) -> Vec<WaitItem> {
    crate::inbox::pending(repo, author)
}

/// The master-with-nothing-active outcome: a `PromoteFromQueue` for
/// the first queued item, or empty when the queue is empty.
///
/// It is REVIEW-CYCLE work, and now that a block no longer skips
/// `items_for` wholesale this function does run while one pends — its
/// result is dropped by [`present_with_blocks`] instead. Starting a new
/// plan is precisely what a pending question should stop, so the
/// suppression lives there, with every other review item, rather than
/// being re-decided here.
fn queue_promote_outcome(queue: &[crate::cli::queue::QueueEntry]) -> Vec<WaitItem> {
    queue
        .first()
        .map(|first| {
            vec![WaitItem::PromoteFromQueue {
                name: first.name.clone(),
                priority: first.priority,
            }]
        })
        .unwrap_or_default()
}

/// `clank wait --json` envelope. The stop-hook parses this; the
/// contract is keys + values (not key order). Typed in place of the
/// former ad-hoc `json!` (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct WaitEnvelope<'a> {
    items: Vec<WaitJsonItem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_cwd: Option<&'a Path>,
}

/// A control result, not an empty successful wait. The hook must
/// suppress this obsolete incarnation rather than call it idle.
#[derive(serde::Serialize)]
struct SupersededEnvelope {
    items: [(); 0],
    superseded: bool,
}

pub(crate) fn emit(items: &[WaitItem], json: bool) {
    emit_with_context(items, json, None);
}
fn emit_with_context(items: &[WaitItem], json: bool, source_cwd: Option<&Path>) {
    if let Err(error) = emit_checked(items, json, source_cwd, &mut std::io::stdout()) {
        eprintln!("wait: could not write work: {error}");
    }
}
pub(crate) fn emit_checked(
    items: &[WaitItem],
    json: bool,
    source_cwd: Option<&Path>,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    if json {
        let envelope = WaitEnvelope {
            items: items.iter().map(render_json).collect(),
            source_cwd,
        };
        serde_json::to_writer(&mut *writer, &envelope).map_err(std::io::Error::other)?;
        writeln!(writer)?;
    } else {
        for item in items {
            writeln!(writer, "{}", render_human(item))?;
        }
    }
    writer.flush()
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
pub(crate) enum WaitJsonItem<'a> {
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
        unknown: Option<&'a str>,
        untagged_touched: Vec<&'a str>,
        extra_named: Option<&'a str>,
    },
    #[serde(rename = "promote_from_queue")]
    PromoteFromQueue { name: &'a str, priority: u16 },
    #[serde(rename = "message")]
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<&'a clank_core::wait::EventPresentation>,
        handle: &'a str,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        requires_reply: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_route: Option<&'a clank_core::wait::MessageReplyRoute>,
        /// The whole message. The wake shows its first line; an agent
        /// that wants the rest has it here rather than in a file.
        body: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<&'a str>,
        subject: &'a str,
    },
    #[serde(rename = "github_event")]
    GithubEvent {
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<&'a clank_core::wait::EventPresentation>,
        repo: &'a str,
        event: &'a str,
        detail: Option<&'a str>,
        number: Option<u64>,
        title: Option<&'a str>,
        actor: Option<&'a str>,
        url: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<&'a str>,
    },
    #[serde(rename = "timer_event")]
    TimerEvent {
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<&'a clank_core::wait::EventPresentation>,
        name: &'a str,
        id: &'a str,
        due_at: u64,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        recurrence: Option<&'a clank_core::agent_config::TimerRecurrence>,
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<&'a str>,
    },
    #[serde(rename = "command_event")]
    CommandEvent {
        #[serde(skip_serializing_if = "Option::is_none")]
        presentation: Option<&'a clank_core::wait::EventPresentation>,
        name: &'a str,
        id: &'a str,
        event: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<u64>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        failure: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        requires_reply: bool,
        /// What to pass `clank events ack`. Absent only when the
        /// event was never logged, and then there is nothing to ack.
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<&'a str>,
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
        question: &'a str,
    },
    #[serde(rename = "unblocked")]
    Unblocked {
        name: &'a str,
        answer: &'a str,
        carries: &'a clank_core::wait::AnswerRef,
    },
}

fn plan_path(plan: &PlanKey) -> String {
    crate::init_facts::plan_md_rel(plan.as_str())
}

pub(crate) fn render_json(item: &WaitItem) -> WaitJsonItem<'_> {
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
        WaitItem::Idle { prompt } => WaitJsonItem::Idle { prompt },
        WaitItem::AdHocReview { sha, feedback_path } => WaitJsonItem::AdHocReview {
            sha: sha.as_str(),
            feedback_path,
        },
        WaitItem::AdHocRevise { sha } => WaitJsonItem::AdHocRevise { sha: sha.as_str() },
        WaitItem::FixCommitTag { sha, violation } => WaitJsonItem::FixCommitTag {
            sha: sha.as_str(),
            unknown: violation.unknown.as_deref(),
            untagged_touched: violation
                .untagged_touched
                .iter()
                .map(|p| p.as_str())
                .collect(),
            extra_named: violation.extra_named.as_ref().map(|p| p.as_str()),
        },
        WaitItem::PromoteFromQueue { name, priority } => WaitJsonItem::PromoteFromQueue {
            name,
            priority: *priority,
        },
        WaitItem::Message {
            presentation: _,
            handle,
            from,
            repo,
            subject,
            body,
            selector,
            requires_reply,
            reply_route,
        } => WaitJsonItem::Message {
            presentation: item.presentation(),
            handle,
            requires_reply: *requires_reply,
            reply_route: reply_route.as_ref(),
            from: from.as_deref(),
            repo: repo.as_deref(),
            subject,
            body,
            selector: selector.as_deref(),
        },
        WaitItem::GithubEvent {
            presentation: _,
            repo,
            event,
            detail,
            number,
            title,
            actor,
            url,
            instructions,
            content: _,
        } => WaitJsonItem::GithubEvent {
            presentation: item.presentation(),
            repo,
            event,
            detail: detail.as_deref(),
            number: *number,
            title: title.as_deref(),
            actor: actor.as_deref(),
            url: url.as_deref(),
            instructions: instructions.as_deref(),
        },
        WaitItem::TimerEvent {
            presentation: _,
            name,
            id,
            due_at,
            content,
            recurrence,
            selector,
        } => WaitJsonItem::TimerEvent {
            presentation: item.presentation(),
            name,
            id,
            due_at: *due_at,
            content,
            recurrence: recurrence.as_ref(),
            selector: selector.as_deref(),
        },
        WaitItem::CommandEvent {
            name,
            id,
            event,
            content,
            at,
            failure,
            requires_reply,
            selector,
            ..
        } => WaitJsonItem::CommandEvent {
            presentation: item.presentation(),
            name,
            id,
            event,
            content,
            at: *at,
            failure: *failure,
            requires_reply: *requires_reply,
            selector: selector.as_deref(),
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
            question,
        } => WaitJsonItem::Blocked {
            agent,
            name,
            question,
        },
        WaitItem::Unblocked { answer, carries } => WaitJsonItem::Unblocked {
            name: &carries.name,
            answer,
            carries,
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
            if let Some(extra) = &violation.extra_named {
                parts.push(format!("tagged but not touched: {}", extra.as_str()));
            }
            if let Some(unknown) = &violation.unknown {
                parts.push(format!("names no active plan: {unknown}"));
            }
            format!(
                "fix-commit-tag  {}  {} — fix the single [plan] tag to match the plan \
                 file it touches: split unrelated plan-file changes, re-tag, or REMOVE \
                 the tag if the commit is ad-hoc (not plan work)",
                short(sha),
                parts.join("; ")
            )
        }
        WaitItem::PromoteFromQueue { name, priority } => {
            format!("promote  {name}  (priority {priority:03})")
        }
        WaitItem::Message {
            handle,
            from,
            repo,
            subject,
            ..
        } => format!(
            "message  {handle}  {}  {subject}",
            crate::inbox::sender_words(from.as_deref(), repo.as_deref())
        ),
        // ONE arm for every event source. The github and command
        // variants had separate arms building the same line from
        // differently-named fields; a third backend needed a third
        // (a-command-is-an-event-source-like-any-other).
        e @ (WaitItem::GithubEvent { .. }
        | WaitItem::CommandEvent { .. }
        | WaitItem::TimerEvent { .. }) => {
            let Some(v) = e.event_view() else {
                unreachable!("every event source projects")
            };
            // A source reporting ITSELF broken reads differently from
            // something it observed: the agent is not being told about
            // the world, it is being told the watch is not working.
            let lead = if v.failure {
                "source!".to_string()
            } else if v.github.is_some() {
                "github  ".to_string()
            } else if matches!(e, WaitItem::TimerEvent { .. }) {
                "timer   ".to_string()
            } else {
                "command ".to_string()
            };
            let detail = v
                .github
                .as_ref()
                .and_then(|g| g.detail)
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            // Segments joined uniformly so absent optionals leave no
            // dangling separators.
            let source = if matches!(e, WaitItem::TimerEvent { .. }) {
                one_line(v.source, 80)
            } else {
                v.source.to_string()
            };
            let mut parts = vec![format!("{lead} {}{detail}", v.kind), source];
            if let Some(recurrence) = v.recurrence {
                parts.push(crate::cli::events::recurrence_summary(recurrence));
            }
            if let Some(g) = &v.github {
                if let Some(n) = g.number {
                    parts.push(format!("#{n}"));
                }
            }
            // The WHOLE body, safe for a terminal. A single line keeps its
            // place in the row; more than one leaves the row and follows
            // it indented — inline, the second line would start what
            // reads as the next item, and the ack after it would sit at
            // the end of the message instead of beside the event. Timer
            // bodies used to be cut at 300 characters here, and command
            // bodies arrived already cut to their first line.
            let body = v
                .content
                .map(crate::cli::status_tui::text::safe_multiline)
                .filter(|b| !b.is_empty());
            if let Some(b) = body.as_deref().filter(|b| !b.contains('\n')) {
                parts.push(b.to_string());
            }
            if let Some(g) = &v.github {
                if let Some(a) = g.actor {
                    parts.push(format!("by {a}"));
                }
                // The URL rides along — it is the actionable pointer
                // (external-wake-hints-carry-payload).
                if let Some(u) = g.url {
                    parts.push(u.to_string());
                }
            }
            // The line that says an event is owed should say how to
            // finish it.
            if let Some(s) = v.selector {
                if v.requires_reply {
                    parts.push(format!(
                        "(reply required; use clank events ack {s} --reply <text>)"
                    ));
                } else {
                    parts.push(format!("(ack {s})"));
                }
            }
            let mut line = parts.join("  ");
            if let Some(b) = body.as_deref().filter(|b| b.contains('\n')) {
                const INDENT: &str = "         ";
                line.push('\n');
                line.push_str(INDENT);
                line.push_str(&crate::cli::status_tui::text::indented_body(b, INDENT));
            }
            // The operator's per-watch prompt rides as ONE indented
            // follow-on line: control chars collapse (same treatment
            // as PR titles — operator text must not restructure the
            // item list), full text lives in `clank events read`.
            if let Some(i) = v.instructions.filter(|i| !i.trim().is_empty()) {
                line.push_str("\n         \u{21b3} ");
                line.push_str(&one_line(i, 300));
            }
            line
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
            // The question is for the HUMAN (who reads the block
            // elsewhere); the woken agent only needs "not your
            // move". It stays a json field.
            question: _,
        } => format!("blocked  {agent}/{name}  (awaiting human)"),
        WaitItem::Unblocked { answer, carries } => {
            format!("answer   {}  {answer}", carries.name)
        }
    }
}

/// Parse a duration string (`30s`, `5m`, `1h`; `0`/empty = None).
/// Collapse to a single line for hint contexts: control characters
/// become spaces (runs collapse), overlong text truncates with an
/// ellipsis.
pub(crate) fn one_line(s: &str, cap: usize) -> String {
    let mut out = String::with_capacity(s.len().min(cap + 4));
    let mut last_space = false;
    for ch in s.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch == ' ' && last_space {
            continue;
        }
        last_space = ch == ' ';
        out.push(ch);
    }
    let out = out.trim().to_string();
    if out.chars().count() > cap {
        let mut t: String = out.chars().take(cap).collect();
        t.push('…');
        t
    } else {
        out
    }
}

/// Parse a duration in clank's one grammar (`30s`, `5m`, `1h`; `0`
/// or empty means none). Shared grammar from extra-wait-events; the
/// wait's own bound is gone (remove-wait-timeout) and github
/// `poll_interval` is now the only caller, so the errors name the
/// VALUE rather than any flag.
pub(crate) fn parse_duration_str(raw: &str) -> anyhow::Result<Option<Duration>> {
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
        .map_err(|_| anyhow::anyhow!("invalid duration `{raw}` (expected e.g. 30s, 5m, 1h)"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n
            .checked_mul(60)
            .ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
        "h" => n
            .checked_mul(3600)
            .ok_or_else(|| anyhow::anyhow!("timeout overflow"))?,
        other => anyhow::bail!("invalid duration unit `{other}` (use s, m, or h)"),
    };
    Ok(Some(Duration::from_secs(secs)))
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    use crate::cli::monitor::{Conn, Note};

    /// A monitor's end of a pair, writing frames a test chooses.
    fn monitor_saying(frames: Vec<Note>) -> Conn {
        use std::io::Write;
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            for frame in frames {
                let line = serde_json::to_string(&frame).unwrap();
                if writeln!(&theirs, "{line}").is_err() {
                    return;
                }
            }
            // Stay open: a close would look like a dead monitor and
            // end the loop for the wrong reason.
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
        Conn::over(mine)
    }
    fn promote() -> WaitItem {
        WaitItem::PromoteFromQueue {
            name: "next".into(),
            priority: 500,
        }
    }

    /// Beats are not work: they prove the monitor is folding, and the
    /// wait keeps waiting.
    #[tokio::test]
    async fn a_beat_is_not_a_wake_but_items_are() {
        let repo = tempfile::tempdir().unwrap();
        let conn = monitor_saying(vec![
            Note::Beat,
            Note::Beat,
            Note::Items {
                source_cwd: None,
                items: vec![promote()],
            },
        ]);

        let got = shared_wait(
            conn,
            repo.path(),
            &AgentLabel::parse("claude").unwrap(),
            &WaitBinding(None, WaitPolicy::Manual),
        )
        .await
        .expect("the items arrive");

        let Shared::Woke(got, _) = got else {
            panic!("items are a wake, got {got:?}")
        };
        assert!(
            matches!(got.as_slice(), [WaitItem::PromoteFromQueue { .. }]),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn shared_delivery_retains_source_host_cwd_instead_of_the_client_cwd() {
        let repo = tempfile::tempdir().unwrap();
        let host_cwd = std::path::PathBuf::from("/different/source-host/cwd");
        let conn = monitor_saying(vec![Note::Items {
            items: vec![promote()],
            source_cwd: Some(host_cwd.clone()),
        }]);
        let got = shared_wait(
            conn,
            repo.path(),
            &AgentLabel::parse("claude").unwrap(),
            &WaitBinding(None, WaitPolicy::Manual),
        )
        .await
        .unwrap();
        let Shared::Woke(_, cwd) = got else {
            panic!("expected shared work")
        };
        assert_eq!(cwd, Some(host_cwd));
    }

    /// An empty Items frame is not a wake either — returning it would
    /// wake an agent with nothing to do.
    #[tokio::test]
    async fn an_empty_items_frame_is_skipped() {
        let repo = tempfile::tempdir().unwrap();
        let conn = monitor_saying(vec![
            Note::Items {
                items: Vec::new(),
                source_cwd: None,
            },
            Note::Items {
                source_cwd: None,
                items: vec![promote()],
            },
        ]);

        let got = shared_wait(
            conn,
            repo.path(),
            &AgentLabel::parse("claude").unwrap(),
            &WaitBinding(None, WaitPolicy::Manual),
        )
        .await
        .unwrap();
        let Shared::Woke(got, _) = got else {
            panic!("the frame that carried items is the wake, got {got:?}")
        };
        assert_eq!(got.len(), 1, "the empty frame was not a wake: {got:?}");
    }

    /// A repo whose monitor admits anyone who asks.
    ///
    /// Sharing refusals must be the GUARD talking and not a failure to
    /// connect, so the fixture is a monitor that would say yes.
    fn repo_with_a_willing_monitor() -> (crate::cli::monitor::TempRepo, tempfile::TempDir) {
        use std::io::{BufRead, BufReader, Write};

        let repo = crate::cli::monitor::TempRepo::new();
        let dir = tempfile::tempdir().unwrap();
        let socket = crate::cli::monitor::socket_path(dir.path(), 4242).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        crate::cli::monitor::publish(
            repo.path(),
            &crate::cli::monitor::Descriptor {
                proto: crate::cli::monitor::PROTO,
                pid: 4242,
                repo: repo.path().to_string_lossy().to_string(),
                socket: socket.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let welcome = crate::cli::monitor::Welcome::Ok {
                    proto: crate::cli::monitor::PROTO,
                };
                let _ = writeln!(
                    reader.into_inner(),
                    "{}",
                    serde_json::to_string(&welcome).unwrap()
                );
            }
        });
        (repo, dir)
    }

    /// `--peek` must neither connect nor start anything.
    ///
    /// It is documented SIDE-EFFECT-FREE, and starting a monitor is
    /// the largest side effect in this file. The guarantee is an
    /// ORDERING inside one function — peek returns before the share —
    /// and nothing observable distinguishes "returned early" from
    /// "connected and then returned", so the order is read rather
    /// than driven.
    #[test]
    fn peek_returns_before_anything_is_shared_or_started() {
        let src = include_str!("wait.rs");
        let peek = src
            .find("if args.peek {")
            .expect("peek still returns early");
        let share = src
            .find("share(&repo, &author, &binding, &sources)")
            .expect("the share is still in run");
        assert!(
            peek < share,
            "peek must return BEFORE the share, or it can start a monitor"
        );
        assert!(
            !src[peek..share].contains("share("),
            "and nothing between them may share either"
        );
    }

    /// The one wiring no test here may run.
    ///
    /// Starting a real monitor is banned in this suite, so every test
    /// below substitutes the starter — which leaves `share`'s own
    /// choice of the real one unexercised. Read it instead: without
    /// this, production could quietly stop starting anything and the
    /// whole suite would still be green.
    #[test]
    fn share_hands_the_real_starter_to_the_seam() {
        let src = include_str!("wait.rs");
        let from = src.find("async fn share(").expect("share is still here");
        let to = src[from..]
            .find("async fn share_with(")
            .expect("the seam is still here");
        assert!(
            src[from..from + to].contains("monitor::spawn_monitor"),
            "`share` must pass the REAL starter, not a stub"
        );
    }

    /// Sharing against the fixture, with starting one made impossible.
    ///
    /// A refusal is then the fixture breaking rather than the guard
    /// working, so the assertions stay honest instead of passing on a
    /// real monitor.
    async fn share_without_starting(
        repo: &Path,
        author: &AgentLabel,
        binding: &WaitBinding,
        sources: &WakeSources,
    ) -> Option<Conn> {
        share_with(repo, author, binding, sources, |_| {
            Err(std::io::Error::other("a test must not start a monitor"))
        })
        .await
    }

    /// A wait with its own sources must NOT share.
    ///
    /// The monitor delivers the projection and does not yet run
    /// `wait_events`, so a shared wait with github or command sources
    /// would never hear from them.
    #[tokio::test]
    async fn a_wait_shares_when_the_monitor_covers_all_of_its_sources() {
        let (repo, _dir) = repo_with_a_willing_monitor();
        let author = AgentLabel::parse("claude").unwrap();
        let binding = WaitBinding(None, WaitPolicy::Manual);

        let nothing = WakeSources { events: Vec::new() };
        assert!(nothing.may_share());
        assert!(
            share_without_starting(repo.path(), &author, &binding, &nothing)
                .await
                .is_some(),
            "with no sources it shares"
        );

        // A CONFIG source is an AUTHOR's, and the monitor runs it once
        // per repo and routes it back to that author — so having one is
        // no longer a reason to watch alone.
        let agent_dir = crate::agent_store::agents_root(repo.path()).join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.json"),
            r#"{"wait_events":[{"kind":"command","name":"deploys","command":["true"]}]}"#,
        )
        .unwrap();
        let configured = WakeSources {
            events: resolve_wait_event_sources(repo.path(), &author, &[]).unwrap(),
        };
        assert!(
            !configured.events.is_empty(),
            "the fixture declares a config source"
        );
        assert!(configured.may_share());
        assert!(
            share_without_starting(repo.path(), &author, &binding, &configured)
                .await
                .is_some(),
            "the monitor hosts config sources, so this wait may share"
        );

        // A `--event` source belongs to ONE INVOCATION. The monitor
        // never pools it, or this invocation's events would reach
        // whoever else happened to be this author.
        let invoked = WakeSources {
            events: resolve_wait_event_sources(
                repo.path(),
                &author,
                &[r#"{"kind":"command","name":"ticker","command":["true"]}"#.to_string()],
            )
            .unwrap(),
        };
        assert!(!invoked.may_share());
        assert!(
            share_without_starting(repo.path(), &author, &binding, &invoked)
                .await
                .is_none(),
            "a source of this invocation's own keeps it watching for itself"
        );
    }

    /// A configured source and an invocation's source of the SAME NAME
    /// each run their own command, and keep their own state.
    ///
    /// They used to share everything keyed off the name: one log, one
    /// ingest lease and one `CLANK_SOURCE_STATE_DIR`. The lease is
    /// what made it visible — whichever arrived second tailed the
    /// first's log and never ran the command it was given — and the
    /// shared state directory is what made it lossy, because a
    /// stateful backend reads another invocation's cursor there
    /// (codex on 03cc11f).
    #[tokio::test]
    async fn a_configured_and_an_invocation_source_of_one_name_do_not_share() {
        use crate::cli::command_source as cs;

        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("claude").unwrap();
        let agent_dir = crate::agent_store::agents_root(dir.path()).join("claude");
        let events = agent_dir.join("events");
        std::fs::create_dir_all(&agent_dir).unwrap();
        // A configured `deploys`, and a `--event` one by the same name.
        // Built through `serde_json` rather than written as a nested
        // string literal: the escaping of a shell command inside JSON
        // inside Rust is where this test first silently tested nothing.
        // Each producer reports the private directory it was HANDED,
        // in `CLANK_SOURCE_STATE_DIR`. Recomputing the directories in
        // the test proved only that the helper is injective; what has
        // to be true is that the RUNNER passes the resolved key, and
        // only the producer can report what it actually received.
        let emits = |id: &str| {
            format!(
                r#"printf '{{"id":"{id}","content":"'"$CLANK_SOURCE_STATE_DIR"'"}}
'; sleep 300"#
            )
        };
        let declare = |name: &str, body: &str| serde_json::json!({"kind": "command", "name": name, "command": ["sh", "-c", body]});
        std::fs::write(
            agent_dir.join("config.json"),
            serde_json::json!({"wait_events": [declare("deploys", &emits("cfg"))]}).to_string(),
        )
        .unwrap();
        let both = resolve_wait_event_sources(
            dir.path(),
            &author,
            &[declare("deploys", &emits("inv")).to_string()],
        )
        .unwrap();
        assert_eq!(both.len(), 2, "one configured and one invocation's");

        let (mut rx, spawned) = spawn_local(&both, Some(&events), Some("inv-7"));
        let tasks = spawned.into_tasks();
        let mut seen: Vec<String> = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while seen.len() < 2 {
                match rx.recv().await {
                    Some((WaitItem::CommandEvent { content, .. }, _)) => seen.push(content),
                    Some(_) => {}
                    None => break,
                }
            }
        })
        .await;
        assert_eq!(
            seen.len(),
            2,
            "BOTH commands ran: neither tailed the other's log — {seen:?}"
        );
        seen.sort();
        assert_ne!(
            seen[0], seen[1],
            "and each was handed its OWN private directory. One shared \
             `CLANK_SOURCE_STATE_DIR` is how a stateful backend reads the \
             other invocation's cursor, so an event disappears before it \
             reaches the isolated log: {seen:?}"
        );
        // Each is the directory that source's RESOLVED key names, which
        // is what ties the private state to the log and the lease.
        let keys: Vec<String> = both
            .iter()
            .map(|(s, origin)| match s {
                clank_core::agent_config::WaitEventSource::Command(c) => {
                    cs::storage_key(c.source_key(), *origin, "inv-7")
                }
                other => panic!("command sources, got {other:?}"),
            })
            .collect();
        for key in &keys {
            let want = cs::backend_state_dir(&agent_dir, key)
                .to_string_lossy()
                .to_string();
            assert!(
                seen.contains(&want),
                "a producer was handed {seen:?}, not the directory its own \
                 key names ({want})"
            );
        }

        for (_, task) in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    /// A CLI author runs its own sources and receives both of them; a
    /// config-only author is eligible to share and receives its own.
    ///
    /// That is the whole of what this drives, and the doc says so
    /// because the previous version of it did not. Each author gets
    /// its own `EventSources` and its own receiver, so "neither author
    /// sees the other's" could not fail here — there is no shared
    /// channel for a row to cross — and no monitor exists in this test
    /// to see anything. The three isolation properties are real and
    /// proved elsewhere, where they CAN fail (codex and ruthless on
    /// 153f450):
    ///
    /// - cross-author delivery, by `monitor::two_authors_sources_do_not_cross`,
    ///   which writes a row into each author's events dir and serves
    ///   both through one `serve_to`;
    /// - a scoped log being invisible to the monitor, by
    ///   `monitor::the_monitor_reads_only_an_authors_configured_logs`;
    /// - and that nothing the monitor hosts is an invocation's, by
    ///   `monitor::nothing_the_monitor_hosts_belongs_to_an_invocation`.
    ///
    /// What this one does establish, and what the mutation check
    /// confirms, is that the CLI source RUNS and DELIVERS: it fails if
    /// the source is never started, and if it is started with its
    /// delivery discarded.
    #[tokio::test]
    async fn a_cli_author_keeps_its_own_channel_while_another_shares() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(
            dir.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();

        let emits = |name: &str, what: &str| {
            serde_json::json!({
                "kind": "command",
                "name": name,
                "command": [
                    "sh",
                    "-c",
                    format!(r#"printf '{{"id":"{what}","content":"{what}"}}\n'; sleep 300"#),
                ],
            })
        };
        let configure = |who: &str, name: &str, what: &str| {
            let agent = crate::agent_store::agents_root(dir.path()).join(who);
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(
                agent.join("config.json"),
                serde_json::json!({"wait_events": [emits(name, what)]}).to_string(),
            )
            .unwrap();
            agent
        };
        let claude = AgentLabel::parse("claude").unwrap();
        let codex = AgentLabel::parse("codex").unwrap();
        let claude_dir = configure("claude", "builds", "claude-config");
        let codex_dir = configure("codex", "deploys", "codex-config");

        // `codex` also declares an invocation source, so it may NOT
        // share and runs everything itself.
        let mine = resolve_wait_event_sources(
            dir.path(),
            &codex,
            &[emits("ticker", "codex-invocation").to_string()],
        )
        .unwrap();
        let sources = WakeSources {
            events: mine.clone(),
        };
        assert!(
            !sources.may_share(),
            "an invocation's source keeps this wait watching for itself"
        );
        let (mut rx, spawned) = spawn_local(&mine, Some(&codex_dir.join("events")), Some("inv-3"));

        // Its own channel receives BOTH: the configured event and the
        // invocation's.
        let tasks = spawned.into_tasks();
        let mut heard: Vec<String> = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while heard.len() < 2 {
                match rx.recv().await {
                    Some((WaitItem::CommandEvent { content, .. }, _)) => heard.push(content),
                    Some(_) => {}
                    None => break,
                }
            }
        })
        .await;
        heard.sort();
        assert_eq!(
            heard,
            vec!["codex-config".to_string(), "codex-invocation".to_string()],
            "the CLI author runs and receives both of its own sources"
        );

        // `claude` is config-only, so it is ELIGIBLE to share. What
        // this reads is its own config source emitting, not a monitor
        // presenting: the presentation path is `monitor::serve_to`'s
        // own tests, and this half is here to show the eligibility and
        // the emission are both real for the same author.
        let theirs = resolve_wait_event_sources(dir.path(), &claude, &[]).unwrap();
        assert!(
            WakeSources {
                events: theirs.clone()
            }
            .may_share(),
            "config only, so the monitor covers everything that wakes it"
        );
        let (mut claude_rx, claude_spawned) =
            spawn_local(&theirs, Some(&claude_dir.join("events")), None);
        let claude_tasks = claude_spawned.into_tasks();
        let claude_heard = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match claude_rx.recv().await {
                    Some((WaitItem::CommandEvent { content, .. }, _)) => return content,
                    Some(_) => {}
                    None => return String::new(),
                }
            }
        })
        .await
        .unwrap_or_default();
        assert_eq!(
            claude_heard, "claude-config",
            "the sharing author gets its OWN configured event and no \
             invocation of anybody else's"
        );

        for (_, task) in tasks.into_iter().chain(claude_tasks) {
            task.abort();
            let _ = task.await;
        }
    }

    /// Two invocations of ONE name for one author each get their own
    /// event, on their own channel.
    ///
    /// Two live producers, not two computed keys: comparing the keys
    /// proved the helper injective and left the delivery unproven,
    /// which is the gap this plan kept finding in its own tests
    /// (codex on a61a56d).
    #[tokio::test]
    async fn two_invocations_of_one_name_each_receive_their_own_event() {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("claude").unwrap();
        let events = crate::agent_store::agents_root(dir.path())
            .join("claude")
            .join("events");
        let emits = |what: &str| {
            serde_json::json!({
                "kind": "command",
                "name": "deploys",
                "command": [
                    "sh",
                    "-c",
                    format!(r#"printf '{{"id":"{what}","content":"{what}"}}\n'; sleep 300"#),
                ],
            })
            .to_string()
        };

        let arm = |body: String, scope: &'static str| {
            let declared = resolve_wait_event_sources(dir.path(), &author, &[body]).unwrap();
            spawn_local(&declared, Some(&events), Some(scope))
        };
        let (first_rx, first) = arm(emits("one"), "inv-1");
        let (second_rx, second) = arm(emits("two"), "inv-2");

        let heard = |mut rx: tokio::sync::mpsc::UnboundedReceiver<
            crate::cli::event_source::Emission,
        >| async move {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    match rx.recv().await {
                        Some((WaitItem::CommandEvent { content, .. }, _)) => return content,
                        Some(_) => {}
                        None => return String::new(),
                    }
                }
            })
            .await
            .unwrap_or_default()
        };
        let first_tasks = first.into_tasks();
        let second_tasks = second.into_tasks();
        let tasks: Vec<_> = first_tasks.into_iter().chain(second_tasks).collect();
        let (a, b) = tokio::join!(heard(first_rx), heard(second_rx));

        assert_eq!(a, "one", "the first invocation's own command ran");
        assert_eq!(
            b, "two",
            "and so did the second's — neither tailed the other's log, \
             which is what sharing one name-derived key did"
        );
        for (_, task) in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    /// Two invocations of one name for one author do not share either.
    #[tokio::test]
    async fn two_invocations_of_one_name_keep_their_own_state() {
        use crate::cli::command_source as cs;
        let src = clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
            stop_grace: None,
        };
        let agent_dir = std::path::Path::new("/a/.clank/agents/claude");

        let one = cs::storage_key(
            src.source_key(),
            crate::cli::event_source::SourceOrigin::Cli,
            "inv-1",
        );
        let two = cs::storage_key(
            src.source_key(),
            crate::cli::event_source::SourceOrigin::Cli,
            "inv-2",
        );
        assert_ne!(one, two);
        assert_ne!(
            cs::backend_state_dir(agent_dir, &one),
            cs::backend_state_dir(agent_dir, &two),
            "two invocations of one watch must not read each other's cursor"
        );
    }

    /// A pooled host refuses an invocation's source rather than pooling
    /// it.
    ///
    /// The monitor has no invocation to scope one to. Running it under
    /// the author's shared namespace is exactly the collision this
    /// plan must not have, so the refusal is the safe answer and is
    /// said out loud.
    #[tokio::test]
    async fn a_pooled_host_will_not_run_an_invocations_source() {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("claude").unwrap();
        let cli = resolve_wait_event_sources(
            dir.path(),
            &author,
            &[r#"{"kind":"command","name":"ticker","command":["true"]}"#.to_string()],
        )
        .unwrap();
        assert_eq!(cli.len(), 1);

        // No invocation to give: what the monitor passes.
        let (_rx, spawned) = spawn_local(&cli, None, None);
        assert!(
            spawned.tasks.is_empty(),
            "an invocation's source is refused, not pooled under the author"
        );

        // With one, it runs.
        let (_rx, spawned) = spawn_local(&cli, None, Some("inv-1"));
        assert_eq!(spawned.tasks.len(), 1);
        spawned.shutdown().await;
    }

    /// A PRE-HOSTING monitor is refused, and the wait keeps its sources.
    ///
    /// One of those can still be running after a binary update, holding
    /// its repo's lease. It accepts the same hello and serves only
    /// projected repo work, so a config-source wait that attached to it
    /// would have stopped running its own sources and missed their
    /// events for as long as that monitor lived (codex on a135276).
    #[tokio::test]
    async fn a_config_source_wait_refuses_a_monitor_that_cannot_host() {
        let (repo, dir) = repo_with_a_willing_monitor();
        let author = AgentLabel::parse("claude").unwrap();
        let binding = WaitBinding(None, WaitPolicy::Manual);
        let agent_dir = crate::agent_store::agents_root(repo.path()).join("claude");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.json"),
            r#"{"wait_events":[{"kind":"command","name":"deploys","command":["true"]}]}"#,
        )
        .unwrap();
        let configured = WakeSources {
            events: resolve_wait_event_sources(repo.path(), &author, &[]).unwrap(),
        };

        // A monitor of the version that does NOT host sources, which
        // ANSWERS as that version. The descriptor alone is not enough
        // any more and should not be: it is a hint about where to
        // knock, and the handshake is what settles the version — so a
        // fixture that published one number and answered with another
        // was describing a monitor that cannot exist.
        let socket = crate::cli::monitor::socket_path(dir.path(), 4343).unwrap();
        let older = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            while let Ok((stream, _)) = older.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let decline = crate::cli::monitor::Welcome::Decline {
                    proto: crate::cli::monitor::PROTO - 1,
                    why: "this monitor does not host sources".into(),
                };
                let _ = writeln!(
                    reader.get_mut(),
                    "{}",
                    serde_json::to_string(&decline).unwrap()
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        crate::cli::monitor::publish(
            repo.path(),
            &crate::cli::monitor::Descriptor {
                proto: crate::cli::monitor::PROTO - 1,
                pid: 4343,
                repo: repo.path().to_string_lossy().to_string(),
                socket: socket.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();

        assert!(
            configured.may_share(),
            "the wait itself is eligible — it is the MONITOR that cannot"
        );
        assert!(
            share_without_starting(repo.path(), &author, &binding, &configured)
                .await
                .is_none(),
            "so it watches for itself rather than losing its events"
        );
    }

    /// A monitor that stops speaking sends the wait back to watching
    /// for itself, rather than leaving it listening forever.
    /// A wait told it is superseded ends as superseded: it does not go
    /// on to watch for itself, which would deliver the newer wait's work
    /// a second time.
    #[tokio::test]
    async fn a_superseded_wait_ends_without_watching_for_itself() {
        use std::io::Write;
        let repo = tempfile::tempdir().unwrap();
        let (mine, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        mine.set_nonblocking(true).unwrap();
        writeln!(
            theirs,
            "{}",
            serde_json::to_string(&crate::cli::monitor::Note::Superseded).unwrap()
        )
        .unwrap();
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            shared_wait_within(
                Conn::over(mine),
                repo.path(),
                &AgentLabel::parse("claude").unwrap(),
                &WaitBinding(None, WaitPolicy::Manual),
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("a superseded wait ends at once");
        let Err(e) = ended else {
            panic!("a superseded wait went on: {ended:?}")
        };
        assert!(e.is::<WaitSuperseded>(), "{e:#}");
    }

    #[tokio::test]
    async fn silence_ends_the_sharing() {
        let repo = tempfile::tempdir().unwrap();
        let (mine, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        // `_theirs` is held open and silent: a dead monitor and a
        // wedged one are the same thing to a client. Non-blocking, as
        // `connect_within` hands it back, so the grace below is the
        // only thing that can end the wait.
        mine.set_nonblocking(true).unwrap();

        // Bounded: swallowing the error loops forever, and a hung
        // suite hides a regression where a failing one reports it.
        let why = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            shared_wait_within(
                Conn::over(mine),
                repo.path(),
                &AgentLabel::parse("claude").unwrap(),
                &WaitBinding(None, WaitPolicy::Manual),
                std::time::Duration::from_millis(150),
            ),
        )
        .await
        .expect("silence must end the sharing, not be waited on")
        .expect("silence is not an error: the wait watches for itself instead");

        let Shared::Alone(why) = why else {
            panic!("silence is not a wake, got {why:?}")
        };
        assert!(why.contains("stopped answering"), "{why}");
        assert!(
            why.contains("did not answer within"),
            "and it must carry the read's own reason, not just the close: {why}"
        );
    }

    /// Both doors out of sharing, with CONCURRENT clients.
    ///
    /// They are different paths and the plan asks for both: a monitor
    /// that DIES is an EOF on the next read, and one that WEDGES
    /// answers nothing while its socket stays open, which only the
    /// grace can end. One client recovering proves little — the
    /// failure mode worth excluding is a monitor's loss stranding some
    /// of its clients and not others — so each door is driven by
    /// several at once and every one of them must come back alone.
    #[tokio::test]
    async fn every_client_recovers_from_a_dead_monitor_and_from_a_wedged_one() {
        let repo = tempfile::tempdir().unwrap();

        for door in ["died", "wedged"] {
            let mut parked = Vec::new();
            let mut peers = Vec::new();
            for _ in 0..4 {
                let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
                mine.set_nonblocking(true).unwrap();
                peers.push(theirs);
                let repo = repo.path().to_path_buf();
                parked.push(tokio::spawn(async move {
                    shared_wait_within(
                        Conn::over(mine),
                        &repo,
                        &AgentLabel::parse("claude").unwrap(),
                        &WaitBinding(None, WaitPolicy::Manual),
                        std::time::Duration::from_millis(200),
                    )
                    .await
                }));
            }

            if door == "died" {
                // The first door: the process is gone, so the socket
                // reaches EOF — no grace need elapse at all.
                peers.clear();
            }
            // The second door keeps `peers` alive and silent, which is
            // a monitor still accepting and no longer folding.

            for client in parked {
                let got = tokio::time::timeout(std::time::Duration::from_secs(5), client)
                    .await
                    .unwrap_or_else(|_| panic!("a {door} monitor must not strand a client"))
                    .expect("the client task did not panic")
                    .expect("recovery is not an error: the wait watches for itself");
                let Shared::Alone(why) = got else {
                    panic!("a {door} monitor is not a wake, got {got:?}")
                };
                assert!(why.contains("stopped answering"), "{door}: {why}");
            }
            drop(peers);
        }
    }

    /// A rebind retires a wait that is owed NOTHING.
    ///
    /// The old session must not sit parked on healthy beats while its
    /// successor runs: ownership has to be checked on every note, not
    /// only when work turns up, or an idle repo keeps the loser alive
    /// indefinitely. Nothing actionable is ever sent here, so the exit
    /// can only come from the lifecycle check.
    #[tokio::test]
    async fn a_rebound_label_is_retired_by_beats_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("codex").unwrap();
        let ours = clank_core::SessionId::parse("session-a1").unwrap();
        let successor = clank_core::SessionId::parse("session-b2").unwrap();
        let binding = WaitBinding(
            Some((clank_core::Tool::Codex, ours.clone())),
            WaitPolicy::Manual,
        );
        crate::agent_store::bind_session_to_agent(repo, &label, clank_core::Tool::Codex, &ours)
            .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo,
            &label,
            clank_core::Tool::Codex,
            &successor,
        )
        .unwrap();

        let conn = monitor_saying(vec![
            Note::Beat,
            Note::Items {
                items: Vec::new(),
                source_cwd: None,
            },
            Note::Beat,
        ]);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            shared_wait_within(
                conn,
                repo,
                &label,
                &binding,
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("a superseded wait must not park on beats")
        .expect_err("a rebound label is retired, not kept warm");
        assert!(err.is::<WaitSuperseded>(), "{err:#}");
    }

    /// Items from a SHARED fold get the same late check a local wait
    /// makes. The monitor decided who was owed what at the fold; a
    /// rebind since then means this process no longer speaks for the
    /// label, and sharing must not be a way around that.
    #[tokio::test]
    async fn a_rebound_label_does_not_accept_shared_items() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("codex").unwrap();
        let ours = clank_core::SessionId::parse("session-a1").unwrap();
        let successor = clank_core::SessionId::parse("session-b2").unwrap();
        let binding = WaitBinding(
            Some((clank_core::Tool::Codex, ours.clone())),
            WaitPolicy::Manual,
        );
        crate::agent_store::bind_session_to_agent(repo, &label, clank_core::Tool::Codex, &ours)
            .unwrap();
        // The rebind the fold cannot have seen.
        crate::agent_store::bind_session_to_agent(
            repo,
            &label,
            clank_core::Tool::Codex,
            &successor,
        )
        .unwrap();

        let (mine, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        mine.set_nonblocking(true).unwrap();
        let note = crate::cli::monitor::Note::Items {
            source_cwd: None,
            items: vec![WaitItem::PromoteFromQueue {
                name: "next".into(),
                priority: 500,
            }],
        };
        writeln!(peer, "{}", serde_json::to_string(&note).unwrap()).unwrap();

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            shared_wait_within(
                Conn::over(mine),
                repo,
                &label,
                &binding,
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("the late check must answer, not park")
        .expect_err("a rebound label must not accept the items");
        assert!(err.is::<WaitSuperseded>(), "{err:#}");
    }
}

#[cfg(test)]
mod tests {
    fn unblocked(name: &str, answer: &str) -> clank_core::wait::WaitItem {
        clank_core::wait::WaitItem::Unblocked {
            answer: answer.into(),
            carries: clank_core::wait::AnswerRef {
                agent: clank_core::AgentLabel::parse("claude").unwrap(),
                name: name.into(),
                occurrence: Some("0123456789abcdef0123456789abcdef".into()),
                question_digest: "qd".into(),
                answer_digest: "ad".into(),
            },
        }
    }

    #[test]
    fn recurring_timer_human_hint_is_terminal_safe_and_carries_the_whole_body() {
        let r = crate::cli::timer_source::recurrence::Recurrence::from_generation(
            clank_core::agent_config::TimerGeneration {
                name: "deploy\nextra\u{1b}".into(),
                scope: None,
                anchor: 100,
                interval: 10,
            },
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let super::super::github::log::RecurringTransition::Accepted { item, .. } =
            super::super::github::log::EventLog::open(dir.path(), &r.source_key())
                .unwrap()
                .accept_recurring(&r, "check\nnow\u{1b}", 140)
                .unwrap()
        else {
            panic!("not accepted")
        };
        let hint = super::render_human(&item);
        assert!(hint.contains("5 scheduled occurrences"), "{hint}");
        assert!(hint.contains("every 10s"));
        // Terminal-safe: no escape and no control except the layout two.
        // It used to forbid `\n` outright, because the body was flattened
        // into the row — which is how a timer's second line was lost.
        assert!(
            !hint
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{hint:?}"
        );
        // The NAME is an identifier and still cannot open a new line...
        let header = hint.lines().next().unwrap();
        assert!(header.contains("deploy extra"), "{header}");
        // ...and the row carries NO copy of a multi-line body. A
        // flattened one there — `check now` — is the very text the timer
        // never said, and a mutation that left it in beside the real
        // body below survived until this line existed.
        assert!(!header.contains("check"), "{header}");
        // ...while the BODY arrives whole, below the row it belongs to.
        assert!(hint.ends_with("\n         check\n         now"), "{hint:?}");
    }

    use super::*;

    #[test]
    fn same_session_waits_coexist_and_rebind_revokes_only_other_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let label = AgentLabel::parse("codex").unwrap();
        let a = clank_core::SessionId::parse("session-a1").unwrap();
        let b = clank_core::SessionId::parse("session-b2").unwrap();
        let ours = WaitBinding(
            Some((clank_core::Tool::Codex, a.clone())),
            WaitPolicy::Manual,
        );
        let another = ours.clone();
        crate::agent_store::bind_session_to_agent(repo, &label, clank_core::Tool::Codex, &a)
            .unwrap();
        ours.ensure_current(repo, &label).unwrap();
        another.ensure_current(repo, &label).unwrap();
        // Repeated SessionStart for the same session changes config's
        // timestamp, but cannot invalidate either live wait.
        crate::agent_store::bind_session_to_agent(repo, &label, clank_core::Tool::Codex, &a)
            .unwrap();
        ours.ensure_current(repo, &label).unwrap();
        another.ensure_current(repo, &label).unwrap();
        crate::agent_store::bind_session_to_agent(repo, &label, clank_core::Tool::Codex, &b)
            .unwrap();
        assert!(
            ours.ensure_current(repo, &label)
                .unwrap_err()
                .is::<WaitSuperseded>()
        );
        let successor = WaitBinding(
            Some((clank_core::Tool::Codex, b.clone())),
            WaitPolicy::Manual,
        );
        successor.ensure_current(repo, &label).unwrap();
        crate::agent_store::release_binding(repo, &label, &b).unwrap();
        assert!(successor.ensure_current(repo, &label).is_err());
        let agent_dir = crate::agent_store::agents_root(repo).join(label.as_str());
        for name in ["wait.gen", "wait.claim", "turn.end"] {
            assert!(!agent_dir.join(name).exists(), "{name} must not be created");
        }
    }

    #[test]
    fn supersession_has_a_distinct_wait_wire() {
        let value = serde_json::to_value(SupersededEnvelope {
            items: [],
            superseded: true,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!({"items": [], "superseded": true}));
    }

    // ── extra-wait-events: command source field contracts ──────

    fn command_source(name: &str, argv: &[&str]) -> clank_core::agent_config::CommandSource {
        clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            command: argv.iter().map(|s| s.to_string()).collect(),
            reply_command: None,
            receipt_command: None,
            glyph: None,
            stop_grace: None,
        }
    }

    #[test]
    fn automatic_wait_checks_auto_off_on_existing_beats_without_cancelling_manual_waits() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("grok").unwrap();
        crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::Off).unwrap();
        let automatic = WaitBinding(None, WaitPolicy::AutomaticGrok { home: None });
        assert!(
            automatic
                .ensure_current(repo.path(), &author)
                .unwrap_err()
                .to_string()
                .contains("auto mode is off")
        );
        assert!(
            WaitBinding(None, WaitPolicy::Manual)
                .ensure_current(repo.path(), &author)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn configured_wide_and_unsafe_icons_do_not_stop_either_event_producer() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("codex").unwrap();
        let mut wide = command_source(
            "signal",
            &[
                "/bin/sh",
                "-c",
                "printf '%s\\n' '{\"id\":\"wide\",\"kind\":\"message\",\"content\":\"signal delivered\"}'; sleep 300",
            ],
        );
        wide.glyph = Some("💬".into());
        let mut unsafe_icon = command_source(
            "other",
            &[
                "/bin/sh",
                "-c",
                "printf '%s\\n' '{\"id\":\"unsafe\",\"kind\":\"message\",\"content\":\"other delivered\"}'; sleep 300",
            ],
        );
        unsafe_icon.glyph = Some("\n".into());
        let cfg = clank_core::agent_config::AgentConfig {
            wait_events: vec![
                clank_core::agent_config::WaitEventSource::Command(wide),
                clank_core::agent_config::WaitEventSource::Command(unsafe_icon),
            ],
            ..Default::default()
        };
        crate::agent_store::update_agent_config(repo.path(), &author, |stored| *stored = cfg)
            .unwrap();
        let declared = resolve_wait_event_sources(repo.path(), &author, &[]).unwrap();
        let (mut rx, sources) = spawn_local(&declared, None, None);
        let mut received = std::collections::BTreeMap::new();
        for _ in 0..2 {
            let (item, _) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("both configured producers must deliver")
                .unwrap();
            let WaitItem::CommandEvent {
                name,
                content,
                failure,
                ..
            } = item
            else {
                panic!("expected command event");
            };
            assert!(!failure);
            received.insert(name, content);
        }
        assert_eq!(
            received.get("signal").map(String::as_str),
            Some("signal delivered")
        );
        assert_eq!(
            received.get("other").map(String::as_str),
            Some("other delivered")
        );
        drop(sources);
    }

    /// An interruption description that cannot be typed into a pane is
    /// presentation, not custody: its source still starts and delivers next
    /// to another, its required reply still holds, and the hint handed to
    /// the transport tells its event as `New events` with its selector.
    #[tokio::test]
    async fn an_unusable_interrupt_message_never_stops_its_source() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("codex").unwrap();
        let mut voice = command_source(
            "voice",
            &[
                "/bin/sh",
                "-c",
                "printf '%s\\n' '{\"id\":\"v1\",\"content\":\"hello\"}'; sleep 300",
            ],
        );
        voice.interrupt = true;
        voice.interrupt_message = Some("Interrupted for new voice message".into());
        let mut bad = command_source(
            "bad",
            &[
                "/bin/sh",
                "-c",
                "printf '%s\\n' '{\"id\":\"b1\",\"content\":\"answer me\",\"requires_reply\":true}'; sleep 300",
            ],
        );
        bad.interrupt = true;
        bad.interrupt_message = Some("two\nlines".into());
        let keys = [voice.source_key(), bad.source_key()];
        let cfg = clank_core::agent_config::AgentConfig {
            wait_events: vec![
                clank_core::agent_config::WaitEventSource::Command(voice),
                clank_core::agent_config::WaitEventSource::Command(bad),
            ],
            ..Default::default()
        };
        crate::agent_store::update_agent_config(repo.path(), &author, |stored| *stored = cfg)
            .unwrap();
        let declared = resolve_wait_event_sources(repo.path(), &author, &[]).unwrap();
        assert_eq!(declared.len(), 2, "both sources are admitted");
        let events = crate::agent_store::agents_root(repo.path())
            .join(author.as_str())
            .join("events");
        std::fs::create_dir_all(&events).unwrap();
        let (mut rx, sources) = spawn_local(&declared, Some(&events), None);
        let mut names = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let (item, _) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("both producers deliver")
                .unwrap();
            let WaitItem::CommandEvent { name, failure, .. } = item else {
                panic!("expected command event");
            };
            assert!(!failure);
            names.insert(name);
        }
        assert_eq!(names, ["bad".to_string(), "voice".to_string()].into());
        drop(sources);

        let rows = crate::cli::events::read_all(&events).unwrap();
        let bad_row = rows.iter().find(|r| r.source == keys[1]).unwrap();
        assert!(bad_row.row.interrupt && !bad_row.row.acked);
        assert!(matches!(
            bad_row.row.item,
            WaitItem::CommandEvent {
                requires_reply: true,
                ..
            }
        ));
        let handed = std::sync::Mutex::new(None);
        crate::cli::event_interrupt::dispatch(events, Duration::from_secs(1), |text| {
            *handed.lock().unwrap() = Some(text);
            async { crate::cli::github::log::InterruptOutcome::Submitted }
        })
        .await
        .unwrap();
        let text = handed.into_inner().unwrap().unwrap();
        assert!(
            text.contains(&format!("Interrupted for new voice message: {}@", keys[0]))
                && text.contains(&format!("New events: {}@", keys[1]))
                && !text.contains("two"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn dropping_sources_ends_the_running_command_instead_of_detaching_it() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("grok").unwrap();
        let source = command_source(
            "cleanup",
            &[
                "sh",
                "-c",
                r#"printf '{"id":"started","content":"%s"}\n' $$; sleep 300"#,
            ],
        );
        let declared = vec![(
            clank_core::agent_config::WaitEventSource::Command(source),
            crate::cli::event_source::SourceOrigin::Config,
        )];
        let (mut rx, sources) = spawn_local(&declared, None, None);
        let (item, _) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let WaitItem::CommandEvent { content, .. } = item else {
            panic!("expected the producer's start event");
        };
        let pid = content.parse::<i32>().unwrap();
        assert_eq!(
            crate::signal::probe_process(pid),
            crate::signal::Probe::Ours
        );
        drop(sources);
        tokio::time::timeout(Duration::from_secs(5), async {
            while crate::signal::probe_process(pid) != crate::signal::Probe::Gone {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelling the wait must not leave its command source running");
    }

    /// A supervisor that cannot take its source's lease says it stopped,
    /// and why, rather than leave its host believing it starts.
    #[tokio::test]
    async fn a_command_source_that_cannot_take_its_lease_reports_it_stopped() {
        use crate::cli::event_source::Lifecycle;
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("events");
        std::fs::write(&not_a_dir, "a file where the events directory should be").unwrap();
        let src = command_source("blocked", &["true"]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reports, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let author = AgentLabel::parse("alpha").unwrap();
        let sink = crate::cli::event_source::EmissionSink::for_author(tx, author.clone())
            .reporting(reports, author, 3);
        let key = src.source_key();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_command_source(src, sink, Some(not_a_dir), key, StopRequest::never()),
        )
        .await
        .expect("it ends rather than retrying a lease it cannot take");
        assert!(
            matches!(heard.try_recv(), Ok((_, _, 3, Lifecycle::Stopped { why })) if why.contains("lease")),
        );
    }

    /// A hosted producer's host hears where it is: running, with the
    /// pid of the command it started, then backing off with how that
    /// run ended.
    #[tokio::test]
    async fn a_command_source_reports_its_lifecycle() {
        use crate::cli::event_source::Lifecycle;
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events");
        let src = command_source(
            "exits",
            &[
                "sh",
                "-c",
                "echo $$ > \"$0.pid\"; exit 3",
                events.to_str().unwrap(),
            ],
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reports, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let author = AgentLabel::parse("alpha").unwrap();
        let sink = crate::cli::event_source::EmissionSink::for_author(tx, author.clone())
            .reporting(reports, author.clone(), 1);
        let key = src.source_key();
        let task = tokio::spawn(run_command_source(
            src,
            sink,
            Some(events.clone()),
            key.clone(),
            StopRequest::never(),
        ));
        let mut states = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some((who, at, instance, state)) = heard.recv().await {
                assert_eq!(instance, 1);
                assert_eq!((&who, at.as_str()), (&author, key.as_str()));
                let backing_off = matches!(state, Lifecycle::BackingOff { .. });
                states.push(state);
                if backing_off {
                    break;
                }
            }
        })
        .await;
        task.abort();
        let _ = task.await;
        let pid: u32 = std::fs::read_to_string(format!("{}.pid", events.display()))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            matches!(states.as_slice(), [Lifecycle::Running { pid: p, .. }, Lifecycle::BackingOff { failures: 1, exit: Some(3), error: None, .. }] if *p == pid),
            "{states:?}"
        );
    }

    /// Run a supervised source until `want` items arrive, then stop
    /// it. The supervisor loops forever by design, so a test says how
    /// many events it is waiting for rather than awaiting completion.
    async fn collect(
        src: clank_core::agent_config::CommandSource,
        dir: Option<std::path::PathBuf>,
        want: usize,
    ) -> Vec<WaitItem> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let key = src.source_key();
        let task = tokio::spawn(run_command_source(src, tx, dir, key, StopRequest::never()));
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            while out.len() < want {
                match rx.recv().await {
                    Some((item, _)) => out.push(item),
                    None => break,
                }
            }
        })
        .await;
        // AWAITED, not just signalled. The source holds an ingest
        // lease and a process group; an abort that is never joined
        // leaves both alive for as long as the runtime takes to
        // unwind it, so a second arm in the SAME process finds the
        // lease held and does nothing. Production gets this free —
        // each arm is its own process — which is exactly why a test
        // has to ask for it.
        task.abort();
        let _ = task.await;
        out
    }

    #[tokio::test]
    async fn an_invocation_event_can_be_answered_after_its_producer_exits_without_config_lookup() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path();
        let dir = crate::agent_store::agents_root(repo)
            .join("alpha")
            .join("events");
        let mut source = command_source(
            "questions",
            &[
                "/bin/sh",
                "-c",
                "printf '%s\\n' '{\"id\":\"question/雪\",\"content\":\"Which option?\",\"requires_reply\":true}'; sleep 300",
            ],
        );
        source.reply_command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat > \"$CLANK_SOURCE_STATE_DIR/answer\"; pwd > \"$CLANK_SOURCE_STATE_DIR/cwd\""
                .into(),
        ]);
        let key = crate::cli::command_source::storage_key(
            source.source_key(),
            crate::cli::event_source::SourceOrigin::Cli,
            "fixture-invocation",
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let producer = tokio::spawn(run_command_source(
            source,
            tx,
            Some(dir.clone()),
            key.clone(),
            StopRequest::never(),
        ));
        let (item, _) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        producer.abort();
        let _ = producer.await;
        let WaitItem::CommandEvent {
            selector: Some(selector),
            reply_route: Some(route),
            requires_reply: true,
            ..
        } = item
        else {
            panic!("event lost its required reply route");
        };
        assert!(selector.contains("-inv-fixture-invocation@"));
        assert_eq!(
            route.state_dir,
            Some(crate::cli::command_source::backend_state_dir(
                dir.parent().unwrap(),
                &key
            ))
        );
        // No agent source config exists; --event ownership must be sufficient.
        assert!(!dir.parent().unwrap().join("config.json").exists());
        let args = crate::cli::EventsArgs {
            command: crate::cli::EventsCmd::Ack(crate::cli::EventsAckArgs {
                repo: Some(repo.into()),
                author: Some("alpha".into()),
                ids: vec![selector],
                reply: Some("  β\n".into()),
            }),
        };
        crate::cli::events::run(args).await.unwrap();
        let state = route.state_dir.unwrap();
        let reply: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state.join("answer")).unwrap()).unwrap();
        assert_eq!(reply["id"], "question/雪");
        assert_eq!(reply["reply"], "  β\n");
        assert_eq!(reply["author"], "alpha");
        assert_eq!(
            dunce::canonicalize(std::fs::read_to_string(state.join("cwd")).unwrap().trim())
                .unwrap(),
            dunce::canonicalize(route.cwd).unwrap()
        );
        let row = crate::cli::github::log::EventLog::open(&dir, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
            .remove(0);
        assert!(row.acked && row.reply_accepted);
    }

    /// Block until the source's ingest lease is free again.
    ///
    /// A re-arm is a fresh PROCESS in production, so the previous
    /// holder's fd is closed by exit and the lease is always free. In
    /// one test process it is released when the aborted task is
    /// dropped, which is runtime timing — so an arm-two test that
    /// just starts would intermittently find the lease held, return
    /// immediately, and look like a source that produced nothing.
    /// Waiting for it here makes the handoff the test's business
    /// rather than the scheduler's.
    async fn lease_released(events: &std::path::Path, key: &str) {
        for _ in 0..200 {
            match crate::cli::github::log::IngestLease::try_acquire(events, key) {
                Ok(Some(_)) => return,
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        panic!("the previous arm never released the ingest lease for `{key}`");
    }

    fn selector_of(item: &WaitItem) -> String {
        match item {
            WaitItem::CommandEvent { selector, .. } => selector
                .clone()
                .expect("a presented event says how to ack it"),
            other => panic!("expected a command event, got {other:?}"),
        }
    }

    /// A deferral withholds the launch, and does not overwrite the
    /// record that caused it.
    ///
    /// The failure this excludes: the retirement reports that it could
    /// not confirm the previous producer ended and keeps its record —
    /// and the caller starts a second producer anyway, whose own
    /// `record_producer` then overwrites the only evidence the first
    /// one exists. Two of the user's commands running, and nothing
    /// left that could end either (codex on 8d3d669).
    ///
    /// This replaces a test that read the SOURCE for a reap-before-
    /// spawn ordering. That ordering is no longer a convention to
    /// check: the launch consumes the retirement's outcome, so it
    /// cannot precede it, and what is worth asserting is the decision
    /// rather than the order.
    #[tokio::test]
    async fn a_deferred_retirement_withholds_the_launch_and_keeps_the_record() {
        use crate::cli::command_source as cs;

        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        let marker = dir.path().join("it-ran");
        let src = command_source(
            "deploys",
            &[
                "sh",
                "-c",
                Box::leak(
                    format!(
                        r#"touch '{}'; printf '{{"id":"A","content":"x"}}\n'; sleep 300"#,
                        marker.display()
                    )
                    .into_boxed_str(),
                ),
            ],
        );
        let key = src.source_key();
        // The record of a producer nobody can confirm is gone.
        std::fs::write(
            cs::producer_path(&events, &key),
            br#"{"pgid":4242,"started":"Thu 1 Oct 16:58:08 2026"}"#,
        )
        .unwrap();
        let before = std::fs::read(cs::producer_path(&events, &key)).unwrap();

        let defer = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let task = {
            let defer = defer.clone();
            let key = src.source_key();
            tokio::spawn(run_command_source_with(
                src,
                tx,
                Some(events.clone()),
                key,
                move |_: &Path, _: &str| {
                    if defer.load(std::sync::atomic::Ordering::Acquire) {
                        cs::Retired::Deferred("cannot confirm it ended".into())
                    } else {
                        cs::Retired::Clear
                    }
                },
                &crate::cli::source_group::OWNERS,
                StopRequest::never(),
            ))
        };

        tokio::time::sleep(cs::TAIL_INTERVAL * 2).await;
        assert!(
            !marker.exists(),
            "a deferral must not start a second producer beside the first"
        );
        assert_eq!(
            std::fs::read(cs::producer_path(&events, &key)).unwrap(),
            before,
            "nor overwrite the record that caused the deferral"
        );

        // Settled, and the launch follows.
        defer.store(false, std::sync::atomic::Ordering::Release);
        tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("a retired predecessor releases the launch")
            .expect("the channel closed");
        assert!(marker.exists(), "the producer ran once it was authorised");
        assert_ne!(
            std::fs::read(cs::producer_path(&events, &key)).unwrap(),
            before,
            "and the record is now its own"
        );

        task.abort();
        let _ = task.await;
    }

    /// A running producer's group is written down, led by its anchor,
    /// and ended when the source's task is cancelled.
    ///
    /// Driven through `run_command_source` because an unwired record
    /// is exactly the failure — the recognition and signalling have
    /// their own tests in `command_source.rs` and `source_group.rs`.
    /// Cancellation is how a monitor's stand-down, a config change and
    /// a returning wait all end their sources.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_producer_is_recorded_for_its_successor_and_ended_with_its_task() {
        use crate::cli::command_source as cs;
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        let pidfile = dir.path().join("pid");
        let src = command_source(
            "deploys",
            &[
                "sh",
                "-c",
                Box::leak(
                    format!(
                        r#"echo $$ > '{}'; printf '{{"id":"A","content":"x"}}\n'; exec sleep 300"#,
                        pidfile.display()
                    )
                    .into_boxed_str(),
                ),
            ],
        );
        let key = src.source_key();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        let task = tokio::spawn(run_command_source(
            src,
            tx,
            Some(events.clone()),
            key.clone(),
            StopRequest::never(),
        ));
        tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("the producer emitted nothing")
            .expect("the channel closed");

        let body = std::fs::read(cs::producer_path(&events, &key))
            .expect("a running producer is recorded");
        let record: cs::Producer = serde_json::from_slice(&body).unwrap();
        let cs::Producer::Anchored { pgid, .. } = record else {
            panic!("a new host writes an anchored record: {record:?}")
        };
        assert_eq!(
            cs::leader_of(&record),
            cs::Leader::Same,
            "and its leader is the live anchor it names"
        );
        let command: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(command, pgid, "the command does not lead its group");
        // SAFETY: a plain query, no memory effects.
        assert_eq!(
            unsafe { libc::getpgid(command) },
            pgid,
            "it runs INSIDE the recorded group, which is what gets signalled"
        );

        task.abort();
        let _ = task.await;
        let group = crate::signal::Pgid::new(pgid).unwrap();
        assert_eq!(cs::group_of(group), cs::Group::Empty);
        assert!(!cs::producer_path(&events, &key).exists());
    }

    const HOST_FIXTURE: &str = "CLANK_TEST_SOURCE_HOST_DIR";
    const HOST_MODE: &str = "CLANK_TEST_SOURCE_HOST_MODE";

    /// Not a test: the host process the tests below signal, entered by
    /// re-running this test binary with `HOST_FIXTURE` set. Without it
    /// this returns at once.
    ///
    /// A separate process because the wiring under test is what a
    /// process does when it is signalled or exits, which the test
    /// process cannot do to itself and survive.
    #[test]
    fn source_host_fixture() {
        let Some(dir) = std::env::var_os(HOST_FIXTURE).map(std::path::PathBuf::from) else {
            return;
        };
        let mode = std::env::var(HOST_MODE).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            use crate::cli::source_group;
            let src = fixture_source(&dir);
            let key = src.source_key();
            if mode == "unarmed" {
                let lease = std::sync::Arc::new(
                    crate::cli::github::log::IngestLease::try_acquire(&dir, &key)
                        .unwrap()
                        .unwrap(),
                );
                let _recorded = source_group::Spawned::spawn(&source_group::anchor_program())
                    .unwrap()
                    .record(
                        &source_group::OWNERS,
                        &dir,
                        &key,
                        lease,
                        crate::cli::command_source::DEFAULT_STOP_GRACE,
                    )
                    .unwrap();
                std::fs::write(dir.join("ready"), "").unwrap();
                std::future::pending::<()>().await;
            }
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let tx = crate::cli::event_source::EmissionSink::from(tx);
            let _task = tokio::spawn(run_command_source(
                src,
                tx,
                Some(dir.clone()),
                key,
                StopRequest::never(),
            ));
            while !dir.join("producer").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            std::fs::write(dir.join("ready"), "").unwrap();
            if mode == "exit" {
                source_group::exit(7);
            }
            std::future::pending::<()>().await;
        });
    }

    fn fixture_source(dir: &Path) -> clank_core::agent_config::CommandSource {
        command_source(
            "fixture",
            &[
                "sh",
                "-c",
                Box::leak(
                    format!(
                        "echo $$ > '{}'; exec sleep 300",
                        dir.join("producer").display()
                    )
                    .into_boxed_str(),
                ),
            ],
        )
    }

    /// A fixture host, SIGKILLed if a test leaves it running.
    struct FixtureHost(Option<std::process::Child>);

    impl FixtureHost {
        async fn start(dir: &Path, mode: &str) -> Self {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cli::wait::tests::source_host_fixture",
                    "--test-threads",
                    "1",
                ])
                .env(HOST_FIXTURE, dir)
                .env(HOST_MODE, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            let host = Self(Some(child));
            for _ in 0..6000 {
                if dir.join("ready").exists() {
                    return host;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the fixture host never became ready");
        }

        fn pid(&self) -> crate::signal::Pid {
            crate::signal::Pid::new(self.0.as_ref().unwrap().id() as i32).unwrap()
        }

        async fn ended(mut self) -> std::process::ExitStatus {
            let mut child = self.0.take().unwrap();
            tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || child.wait().unwrap()),
            )
            .await
            .expect("the fixture host did not end")
            .unwrap()
        }
    }

    impl Drop for FixtureHost {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn anchored_pgid(dir: &Path, key: &str) -> crate::signal::Pgid {
        use crate::cli::command_source as cs;
        let record: cs::Producer =
            serde_json::from_slice(&std::fs::read(cs::producer_path(dir, key)).unwrap()).unwrap();
        crate::signal::Pgid::new(record.pgid()).unwrap()
    }

    /// The handler a host installs, driven by the signals themselves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_told_to_stop_takes_its_producer_group_with_it() {
        use crate::cli::command_source as cs;
        use crate::cli::source_group::tests as host;
        use std::os::unix::process::ExitStatusExt;
        for sig in [libc::SIGTERM, libc::SIGINT] {
            let dir = tempfile::tempdir().unwrap();
            let key = fixture_source(dir.path()).source_key();
            let fixture = FixtureHost::start(dir.path(), "host").await;
            let producer = host::pid_in(&dir.path().join("producer")).await;
            let group = anchored_pgid(dir.path(), &key);

            crate::signal::signal_process(fixture.pid(), sig);
            let status = fixture.ended().await;
            assert_eq!(status.signal(), Some(sig), "it ends as the signal ends it");
            assert!(
                host::gone(producer).await,
                "signal {sig}: the producer survived"
            );
            assert_eq!(cs::group_of(group), cs::Group::Empty, "signal {sig}");
            assert!(
                !cs::producer_path(dir.path(), &key).exists(),
                "signal {sig}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_exits_takes_its_producer_group_with_it() {
        use crate::cli::command_source as cs;
        use crate::cli::source_group::tests as host;
        let dir = tempfile::tempdir().unwrap();
        let key = fixture_source(dir.path()).source_key();
        let fixture = FixtureHost::start(dir.path(), "exit").await;
        let producer = host::pid_in(&dir.path().join("producer")).await;
        let status = fixture.ended().await;
        assert_eq!(status.code(), Some(7));
        assert!(host::gone(producer).await);
        assert!(!cs::producer_path(dir.path(), &key).exists());
    }

    /// Real host death: nothing in the host runs, its pipes close, and
    /// its successor finds what it left.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_sigkilled_hosts_group_is_ended_by_its_successor() {
        use crate::cli::command_source as cs;
        use crate::cli::source_group::tests as host;
        for mode in ["unarmed", "host"] {
            let dir = tempfile::tempdir().unwrap();
            let key = fixture_source(dir.path()).source_key();
            let fixture = FixtureHost::start(dir.path(), mode).await;
            let group = anchored_pgid(dir.path(), &key);
            let producer = if mode == "host" {
                Some(host::pid_in(&dir.path().join("producer")).await)
            } else {
                None
            };

            crate::signal::signal_process(fixture.pid(), libc::SIGKILL);
            fixture.ended().await;
            if mode == "unarmed" {
                assert!(
                    host::gone(group.get()).await,
                    "an unarmed anchor outlived its host's activation pipe"
                );
            }
            let (events, k) = (dir.path().to_path_buf(), key.clone());
            let out = tokio::task::spawn_blocking(move || cs::reap_previous_producer(&events, &k))
                .await
                .unwrap();
            assert_eq!(out, cs::Retired::Clear, "{mode}");
            if let Some(producer) = producer {
                assert!(host::gone(producer).await, "{mode}");
            }
            assert_eq!(cs::group_of(group), cs::Group::Empty, "{mode}");
            assert!(!cs::producer_path(dir.path(), &key).exists(), "{mode}");
        }
    }

    /// Life's shape, both ways: a host replaced by SIGTERM, and one
    /// replaced by SIGKILL, leaves no producer of its own running when
    /// its successor starts one. The successor's producer is the
    /// witness, checking the old one the moment it starts.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_hosts_producer_is_gone_before_its_successor_starts_one() {
        use crate::cli::source_group::{self, tests as host};
        for sigkilled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let events = dir.path().to_path_buf();
            let oldfile = dir.path().join("old");
            let witness = |old: i32| {
                command_source(
                    "signal",
                    &[
                        "sh",
                        "-c",
                        Box::leak(
                            format!(
                                r#"if kill -0 {old} 2>/dev/null; then v=alive; else v=dead; fi; printf '{{"id":"A","content":"%s"}}\n' "$v"; exec sleep 300"#
                            )
                            .into_boxed_str(),
                        ),
                    ],
                )
            };
            let key = witness(0).source_key();

            let owners = host::owners();
            let lease = std::sync::Arc::new(
                crate::cli::github::log::IngestLease::try_acquire(&events, &key)
                    .unwrap()
                    .unwrap(),
            );
            let (owned, child) = source_group::launch(
                owners,
                &source_group::Arming::standard(),
                &events,
                &key,
                lease,
                crate::cli::command_source::DEFAULT_STOP_GRACE,
                &mut host::sh(&format!(
                    "echo $$ > '{}'; exec sleep 300",
                    oldfile.display()
                )),
            )
            .await
            .unwrap();
            let old = host::pid_in(&oldfile).await;
            if sigkilled {
                let anchor = owned.pgid().get();
                owned.abandon();
                drop(child);
                host::init_stands_in_for(anchor);
                host::init_stands_in_for(old);
            } else {
                tokio::task::spawn_blocking(|| owners.retire_all())
                    .await
                    .unwrap();
                drop(owned);
            }

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let tx = crate::cli::event_source::EmissionSink::from(tx);
            let task = tokio::spawn(run_command_source(
                witness(old),
                tx,
                Some(events.clone()),
                key.clone(),
                StopRequest::never(),
            ));
            let (item, _) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .expect("the successor started nothing")
                .expect("the channel closed");
            let WaitItem::CommandEvent { content, .. } = item else {
                panic!("expected a command event, got {item:?}")
            };
            assert_eq!(content, "dead", "sigkilled: {sigkilled}");
            task.abort();
            let _ = task.await;
        }
    }

    /// Through `run_command_source`, not the helpers: the mechanism
    /// has its own tests in `events.rs`, and a mechanism nobody calls
    /// is what this is about. The sibling re-arm test only REPLAYS
    /// old rows, so no append ever follows a re-arm there.
    #[tokio::test]
    async fn a_second_arm_does_not_reuse_the_first_arms_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        let one = |id: &str| {
            command_source(
                "deploys",
                &[
                    "sh",
                    "-c",
                    // One event, then block: the producer must not
                    // end the run, so this is a re-arm and not a
                    // restart.
                    Box::leak(
                        format!(r#"printf '{{"id":"{id}","content":"x"}}\n'; sleep 300"#)
                            .into_boxed_str(),
                    ),
                ],
            )
        };

        let first = collect(one("A"), Some(events.clone()), 1).await;
        let sel_a = selector_of(&first[0]);
        // Handled through the PUBLIC path, as an agent would.
        let key = one("A").source_key();
        crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .append_ack(
                sel_a
                    .rsplit_once('@')
                    .expect("source-qualified")
                    .1
                    .parse()
                    .unwrap(),
            )
            .unwrap();

        // ARM 2: a fresh supervisor over the same directory.
        lease_released(&events, &key).await;
        let second = collect(one("B"), Some(events.clone()), 1).await;
        assert_eq!(second.len(), 1, "arm 2 presented nothing");
        let sel_b = selector_of(&second[0]);
        assert_ne!(
            sel_a, sel_b,
            "a second arm reusing the sequence makes one ack cover two events",
        );

        // And the acked one stays acked while the new one is owed.
        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 2, "two distinct events");
        let owed: Vec<u64> = rows.iter().filter(|r| !r.acked).map(|r| r.seq).collect();
        assert_eq!(owed.len(), 1, "exactly one is still owed: {rows:?}");
        assert_eq!(
            format!("{key}@{}", owed[0]),
            sel_b,
            "and it is the one the second arm just presented",
        );
    }

    /// The SUPERVISOR reconciles its index from the log.
    ///
    /// `Seen::recover` has its own test, and so does the crash gap it
    /// repairs — but a supervisor that never CALLED it would pass
    /// both, because neither can see the call site. What only this
    /// can show is a replay after the index is gone: the log still
    /// vouches for the id, so no second event appears.
    #[tokio::test]
    async fn the_supervisor_repairs_its_index_from_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        // Same NAME in both arms, so the same source key and the
        // same WAL. Arm 2's producer REPLAYS the first id and then
        // sends a new one — explicitly allowed, since avoiding
        // resends is the backend's business.
        let producer = |lines: &str| {
            command_source(
                "deploys",
                &[
                    "sh",
                    "-c",
                    Box::leak(format!("printf '{lines}'; sleep 300").into_boxed_str()),
                ],
            )
        };
        let key = producer("").source_key();

        let first = collect(
            producer(r#"{"id":"same","content":"x"}\n"#),
            Some(events.clone()),
            1,
        )
        .await;
        assert_eq!(first.len(), 1, "arm 1 presents it once");

        // THE CRASH GAP, forced: the row is in the log, the index is
        // not. Exactly what a death between the two writes leaves.
        std::fs::remove_dir_all(events.join(format!("{key}.seen"))).unwrap();

        // Arm 2 owes the unacked row and the producer replays its id,
        // then sends a new one. Waiting for the NEW id is what proves
        // the replay was already handled — without it this would
        // assert on a race.
        lease_released(&events, &key).await;
        let second = collect(
            producer(r#"{"id":"same","content":"x"}\n{"id":"next","content":"y"}\n"#),
            Some(events.clone()),
            2,
        )
        .await;
        assert_eq!(second.len(), 2, "the owed row and the new event");

        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert_eq!(
            rows.len(),
            2,
            "the log already held `same`; a lost index must not mint a second row for it: {rows:?}",
        );
    }

    /// MILESTONE 2: a github event and a command event project to
    /// the same row, and github keeps what only it has.
    ///
    /// The two had separate rendering arms pulling differently-named
    /// fields to build the same line, in three places. This asserts
    /// the shared shape by rendering both and comparing structure —
    /// not by calling `event_view` twice, which would only prove the
    /// projection agrees with itself.
    #[test]
    fn a_github_event_and_a_command_event_render_as_one_row() {
        let gh = WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(12),
            title: Some("Add the widget".into()),
            actor: Some("hubot".into()),
            url: Some("https://github.com/o/r/pull/12".into()),
            instructions: None,
            content: None,
        };
        let cmd = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "sha-abc".into(),
            event: "deployed".into(),
            content: "prod is live".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: Some("command-deploys-0000abcd@7".into()),
        };

        let gh_line = render_human(&gh);
        let cmd_line = render_human(&cmd);
        // Both name their SOURCE and their KIND, in that shape.
        assert!(
            gh_line.contains("pr_comment") && gh_line.contains("o/r"),
            "{gh_line}"
        );
        assert!(
            cmd_line.contains("deployed") && cmd_line.contains("deploys"),
            "{cmd_line}"
        );
        // Both show their content the same way.
        assert!(gh_line.contains("Add the widget"), "{gh_line}");
        assert!(cmd_line.contains("prod is live"), "{cmd_line}");

        // THE CARVE-OUT: github keeps its link, its number and its
        // actor. Losing these is the way a shared shape goes wrong.
        assert!(
            gh_line.contains("https://github.com/o/r/pull/12"),
            "{gh_line}"
        );
        assert!(
            gh_line.contains("#12") && gh_line.contains("by hubot"),
            "{gh_line}"
        );
        assert!(
            gh_line.contains("(review)"),
            "the sub-kind survives: {gh_line}"
        );
        // And the command event keeps what only it has.
        assert!(
            cmd_line.contains("ack command-deploys-0000abcd@7"),
            "{cmd_line}"
        );

        // A source reporting ITSELF broken is marked as such, and
        // does not read as news about the world.
        let broken = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "clank.source-failure:x".into(),
            event: "clank.source-failure".into(),
            content: "`sh -c ...` has stopped 3 times in a row".into(),
            at: None,
            failure: true,
            requires_reply: false,
            reply_route: None,
            selector: Some("command-deploys-0000abcd@9".into()),
        };
        let broken_line = render_human(&broken);
        assert!(
            !broken_line.starts_with("command") && !broken_line.starts_with("github"),
            "a breakage must not read like an observation: {broken_line}",
        );
        assert!(
            broken_line.contains("ack command-deploys-0000abcd@9"),
            "{broken_line}"
        );
    }

    /// Arm-time validation refuses what cannot run, and ONLY that.    /// Arm-time validation refuses what cannot run, and ONLY that.
    ///
    /// The integration side proves `wait` refuses to arm; this side
    /// proves it does not refuse a source that is fine — without it,
    /// a validator that rejected everything would pass there.
    #[test]
    fn arm_time_validation_refuses_only_what_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let author = AgentLabel::parse("rev").unwrap();
        let resolve = |json: &str| {
            resolve_wait_event_sources(repo, &author, &[json.to_string()])
                .err()
                .map(|e| format!("{e:#}"))
        };

        let fine = resolve(r#"{"kind":"command","name":"fine","command":["true"],"glyph":"*"}"#);
        assert_eq!(fine, None, "a runnable source with a one-cell glyph arms");
        // No glyph at all is fine too — it is optional.
        assert_eq!(
            resolve(r#"{"kind":"command","name":"plain","command":["true"]}"#),
            None,
        );

        let empty = resolve(r#"{"kind":"command","name":"deploys","command":[]}"#)
            .expect("an empty command has nothing to run");
        assert!(
            empty.contains("deploys") && empty.contains("empty command"),
            "{empty}"
        );

        for glyph in ["💬", "👩‍💻", "\n", "", "ab", "\u{1b}"] {
            let source = serde_json::json!({"kind":"command","name":"cosmetic","command":["true"],"glyph":glyph});
            assert_eq!(
                resolve(&source.to_string()),
                None,
                "icon {glyph:?} cannot prevent admission"
            );
        }
    }

    #[test]
    fn receipt_handlers_arm_only_from_valid_configured_sources() {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("rev").unwrap();
        let agent_dir = crate::agent_store::agents_root(dir.path()).join(author.as_str());
        std::fs::create_dir_all(&agent_dir).unwrap();
        let source = serde_json::json!({"kind":"command","name":"voice","command":["anx","events","--flag"],"receipt_command":["anx","receipt","--verbatim"]});
        let write = |value: &serde_json::Value| {
            std::fs::write(
                agent_dir.join("config.json"),
                serde_json::to_vec(&serde_json::json!({"wait_events":[value]})).unwrap(),
            )
            .unwrap();
        };
        write(&source);
        let configured = resolve_wait_event_sources(dir.path(), &author, &[]).unwrap();
        assert_eq!(configured.len(), 1);
        assert_eq!(
            configured[0].1,
            crate::cli::event_source::SourceOrigin::Config
        );
        std::fs::remove_file(agent_dir.join("config.json")).unwrap();
        let error = resolve_wait_event_sources(dir.path(), &author, &[source.to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("invocation-only --event"), "{error}");
        for argv in [serde_json::json!([]), serde_json::json!([" "])] {
            let mut invalid = source.clone();
            invalid["receipt_command"] = argv;
            write(&invalid);
            let error = resolve_wait_event_sources(dir.path(), &author, &[])
                .unwrap_err()
                .to_string();
            assert!(error.contains("empty receipt_command"), "{error}");
        }
        std::fs::remove_file(agent_dir.join("config.json")).unwrap();
        let mut legacy = source;
        legacy.as_object_mut().unwrap().remove("receipt_command");
        assert!(resolve_wait_event_sources(dir.path(), &author, &[legacy.to_string()]).is_ok());
    }

    /// A quarantined log takes its index with it.
    ///
    /// `load` moves a corrupt WAL aside and returns no pending
    /// events — but the `.seen` entries survived, and `ingest_line`
    /// still believed them. So the producer's replay of a pending
    /// event was refused on behalf of an inbox that no longer held
    /// it: the event was unrecoverable by any route (codex 072e459).
    ///
    /// The index is DERIVED. When the thing it describes is thrown
    /// away, it must go too.
    #[tokio::test]
    async fn a_quarantined_log_does_not_leave_its_index_refusing_the_replay() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        let producer = command_source(
            "deploys",
            &[
                "sh",
                "-c",
                r#"printf '{"id":"pending","content":"owed"}\n'; sleep 300"#,
            ],
        );
        let key = producer.source_key();

        // ARM 1: one event, logged and left UNACKED.
        let first = collect(producer.clone(), Some(events.clone()), 1).await;
        assert_eq!(first.len(), 1);
        assert_eq!(
            crate::cli::command_source::Seen::at(&events, &key).known("pending"),
            crate::cli::command_source::Known::Yes,
        );

        // CORRUPT the WAL's interior — not a torn tail, which is a
        // normal crash artifact and is dropped rather than
        // quarantined.
        let log_path = events.join(format!("{key}.jsonl"));
        let text = std::fs::read_to_string(&log_path).unwrap();
        std::fs::write(&log_path, format!("not json at all\n{text}")).unwrap();

        // ARM 2: the same producer replays the same id. The log is
        // quarantined, so the inbox has lost the event — the replay
        // is the ONLY way it comes back, and it must not be refused.
        lease_released(&events, &key).await;
        let second = collect(producer, Some(events.clone()), 1).await;
        assert_eq!(
            second.len(),
            1,
            "the replay must be accepted: the quarantined inbox no longer holds it",
        );
        assert_eq!(event_parts(&second[0]).0, "pending");

        assert!(
            events.join(format!("{key}.jsonl.corrupt")).exists(),
            "the corrupt log is preserved as evidence",
        );
        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1, "and the fresh log holds it again: {rows:?}");
        assert!(!rows[0].acked, "still owed");
    }

    /// Only ONE wait runs the producer — and the other one TAILS,
    /// receives what the holder logs, and takes over when it leaves.
    ///
    /// The previous version of this test asserted the contender
    /// simply returned, which pinned a bug: nothing else tails
    /// command rows for that wait, so it got neither the pending
    /// backlog nor anything logged afterwards, and never took over
    /// (codex 072e459). One producer is only half the contract.
    #[tokio::test]
    async fn a_non_holder_tails_and_takes_over_without_a_second_producer() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().to_path_buf();
        // Each RUN appends a line, so the file counts producer spawns
        // across both supervisors.
        let marks = dir.path().join("runs");
        let producer = |id: &str| {
            command_source(
                "deploys",
                &[
                    "sh",
                    "-c",
                    Box::leak(
                        format!(
                            r#"printf 'run\n' >> {}; printf '{{"id":"{id}","content":"x"}}\n'; sleep 300"#,
                            marks.display()
                        )
                        .into_boxed_str(),
                    ),
                ],
            )
        };
        let key = producer("x").source_key();

        // SEPARATE channels, so "who received what" is answerable.
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let tx_a = crate::cli::event_source::EmissionSink::from(tx_a);
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        let tx_b = crate::cli::event_source::EmissionSink::from(tx_b);

        let holder = tokio::spawn(run_command_source(
            producer("owned"),
            tx_a,
            Some(events.clone()),
            key.clone(),
            StopRequest::never(),
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(20), rx_a.recv())
                .await
                .is_ok(),
            "the holder never produced",
        );

        // The follower must receive the holder's row WITHOUT running
        // a producer of its own.
        let follower = tokio::spawn(run_command_source(
            producer("contender"),
            tx_b,
            Some(events.clone()),
            key.clone(),
            StopRequest::never(),
        ));
        let (tailed, _) = tokio::time::timeout(Duration::from_secs(20), rx_b.recv())
            .await
            .expect("the follower must receive the holder's row, not sit idle")
            .expect("channel open");
        assert_eq!(
            event_parts(&tailed).0,
            "owned",
            "the follower presents what the HOLDER logged",
        );
        assert!(
            selector_of(&tailed).starts_with(&key),
            "and a tailed row is ackable like any other: {tailed:?}",
        );
        assert_eq!(
            std::fs::read_to_string(&marks)
                .unwrap_or_default()
                .lines()
                .count(),
            1,
            "the follower must not spawn the user's command",
        );

        // TAKEOVER: the holder leaves, the follower gets the lease
        // and starts its own producer — exactly one more run.
        holder.abort();
        let _ = holder.await;
        let (taken, _) = tokio::time::timeout(Duration::from_secs(20), rx_b.recv())
            .await
            .expect("the follower must take over when the holder leaves")
            .expect("channel open");
        assert_eq!(
            event_parts(&taken).0,
            "contender",
            "after takeover it runs its OWN producer",
        );
        assert_eq!(
            std::fs::read_to_string(&marks)
                .unwrap_or_default()
                .lines()
                .count(),
            2,
            "one producer at a time, and exactly one more after takeover",
        );

        // And it did not re-present, on taking the lease, the row it
        // already handed over while tailing.
        let mut again = Vec::new();
        while let Ok(i) = rx_b.try_recv() {
            again.push(i);
        }
        assert!(
            !again.iter().any(|(i, _)| event_parts(i).0 == "owned"),
            "a takeover must not re-present what this process already presented: {again:?}",
        );

        follower.abort();
        let _ = follower.await;
    }

    fn event_parts(item: &WaitItem) -> (&str, &str, &str) {
        match item {
            WaitItem::CommandEvent {
                id, event, content, ..
            } => (id, event, content),
            other => panic!("expected CommandEvent, got {other:?}"),
        }
    }

    /// Each JSONL line a producer prints is one event.
    #[tokio::test]
    async fn a_producer_streams_its_lines_as_events() {
        let items = collect(
            command_source(
                "deploys",
                &[
                    "sh",
                    "-c",
                    r#"printf '{"id":"1","kind":"deploy","content":"one"}\n{"id":"2","kind":"deploy","content":"two"}\n'"#,
                ],
            ),
            None,
            2,
        )
        .await;
        assert_eq!(items.len(), 2);
        assert_eq!(event_parts(&items[0]), ("1", "deploy", "one"));
        assert_eq!(event_parts(&items[1]), ("2", "deploy", "two"));
    }

    /// One bad line does not cost every later event.
    #[tokio::test]
    async fn a_refused_line_does_not_stop_the_stream() {
        let items = collect(
            command_source(
                "mixed",
                &[
                    "sh",
                    "-c",
                    r#"printf '{"id":"1","kind":"k","content":"before"}\nnot json\n{"kind":"k"}\n{"id":"2","kind":"k","content":"after"}\n'"#,
                ],
            ),
            None,
            2,
        )
        .await;
        assert_eq!(items.len(), 2, "the good lines both arrive");
        assert_eq!(event_parts(&items[0]).2, "before");
        assert_eq!(
            event_parts(&items[1]).2,
            "after",
            "a malformed line and an id-less one are skipped, not fatal",
        );
    }

    /// Every line is an event: clank does no filtering, so a `kind`
    /// it was not "expecting" still arrives.
    ///
    /// Replaces a filter test. What counts as an event is the
    /// backend's business now — one that wants filtering takes it as
    /// arguments in its own command.
    #[tokio::test]
    async fn every_kind_arrives_because_clank_does_not_filter() {
        let items = collect(
            command_source(
                "unfiltered",
                &[
                    "sh",
                    "-c",
                    r#"printf '{"id":"1","kind":"chatter","content":"a"}\n{"id":"2","kind":"deploy","content":"b"}\n'"#,
                ],
            ),
            None,
            2,
        )
        .await;
        assert_eq!(items.len(), 2, "both kinds are events");
        assert_eq!(event_parts(&items[0]).1, "chatter");
        assert_eq!(event_parts(&items[1]).1, "deploy");
    }

    /// A producer cannot report ITSELF broken: the failure kind is
    /// A producer cannot report ITSELF broken: the failure kind is
    /// clank's own, so "this source is not working" can never be
    /// something the source said.
    #[tokio::test]
    async fn a_producer_cannot_forge_a_failure_event() {
        let items = collect(
            command_source(
                "liar",
                &[
                    "sh",
                    "-c",
                    &format!(
                        r#"printf '{{"id":"1","kind":"{k}","content":"fake"}}\n{{"id":"2","kind":"real","content":"ok"}}\n'"#,
                        k = crate::cli::command_source::FAILURE_KIND
                    ),
                ],
            ),
            None,
            1,
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(event_parts(&items[0]).2, "ok", "the forged line is refused");
    }

    /// The SUPERVISOR re-presents what is still owed, before the
    /// producer runs.
    ///
    /// The WAL's unacked rows are readable on their own, but a
    /// supervisor that never re-presented them would pass every other
    /// test here — new lines still arrive, dedup still works. What
    /// only the call site can show is an event stored by a PREVIOUS
    /// arm reaching the agent again, which is the whole of
    /// at-least-once across a wake.
    #[tokio::test]
    async fn the_supervisor_re_presents_unacked_work_from_a_previous_arm() {
        let dir = tempfile::tempdir().unwrap();
        // A producer that prints NOTHING: the only thing that can
        // arrive is what a previous arm left owed.
        let src = command_source("owed", &["sh", "-c", "sleep 300"]);
        let key = src.source_key();

        let mut log = crate::cli::github::log::EventLog::open(dir.path(), &key).unwrap();
        let stored = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "owed".into(),
            id: "left-over".into(),
            event: "k".into(),
            content: "from the last arm".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let seq = log
            .append_inbox(
                crate::cli::github::log::Transport::Command,
                Some("left-over"),
                None,
                None,
                &stored,
            )
            .unwrap();

        let items = collect(src, Some(dir.path().to_path_buf()), 1).await;
        assert_eq!(items.len(), 1, "the owed event is presented again");
        assert_eq!(
            event_parts(&items[0]),
            ("left-over", "k", "from the last arm"),
            "and with its payload, not just its id",
        );
        // Re-presented means presented PROPERLY: an event the agent
        // cannot ack would wake it forever.
        let WaitItem::CommandEvent { selector, .. } = &items[0] else {
            panic!("expected a command event")
        };
        assert_eq!(
            selector.as_deref(),
            Some(format!("{key}@{seq}").as_str()),
            "a re-presented event still says how to finish itself",
        );
    }

    /// THE HANDOVER: the backend gets its OWN directory, and clank
    /// never reads it.
    ///
    /// This replaces the resume token. Clank no longer holds a cursor
    /// on a backend's behalf — it hands over a private directory and
    /// the backend keeps whatever it needs there. The producer proves
    /// it by writing to the directory and reporting the path back as
    /// an event.
    #[tokio::test]
    async fn the_backend_gets_a_private_directory_clank_does_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let src = command_source(
            "stateful",
            &[
                "sh",
                "-c",
                r#"printf 'cursor' > "$CLANK_SOURCE_STATE_DIR/mine"; printf '{"id":"1","kind":"k","content":"%s"}\n' "$CLANK_SOURCE_STATE_DIR"; sleep 300"#,
            ],
        );
        let items = collect(src, Some(events.clone()), 1).await;
        assert_eq!(items.len(), 1);
        let reported = event_parts(&items[0]).2.to_string();
        assert!(
            reported.contains("/sources/") && reported.contains("command-stateful-"),
            "the directory is keyed by the SOURCE KEY, which is one safe \
             component and cannot collide or escape: {reported}",
        );

        let own = std::path::Path::new(&reported);
        assert!(own.is_dir(), "clank creates it BEFORE the command starts");
        assert_eq!(
            std::fs::read_to_string(own.join("mine")).unwrap(),
            "cursor",
            "and what the backend wrote is still there",
        );
        // Clank's own storage is elsewhere: nothing it keeps lives in
        // the backend's directory, and it reads nothing from it.
        assert!(
            !own.starts_with(&events),
            "the backend's directory is not inside clank's event storage",
        );
    }

    /// A descendant holding the pipe open does not stall a restart.
    ///
    /// The root backgrounds a sleeper that INHERITS stdout, prints an
    /// event, and exits. Waiting for pipe EOF instead of root exit
    /// would leave the dead producer un-restarted for as long as the
    /// sleeper lives — five minutes here, indefinitely in production.
    /// Two events means it came back (codex fc7a4ff's hazard, in the
    /// streaming model).
    #[tokio::test]
    async fn a_pipe_holding_descendant_does_not_stall_the_restart() {
        let items = collect(
            command_source(
                "held",
                &[
                    "sh",
                    "-c",
                    r#"sleep 300 & printf '{"id":"r-%s","kind":"k","content":"ran"}\n' "$$""#,
                ],
            ),
            None,
            2,
        )
        .await;
        assert_eq!(
            items.len(),
            2,
            "the root exited and the source restarted, despite the held pipe",
        );
        assert_ne!(
            event_parts(&items[0]).0,
            event_parts(&items[1]).0,
            "two runs, two pids",
        );
    }

    /// A producer that DIES is restarted, and the events it printed
    /// before dying still arrive.
    ///
    /// Replaces the exit-code assertion this test used to make: an
    /// exit is no longer the event, so what matters is that a death
    /// does not lose what came before it or end the source.
    #[tokio::test]
    async fn a_producer_that_dies_is_restarted_and_loses_nothing() {
        let items = collect(
            command_source(
                "dying",
                &[
                    "sh",
                    "-c",
                    r#"printf '{"id":"a-%s","kind":"k","content":"alive"}\n' "$$"; kill -9 $$"#,
                ],
            ),
            None,
            2,
        )
        .await;
        assert_eq!(
            items.len(),
            2,
            "the line printed before the kill arrives, and the source comes back",
        );
        assert_eq!(event_parts(&items[0]).2, "alive");
        assert_ne!(
            event_parts(&items[0]).0,
            event_parts(&items[1]).0,
            "a restart is a new process, so its ids differ",
        );
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

    /// The pure decision, over the three shapes a block can meet.
    ///
    /// Asserted on `present_with_blocks` and not only through a peek
    /// because this is the function both callers share: a regression
    /// here is a regression in the park AND the peek at once, which is
    /// the property the split was introduced for.
    #[test]
    fn a_block_withholds_the_review_cycle_and_delivers_the_events() {
        let blocked = || BlockResult {
            items: vec![WaitItem::Blocked {
                agent: "m".into(),
                name: "q".into(),
                question: "why?".into(),
            }],
            suppress_all: true,
        };
        let review = || {
            vec![WaitItem::PromoteFromQueue {
                name: "next".into(),
                priority: 500,
            }]
        };
        let event = || {
            vec![WaitItem::Message {
                presentation: None,
                requires_reply: false,
                reply_route: None,
                handle: "h".into(),
                from: None,
                repo: None,
                subject: "hello".into(),
                body: "hello".into(),
                selector: None,
            }]
        };

        // Events survive, with the block leading as context.
        let out = present_with_blocks(blocked(), || Owed {
            review: review(),
            events: event(),
        });
        assert!(
            matches!(
                out.as_slice(),
                [WaitItem::Blocked { .. }, WaitItem::Message { .. }]
            ),
            "the block explains, the event wakes: {out:?}"
        );

        // …and the review cycle is what stays paused. Asserted by
        // ABSENCE of the promote and PRESENCE of the event, because
        // "returns nothing" satisfies the absence on its own.
        assert!(
            !out.iter()
                .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
            "{out:?}"
        );

        // A block with no event debt wakes nothing at all: the Blocked
        // item alone would re-fire at every turn end until a human
        // answered.
        assert!(
            present_with_blocks(blocked(), || Owed {
                review: review(),
                events: Vec::new(),
            })
            .is_empty()
        );

        // Unsuppressed, both classes travel together and the block
        // contributes nothing.
        let out = present_with_blocks(
            BlockResult {
                items: Vec::new(),
                suppress_all: false,
            },
            || Owed {
                review: review(),
                events: event(),
            },
        );
        assert_eq!(out.len(), 2, "{out:?}");

        // An ANSWER leads, and unlike a pending block it wakes on its
        // own — it is news, and it is what the agent has been waiting
        // for. The review cycle stays paused until it is received.
        let answered = || BlockResult {
            items: vec![unblocked("q", "yes")],
            suppress_all: true,
        };
        let out = present_with_blocks(answered(), || Owed {
            review: review(),
            events: event(),
        });
        assert!(
            matches!(
                out.as_slice(),
                [WaitItem::Unblocked { .. }, WaitItem::Message { .. }]
            ),
            "the answer leads and nothing is dropped: {out:?}"
        );
        let out = present_with_blocks(answered(), || Owed {
            review: review(),
            events: Vec::new(),
        });
        assert!(
            matches!(out.as_slice(), [WaitItem::Unblocked { .. }]),
            "and it wakes with nothing else ready: {out:?}"
        );
    }

    fn unblocked_repo() -> BlockResult {
        BlockResult {
            items: Vec::new(),
            suppress_all: false,
        }
    }

    fn cmd_row() -> WaitItem {
        WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "cmd".into(),
            id: "1".into(),
            event: "k".into(),
            content: String::new(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        }
    }

    /// A beat with BOTH routes ready emits ONE result containing both,
    /// and a beat with neither parks.
    #[test]
    fn a_beat_merges_both_routes_and_parks_only_when_both_are_empty() {
        // codex fc7a4ff: deterministic, no wall-clock racing.
        let beat = |review: Vec<WaitItem>, mut external: Vec<WaitItem>| {
            present_with_blocks(unblocked_repo(), || {
                beat_owed(
                    Owed {
                        review,
                        events: Vec::new(),
                    },
                    &mut external,
                )
            })
        };
        let repo_item = || {
            vec![WaitItem::Idle {
                prompt: "repo".into(),
            }]
        };

        let combined = beat(repo_item(), vec![cmd_row()]);
        assert_eq!(combined.len(), 2);
        assert!(matches!(combined[0], WaitItem::Idle { .. }));
        assert!(matches!(combined[1], WaitItem::CommandEvent { .. }));
        // Either alone still wakes.
        assert_eq!(beat(Vec::new(), vec![cmd_row()]).len(), 1);
        assert_eq!(beat(repo_item(), Vec::new()).len(), 1);
        // Neither → park.
        assert!(
            beat(Vec::new(), Vec::new()).is_empty(),
            "an empty beat must not make a normal wait exit",
        );
    }

    /// The CHANNEL route obeys the block decision too.
    ///
    /// The decision used to run before the channel drain, so a source
    /// item arriving after the projection read was returned raw: a
    /// blocked park handed the agent an external event with no
    /// `Blocked` beside it to explain why its plan work had gone
    /// quiet, and an answered one handed back the answer with company
    /// it had just promised not to have (codex on 344280f). Asserted
    /// on the composition, because each half was already right alone.
    #[test]
    fn a_channel_only_beat_meets_the_same_block_decision() {
        let decide = |br: BlockResult, mut external: Vec<WaitItem>| {
            present_with_blocks(br, || {
                beat_owed(
                    Owed {
                        // Review work is present and must be dropped:
                        // a test with an empty review side cannot tell
                        // suppression from having nothing to suppress.
                        review: vec![WaitItem::PromoteFromQueue {
                            name: "next".into(),
                            priority: 500,
                        }],
                        events: Vec::new(),
                    },
                    &mut external,
                )
            })
        };

        // Blocked, external-only: the event arrives WITH its context.
        let out = decide(
            BlockResult {
                items: vec![WaitItem::Blocked {
                    agent: "m".into(),
                    name: "q".into(),
                    question: "why?".into(),
                }],
                suppress_all: true,
            },
            vec![cmd_row()],
        );
        assert!(
            matches!(
                out.as_slice(),
                [WaitItem::Blocked { .. }, WaitItem::CommandEvent { .. }]
            ),
            "a channel item must not bypass the decision: {out:?}"
        );

        // Answered, external also ready: the answer LEADS and the
        // event follows. Neither is dropped — a memory-only source has
        // no WAL to re-read its item from.
        let out = decide(
            BlockResult {
                items: vec![unblocked("q", "yes")],
                suppress_all: true,
            },
            vec![cmd_row()],
        );
        assert!(
            matches!(
                out.as_slice(),
                [WaitItem::Unblocked { .. }, WaitItem::CommandEvent { .. }]
            ),
            "the answer leads, and nothing is silently dropped: {out:?}"
        );

        // And in both cases the review cycle stayed paused.
        assert!(
            !out.iter()
                .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
            "{out:?}"
        );
    }

    #[test]
    fn drain_external_fuses_a_closed_channel() {
        // codex 907acd5: the DETERMINISTIC proof the source branch gets
        // disabled — a disconnected channel flips `open` to false (so
        // the select branch stays off and can't hot-loop), while a live
        // channel drains its queued items and stays open.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::cli::event_source::EmissionSink::from(tx);
        tx.send((
            WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "a".into(),
                id: "1".into(),
                event: "k".into(),
                content: String::new(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
            crate::cli::event_source::Durability::Logged,
        ))
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
    fn parse_duration_zero_is_indefinite() {
        assert!(parse_duration_str("0").unwrap().is_none());
        assert!(parse_duration_str("").unwrap().is_none());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(
            parse_duration_str("30s").unwrap(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_duration_str("30").unwrap(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_duration_str("5m").unwrap(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_duration_str("1h").unwrap(),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration_str("abc").is_err());
        assert!(parse_duration_str("5x").is_err());
    }

    // ── wait-ignores-queue-only-blocks ─────────────────────────
    // A pending human Block is attachable context, never wake-worthy
    // alone: Blocked-only parks (times out), but a block must not hide
    // a lower-priority unblocked queue item.

    fn qentry(name: &str, priority: u16) -> crate::cli::queue::QueueEntry {
        crate::cli::queue::QueueEntry {
            priority,
            name: name.into(),
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn idle_queue_promotes_first_item() {
        // The ordinary idle-repo path: no plans, no blocks, non-empty
        // queue. Both call sites reach this, so a regression here
        // removes queue promotion from clank entirely.
        let items = queue_promote_outcome(&[qentry("first", 400), qentry("second", 500)]);
        assert!(
            matches!(
                items.as_slice(),
                [WaitItem::PromoteFromQueue { name, .. }] if name == "first"
            ),
            "expected the first queued item, got {items:?}"
        );
    }

    #[test]
    fn empty_queue_parks() {
        assert!(queue_promote_outcome(&[]).is_empty());
    }

    /// What `check_blocks` REPORTS is unchanged — every pending block
    /// still sets `suppress_all` and still surfaces itself as context.
    /// What that flag MEANS downstream is the thing that narrowed; see
    /// `present_with_blocks`.
    #[test]
    fn pending_block_suppresses_and_surfaces_itself() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let blocks = repo.join(".clank/agents/claude/blocks");
        std::fs::create_dir_all(&blocks).unwrap();
        std::fs::write(blocks.join("q.md"), "why?").unwrap();

        let br = check_blocks(repo, &AgentLabel::parse("claude").unwrap());

        assert!(
            br.suppress_all,
            "any pending block suppresses; blocks are repo-wide"
        );
        assert!(
            matches!(
                br.items.as_slice(),
                [WaitItem::Blocked { agent, name, .. }] if agent == "claude" && name == "q"
            ),
            "expected the pending block surfaced as context, got {:?}",
            br.items
        );
    }

    #[test]
    fn answered_block_returns_unblocked_and_stops_there() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agent = repo.join(".clank/agents/claude");
        std::fs::create_dir_all(agent.join("blocks")).unwrap();
        std::fs::create_dir_all(agent.join("unblocks")).unwrap();
        std::fs::write(agent.join("blocks/q.md"), "why?").unwrap();
        let before = check_blocks(repo, &AgentLabel::parse("claude").unwrap());
        assert!(matches!(
            before.items.as_slice(),
            [WaitItem::Blocked { .. }]
        ));
        // The flat answer path: written where the flat reader looks.
        // A split landing (reader flat, writer scoped) would leave the
        // pair unmatched and park the repo forever.
        crate::cli::block::answer_block(repo, "claude", "q", "because", None).unwrap();

        let br = check_blocks(repo, &AgentLabel::parse("claude").unwrap());

        assert!(
            matches!(
                br.items.as_slice(),
                [WaitItem::Unblocked { answer, carries }]
                    if carries.name == "q" && answer == "because"
            ),
            "expected the answer delivered, got {:?}",
            br.items
        );
    }

    fn grok_repo() -> (tempfile::TempDir, AgentLabel, WaitBinding) {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("grok").unwrap();
        crate::cli::agent::add_repo_roster_agent(
            dir.path(),
            &author,
            crate::cli::teams_config::AgentDescription {
                tool: clank_core::Tool::Grok,
                launch: None,
                initial_prompt: None,
            },
            crate::cli::teams_config::RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(dir.path(), &author).unwrap();
        let session = clank_core::SessionId::parse("grok-session").unwrap();
        crate::agent_store::bind_session_to_agent(
            dir.path(),
            &author,
            clank_core::Tool::Grok,
            &session,
        )
        .unwrap();
        crate::agent_store::set_auto_mode(dir.path(), &author, clank_core::AutoMode::On).unwrap();
        crate::cli::block::ask(dir.path(), &author, "q", "Proceed?").unwrap();
        crate::cli::block::answer_block(dir.path(), "grok", "q", "yes", None).unwrap();
        let binding = WaitBinding(
            Some((clank_core::Tool::Grok, session)),
            WaitPolicy::AutomaticGrok { home: None },
        );
        (dir, author, binding)
    }

    fn received(repo: &Path) -> bool {
        repo.join(".clank/agents/grok/unblocks/q.received").exists()
    }

    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Lands every byte, then hands the label to another session before
    /// the wait records anything.
    struct Rebinds<'a>(&'a Path, Vec<u8>);
    impl std::io::Write for Rebinds<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.1.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            crate::agent_store::bind_session_to_agent(
                self.0,
                &AgentLabel::parse("grok").unwrap(),
                clank_core::Tool::Grok,
                &clank_core::SessionId::parse("another-session").unwrap(),
            )
            .unwrap();
            Ok(())
        }
    }

    /// Grok's wait is a delivery boundary: the answer its wake prints is
    /// received once the print lands, and only then.
    #[test]
    fn a_grok_wake_receives_the_answer_it_printed() {
        let (dir, author, binding) = grok_repo();
        let items = block_items(dir.path(), &author);
        assert!(matches!(items.as_slice(), [WaitItem::Unblocked { .. }]));
        let mut out = Vec::new();
        deliver_grok_wake(dir.path(), &author, &binding, &items, true, None, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\"answer\":\"yes\"")
        );
        assert!(received(dir.path()));
        assert!(block_items(dir.path(), &author).is_empty());
    }

    #[test]
    fn a_grok_wake_that_failed_to_print_receives_nothing() {
        let (dir, author, binding) = grok_repo();
        let items = block_items(dir.path(), &author);
        assert!(
            deliver_grok_wake(
                dir.path(),
                &author,
                &binding,
                &items,
                true,
                None,
                &mut Broken
            )
            .is_err()
        );
        assert!(!received(dir.path()));
        assert_eq!(block_items(dir.path(), &author), items, "owed again");
    }

    #[test]
    fn a_grok_wake_whose_binding_went_stale_receives_nothing() {
        let (dir, author, binding) = grok_repo();
        let items = block_items(dir.path(), &author);
        let mut out = Rebinds(dir.path(), Vec::new());
        deliver_grok_wake(dir.path(), &author, &binding, &items, true, None, &mut out).unwrap();
        assert!(!out.1.is_empty(), "the wake printed");
        assert!(
            !received(dir.path()),
            "but its session no longer holds the label"
        );

        let mut never = Vec::new();
        assert!(
            deliver_grok_wake(
                dir.path(),
                &author,
                &binding,
                &items,
                true,
                None,
                &mut never
            )
            .is_err()
        );
        assert!(never.is_empty() && !received(dir.path()));
    }

    #[test]
    fn fix_commit_tag_human_line_names_all_three_kinds() {
        let item = WaitItem::FixCommitTag {
            sha: sha("aaaa"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: Some("plan".into()),
                untagged_touched: vec![PlanKey::parse("real").unwrap()],
                extra_named: Some(PlanKey::parse("extra").unwrap()),
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
            presentation: None,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(12),
            title: Some("Add the widget".into()),
            actor: Some("hubot".into()),
            url: Some("https://github.com/o/r/pull/12".into()),
            instructions: None,
            content: None,
        };
        assert_eq!(
            render_human(&full),
            "github   pr_comment (review)  o/r  #12  Add the widget  by hubot  \
             https://github.com/o/r/pull/12"
        );
        // …and absent optionals leave no dangling separators.
        let sparse = WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: None,
            title: None,
            actor: None,
            url: None,
            instructions: None,
            content: None,
        };
        assert_eq!(render_human(&sparse), "github   issue_opened  o/r");
        // The watch's operator prompt rides as one indented
        // follow-on line, control chars collapsed so operator text
        // can't restructure the item list (github-watch-prompts).
        // Promptless hints above stay byte-stable.
        let prompted = WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: None,
            title: None,
            actor: None,
            url: None,
            instructions: Some("Triage it:\nlabel,\treply,  then ack.".into()),
            content: None,
        };
        assert_eq!(
            render_human(&prompted),
            "github   issue_opened  o/r\n         ↳ Triage it: label, reply, then ack."
        );
        // Blank instructions render nothing extra.
        let blank = WaitItem::GithubEvent {
            presentation: None,
            repo: "o/r".into(),
            event: "issue_opened".into(),
            detail: None,
            number: None,
            title: None,
            actor: None,
            url: None,
            instructions: Some("  \n ".into()),
            content: None,
        };
        assert_eq!(render_human(&blank), "github   issue_opened  o/r");
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

    /// `WaitItem::kind()` names the tag the WAKE emits.
    ///
    /// Two vocabularies exist and they disagree: `WaitItem` derives
    /// `rename_all`, so its own serialization says `ad_hoc_review`,
    /// while the wake's `WaitJsonItem` renames that variant to
    /// `adhoc_review` — and the wake is what the stop hook parses.
    /// `WaitKind` models the wake, so this is the test that keeps it
    /// honest; comparing against `WaitItem`'s derive instead would
    /// have pinned the wrong spelling for three variants, and a kind
    /// that never matches classifies nothing
    /// (the-turn-end-is-one-decision).
    #[test]
    fn a_kind_names_the_tag_the_wake_emits() {
        use clank_core::wait::WaitKind;

        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        // One item per kind whose spelling could drift — the two
        // vocabularies disagree about, plus a control.
        let items = [
            WaitItem::AdHocReview {
                sha: sha.clone(),
                feedback_path: String::new(),
            },
            WaitItem::AdHocRevise { sha: sha.clone() },
            WaitItem::Idle {
                prompt: String::new(),
            },
        ];
        for item in items {
            let wake = serde_json::to_value(render_json(&item)).unwrap();
            let tag = wake.get("kind").and_then(|k| k.as_str()).expect("tagged");
            assert_eq!(
                item.kind().as_str(),
                tag,
                "{item:?} reports `{}` but the wake emits `{tag}`",
                item.kind().as_str(),
            );
            assert_eq!(
                WaitKind::parse(tag),
                Some(item.kind()),
                "and it parses back"
            );
        }
    }

    #[test]
    fn a_required_reply_is_explicit_in_the_human_wait_output() {
        let item: WaitItem = serde_json::from_value(serde_json::json!({
            "kind": "command_event", "name": "questions", "id": "utt/a",
            "event": "question", "content": "Which option?",
            "requires_reply": true, "selector": "command-questions-abcd@7"
        }))
        .unwrap();
        let line = render_human(&item);
        assert!(line.contains("reply required"), "{line}");
        assert!(
            line.contains("clank events ack command-questions-abcd@7 --reply <text>"),
            "{line}"
        );
        assert!(!line.contains("(ack command-questions-abcd@7)"), "{line}");
    }

    #[test]
    fn wait_json_items_serialize_to_the_stable_shape() {
        use clank_core::vocab::WaitingReason;
        use clank_core::wait::MasterNext;

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
                        unknown: Some("ghost".into()),
                        untagged_touched: vec![PlanKey::parse("real").unwrap()],
                        extra_named: Some(PlanKey::parse("extra").unwrap()),
                    },
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "fix_commit_tag",
                    "sha": sha("abc").as_str(),
                    "unknown": "ghost",
                    "untagged_touched": ["real"],
                    "extra_named": "extra",
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
                    question: "should we?".into(),
                }))
                .unwrap(),
                serde_json::json!({
                    "kind": "blocked",
                    "agent": "claude",
                    "name": "q",
                    "question": "should we?",
                }),
            ),
            (
                serde_json::to_value(render_json(&unblocked("q", "yes"))).unwrap(),
                serde_json::json!({
                    "kind": "unblocked",
                    "name": "q",
                    "answer": "yes",
                    "carries": {
                        "agent": "claude",
                        "name": "q",
                        "occurrence": "0123456789abcdef0123456789abcdef",
                        "question_digest": "qd",
                        "answer_digest": "ad",
                    },
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
    fn wait_json_envelope_wraps_items_under_items_key() {
        let item = WaitItem::Idle {
            prompt: "go".into(),
        };
        let envelope = WaitEnvelope {
            items: vec![render_json(&item)],
            source_cwd: None,
        };
        let got = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            got,
            serde_json::json!({ "items": [{ "kind": "idle", "prompt": "go" }] }),
        );
    }

    /// Through the real host: a root that exits on TERM leaves a
    /// descendant finishing accepted work, and what it prints when done
    /// is read, logged and sent before the drain is over.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_reads_what_work_the_root_left_running_produces() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let ready = root.path().join("ready");
        let worker = root.path().join("worker.sh");
        std::fs::write(
            &worker,
            format!(
                "trap '' TERM\ntouch '{}'\nsleep 3\nprintf '%s\\n' '{{\"id\":\"late\",\"content\":\"finished\"}}'\n",
                ready.display()
            ),
        )
        .unwrap();
        let src = command_source(
            "drained",
            &[
                "sh",
                "-c",
                &format!(
                    "sh '{}' & trap 'exit 0' TERM; while :; do sleep 0.1; done",
                    worker.display()
                ),
            ],
        );
        let key = src.source_key();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key.clone(),
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());

        let drained = owners
            .drain(Duration::from_secs(6))
            .finish(tokio::time::Instant::now() + Duration::from_secs(8))
            .await;
        assert_eq!(drained.end, crate::cli::source_group::DrainEnd::Complete);
        let drained = drained.outcomes;
        task.abort();
        let _ = task.await;
        assert_eq!(
            drained,
            vec![(
                crate::cli::command_source::producer_path(&events, &key),
                crate::cli::source_group::Drained::WithinGrace
            )]
        );
        let mut sent = Vec::new();
        while let Ok((item, _)) = rx.try_recv() {
            sent.push(format!("{item:?}"));
        }
        assert!(
            sent.iter().any(|item| item.contains("finished")),
            "never sent: {sent:?}"
        );
        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert!(
            rows.iter()
                .any(|row| format!("{:?}", row.item).contains("finished")),
            "never logged"
        );
    }

    /// Through the real host, asked to stop as a config edit asks: a
    /// producer that takes a second after TERM to finish what it was
    /// sending has that line read, logged and sent, and the source then
    /// ends, starting nothing more, with its lease free for whatever
    /// replaces it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_source_finishes_what_it_was_sending() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let ready = root.path().join("ready");
        let runs = root.path().join("runs");
        let mut src = command_source(
            "sending",
            &[
                "sh",
                "-c",
                &format!(
                    "echo run >> '{}'; trap 'sleep 1; printf \"%s\\n\" \"{{\\\"id\\\":\\\"late\\\",\\\"content\\\":\\\"finished-after-term\\\"}}\"; exit 0' TERM; touch '{}'; while :; do sleep 0.1; done",
                    runs.display(),
                    ready.display()
                ),
            ],
        );
        src.stop_grace = Some("8s".into());
        let key = src.source_key();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop, request) = StopRequest::channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key.clone(),
            crate::cli::command_source::reap_previous_producer,
            owners,
            request,
        ));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .expect("a stopped source kept running")
            .unwrap();
        let mut sent = Vec::new();
        while let Ok((item, _)) = rx.try_recv() {
            sent.push(format!("{item:?}"));
        }
        assert!(
            sent.iter().any(|item| item.contains("finished-after-term")),
            "never sent: {sent:?}"
        );
        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert!(
            rows.iter()
                .any(|row| format!("{:?}", row.item).contains("finished-after-term")),
            "never logged"
        );
        assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
        assert!(
            crate::cli::github::log::IngestLease::try_acquire(&events, &key)
                .unwrap()
                .is_some()
        );
    }

    /// Through the real host: a producer that has closed its stdout and
    /// lives on is still sent TERM when its source is stopped, and ends
    /// on it rather than holding its lease past its grace.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_reaches_a_producer_that_closed_its_stdout() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let ready = root.path().join("ready");
        let mut src = command_source(
            "silent",
            &[
                "sh",
                "-c",
                &format!(
                    "exec 1>&-; trap 'exit 0' TERM; touch '{}'; while :; do sleep 0.1; done",
                    ready.display()
                ),
            ],
        );
        src.stop_grace = Some("30s".into());
        let key = src.source_key();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop, request) = StopRequest::channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key.clone(),
            crate::cli::command_source::reap_previous_producer,
            owners,
            request,
        ));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());
        tokio::time::sleep(Duration::from_millis(200)).await;

        stop.send(true).unwrap();
        let mut task = task;
        let ended = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
        if ended.is_err() {
            task.abort();
            let _ = task.await;
            panic!("a stopped source with no stdout kept running");
        }
        assert!(
            crate::cli::github::log::IngestLease::try_acquire(&events, &key)
                .unwrap()
                .is_some()
        );
    }

    /// Through the real host: the failure event raised after three
    /// failed runs says how the LAST run ended and carries its stderr
    /// tail, sanitized. A run that flooded stderr is drained without
    /// blocking, and a descendant holding stderr open neither delays the
    /// restart nor puts its output into a later run's tail.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_failure_says_how_its_last_run_ended() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let runs = root.path().join("runs");
        let script = root.path().join("producer.sh");
        std::fs::write(
            &script,
            format!(
                "n=$(( $(cat '{runs}' 2>/dev/null || echo 0) + 1 )); echo $n > '{runs}'\n\
                 case $n in\n\
                 1) yes 'a short flood line' | head -c 5000000 >&2 ;;\n\
                 2) (echo 'from run 2s descendant' >&2; sleep 30 >&2) & echo 'run 2' >&2 ;;\n\
                 *) echo 'run 3 token=abc123' >&2 ;;\n\
                 esac\n\
                 exit 3\n",
                runs = runs.display()
            ),
        )
        .unwrap();
        let src = command_source("flaky", &["sh", &script.display().to_string()]);
        let key = src.source_key();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let started = std::time::Instant::now();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key,
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        let failure = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let (item, _) = rx.recv().await.expect("the source stopped sending");
                let shown = format!("{item:?}");
                if shown.contains("has stopped") {
                    return shown;
                }
            }
        })
        .await
        .expect("no failure event");
        let took = started.elapsed();
        task.abort();
        let _ = task.await;
        assert!(failure.contains("exit 3"), "{failure}");
        assert!(failure.contains("run 3"), "{failure}");
        assert!(!failure.contains("abc123"), "a token leaked: {failure}");
        assert!(
            !failure.contains("run 2") && !failure.contains("flood"),
            "{failure}"
        );
        assert!(
            took < Duration::from_secs(10),
            "a descendant holding stderr delayed the restarts: {took:?}"
        );
    }

    #[test]
    fn a_signalled_run_names_its_signal() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .unwrap();
        assert_eq!(
            crate::cli::source_diagnostic::Ended::of(Some(status)),
            crate::cli::source_diagnostic::Ended::Signal(9)
        );
    }

    /// A host whose registry closes while it backs off after a failed
    /// run stops: it starts no further producer, counts nothing, and
    /// raises no failure event.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_closing_host_stops_relaunching() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let runs = root.path().join("runs");
        let src = command_source(
            "flaky",
            &[
                "sh",
                "-c",
                &format!("echo run >> '{}'; exit 3", runs.display()),
            ],
        );
        let key = src.source_key();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key,
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        for _ in 0..500 {
            if runs.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(runs.exists(), "the first run never happened");
        // Into the backoff after that run, before the next launch.
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || owners.retire_all())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("a closing host kept relaunching")
            .unwrap();
        let ran = std::fs::read_to_string(&runs).unwrap().lines().count();
        assert!(ran <= 2, "it kept starting producers: {ran} runs");
        while let Ok((item, _)) = rx.try_recv() {
            assert!(
                !format!("{item:?}").contains("has stopped"),
                "a false failure event"
            );
        }
    }

    /// Closing while a run is still going ends the host with that run:
    /// no backoff is waited out, because nothing will be restarted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_ended_by_closing_is_not_backed_off() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let ready = root.path().join("ready");
        let src = command_source(
            "long",
            &[
                "sh",
                "-c",
                &format!("touch '{}'; exec sleep 300", ready.display()),
            ],
        );
        let key = src.source_key();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events.clone()),
            key,
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists(), "the run never started");
        tokio::task::spawn_blocking(move || owners.retire_all())
            .await
            .unwrap();
        let closed = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the host did not end")
            .unwrap();
        assert!(
            closed.elapsed() < crate::cli::command_source::BACKOFF_FIRST,
            "it waited out a backoff: {:?}",
            closed.elapsed()
        );
    }

    /// A host whose registry is already closed returns, though the
    /// previous producer it would otherwise keep waiting on still defers
    /// its retirement.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_host_does_not_wait_on_a_deferred_predecessor() {
        let owners = crate::cli::source_group::tests::owners();
        tokio::task::spawn_blocking(move || owners.retire_all())
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let src = command_source("any", &["sh", "-c", "exit 0"]);
        let key = src.source_key();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events),
            key,
            |_: &Path, _: &str| {
                crate::cli::command_source::Retired::Deferred("still settling".into())
            },
            owners,
            StopRequest::never(),
        ));
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("a closed host kept waiting on its predecessor")
            .unwrap();
    }

    /// Two real failures, then the registry closes during the backoff
    /// before the third attempt: no false third failure is raised.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_after_two_failures_raises_no_third() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let runs = root.path().join("runs");
        let src = command_source(
            "flaky",
            &[
                "sh",
                "-c",
                &format!("echo run >> '{}'; exit 3", runs.display()),
            ],
        );
        let key = src.source_key();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::from(tx),
            Some(events),
            key,
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        let count = || {
            std::fs::read_to_string(&runs)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        for _ in 0..600 {
            if count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(count(), 2, "two runs first");
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::task::spawn_blocking(move || owners.retire_all())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the host did not stop")
            .unwrap();
        assert_eq!(count(), 2, "a third producer started");
        while let Ok((item, _)) = rx.try_recv() {
            assert!(
                !format!("{item:?}").contains("has stopped"),
                "a false failure event"
            );
        }
    }
}
