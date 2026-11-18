use std::time::Duration;

use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use super::clients::{Clients, WAITERS_SETTLE};
use super::control::refused;
use super::hosted::{Reading, Sources, admissions, identities, roster_authors};
use super::retire::OwnedTasks;
use super::wire::{Control, Note};

mod transport;

pub(super) use self::transport::{Authorize, RecoveryAnswer};
#[cfg(test)]
use self::transport::{Located, Submitted, Typist, authorized, last_look, typed_send};
use self::transport::{act_on_recovery, authorize};

/// Exit watches on every registered run, so a run's end is an event the
/// loop records rather than something it looks for.
pub(super) struct RunExits {
    watch: Option<crate::proc_identity::exit_watch::ExitWatch<(AgentLabel, i32)>>,
    armed: Vec<(AgentLabel, clank_core::turn_policy::ShellProof)>,
    ended: tokio::sync::mpsc::UnboundedSender<Result<AgentLabel, String>>,
}

impl RunExits {
    /// `ended` hears each run's author as it ends, or why the watch on
    /// them failed.
    pub(super) fn new(
        ended: tokio::sync::mpsc::UnboundedSender<Result<AgentLabel, String>>,
    ) -> Self {
        use crate::proc_identity::exit_watch::Wake;
        let watch = crate::proc_identity::exit_watch::ExitWatch::new(None, {
            let ended = ended.clone();
            move |wake| match wake {
                Wake::Exited((author, _)) => {
                    let _ = ended.send(Ok(author));
                }
                Wake::Failed(why) => {
                    let _ = ended.send(Err(why));
                }
                Wake::Dir => {}
            }
        });
        if let Err(e) = &watch {
            eprintln!("monitor: cannot watch for runs ending: {e}");
        }
        RunExits {
            watch: watch.ok(),
            armed: Vec::new(),
            ended,
        }
    }

    /// Watch exactly `runs`. One already gone, or whose identity cannot
    /// be read, is said to have ended at once, so the loop looks at it.
    pub(super) fn track(&mut self, runs: Vec<(AgentLabel, clank_core::turn_policy::ShellProof)>) {
        use crate::proc_identity::exit_watch::Armed;
        let Some(watch) = &self.watch else {
            return;
        };
        for (author, proof) in self.armed.iter().filter(|run| !runs.contains(run)) {
            watch.disarm(&(author.clone(), proof.pid));
        }
        for (author, proof) in runs.iter().filter(|run| !self.armed.contains(run)) {
            let armed = serde_json::from_str::<crate::proc_identity::ProcToken>(&proof.token)
                .map(|token| watch.arm((author.clone(), proof.pid), proof.pid, &token))
                .unwrap_or(Armed::Unknown);
            if armed == Armed::Ended {
                let _ = self.ended.send(Ok(author.clone()));
            }
        }
        self.armed = runs;
    }
}

/// Every agent's turns and the recovery acting on them: the turns'
/// custody, decided here and nowhere else, recovery's driver for agents
/// whose turn failed with nothing left to wake them, the exits of their
/// registered runs, and the transports recovery's acts leave by and are
/// answered on.
///
/// Its receivers are open to the loop only for its `select!` to receive
/// from.
pub(super) struct Custody {
    serving_since: std::time::Instant,
    turns: crate::cli::turn_custody::Turns,
    records: crate::cli::recovery::RecordWatch,
    pub(super) changed_records:
        tokio::sync::mpsc::UnboundedReceiver<crate::cli::recovery::RecordEvent>,
    recovery: crate::cli::recovery::Driver,
    pub(super) authorizations: tokio::sync::mpsc::UnboundedReceiver<Authorize>,
    asks: tokio::sync::mpsc::UnboundedSender<Authorize>,
    pub(super) answered: tokio::sync::mpsc::UnboundedReceiver<RecoveryAnswer>,
    answers: tokio::sync::mpsc::UnboundedSender<RecoveryAnswer>,
    run_exits: RunExits,
    pub(super) runs_ended: tokio::sync::mpsc::UnboundedReceiver<Result<AgentLabel, String>>,
}

impl Custody {
    /// `run_exits` reports on `runs_ended`.
    pub(super) fn new(
        repo: &std::path::Path,
        run_exits: RunExits,
        runs_ended: tokio::sync::mpsc::UnboundedReceiver<Result<AgentLabel, String>>,
    ) -> Self {
        let (answers, answered) = tokio::sync::mpsc::unbounded_channel::<RecoveryAnswer>();
        let (asks, authorizations) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
        let mut recovery = crate::cli::recovery::Driver::new(
            repo.to_path_buf(),
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default(),
        );
        recovery.discover_pending(&crate::agent_store::agents_root(repo));
        let (record_changes, changed_records) = tokio::sync::mpsc::unbounded_channel();
        let records = crate::cli::recovery::RecordWatch::spawn(record_changes);
        let turns = crate::cli::turn_custody::Turns::new(repo.to_path_buf());
        Custody {
            serving_since: std::time::Instant::now(),
            turns,
            records,
            changed_records,
            recovery,
            authorizations,
            asks,
            answered,
            answers,
            run_exits,
            runs_ended,
        }
    }

    /// Custody of `turns`, with `recovery` acting on them, watching no
    /// run and no record.
    #[cfg(test)]
    pub(super) fn of(
        turns: crate::cli::turn_custody::Turns,
        recovery: crate::cli::recovery::Driver,
    ) -> Self {
        let (answers, answered) = tokio::sync::mpsc::unbounded_channel::<RecoveryAnswer>();
        let (asks, authorizations) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
        let (record_changes, changed_records) = tokio::sync::mpsc::unbounded_channel();
        let (ended, runs_ended) = tokio::sync::mpsc::unbounded_channel();
        Custody {
            serving_since: std::time::Instant::now(),
            turns,
            records: crate::cli::recovery::RecordWatch::spawn(record_changes),
            changed_records,
            recovery,
            authorizations,
            asks,
            answered,
            answers,
            run_exits: RunExits::new(ended),
            runs_ended,
        }
    }

    #[cfg(test)]
    pub(super) fn into_parts(
        self,
    ) -> (
        crate::cli::turn_custody::Turns,
        crate::cli::recovery::Driver,
    ) {
        (self.turns, self.recovery)
    }

    #[cfg(test)]
    pub(super) fn recovery_mut(&mut self) -> &mut crate::cli::recovery::Driver {
        &mut self.recovery
    }

    pub(super) fn track_runs(&mut self) {
        self.run_exits.track(self.turns.runs());
    }

    pub(super) fn recovery_due(&self) -> Option<tokio::time::Instant> {
        self.recovery
            .next_due()
            .into_iter()
            .chain(self.recovery.evidence_due())
            .min()
            .map(|at| {
                let wait = at.saturating_sub(crate::cli::recovery::unix_now());
                tokio::time::Instant::now() + Duration::from_secs(wait)
            })
    }

    /// When an attendance's check-in or a held report's hook falls due.
    pub(super) fn turns_due(&self, clients: &Clients) -> Option<tokio::time::Instant> {
        [self.turns.next_deadline(), clients.report_deadline()]
            .into_iter()
            .flatten()
            .min()
            .map(|at| {
                let wait = at.saturating_sub(crate::cli::turn_custody::now_ms());
                tokio::time::Instant::now() + Duration::from_millis(wait)
            })
    }

    /// When an agent with no waiter may first be said to have none: once
    /// the monitor has served long enough for its waits to arrive.
    pub(super) fn settles(&self) -> Option<tokio::time::Instant> {
        (self.serving_since.elapsed() < WAITERS_SETTLE)
            .then(|| tokio::time::Instant::from_std(self.serving_since + WAITERS_SETTLE))
    }

    /// Whether recovery's last fold met a log another process held.
    pub(super) fn awaits_a_complete_fold(&self) -> bool {
        self.recovery.awaits_a_complete_fold()
    }

    /// Carry out `acts` by their transports, a helper's through its
    /// connection among `clients`.
    pub(super) fn act(
        &self,
        repo: &std::path::Path,
        clients: &Clients,
        acts: Vec<crate::cli::recovery::Act>,
        owned: &mut OwnedTasks,
    ) {
        act_on_recovery(
            repo,
            clients,
            acts,
            &self.answers,
            &self.asks,
            &self.records,
            owned,
        );
    }

    /// The author whose presentation connection `conn` carries.
    pub(super) fn carrier(&self, conn: u64) -> Option<AgentLabel> {
        self.turns.carrier(conn).cloned()
    }

    /// Register `author`'s wait on `conn` as a deliverer.
    pub(super) fn watch(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        g: impl FnOnce() -> crate::cli::turn_custody::Gathered,
    ) {
        self.turns.watch(author, conn, g);
    }

    /// Take the questions a new Stop `report` of `author`'s carries.
    pub(super) fn observe_questions(
        &mut self,
        author: &AgentLabel,
        report: &crate::cli::turn_wire::Report,
    ) {
        self.turns.observe_questions(author, report);
    }

    /// The verdict for `report` on `conn`, stepped once more: `None` while
    /// it has none to give.
    pub(super) fn answer(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        report: &crate::cli::turn_wire::Report,
        first: bool,
        g: &crate::cli::turn_custody::Gathered,
    ) -> Option<crate::cli::turn_wire::Frame> {
        self.turns.answer(author, conn, report, first, g)
    }

    /// Connection `conn` of `author`'s closed: what it carried fails.
    pub(super) fn closed(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        g: &crate::cli::turn_custody::Gathered,
    ) {
        self.turns.closed(author, conn, g);
    }

    /// Connection `conn` of `author`'s ended: a deliverer still held, or
    /// one whose presentation it carried, answered nothing, and a park
    /// among them is lost. Published.
    pub(super) fn ended(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        g: &crate::cli::turn_custody::Gathered,
    ) {
        self.turns.closed(author, conn, g);
        self.turns.lost(author, conn, g);
        self.project(author, g);
    }

    /// `author` was woken past the Stop it reported.
    pub(super) fn woken(&mut self, author: &AgentLabel) {
        self.turns.woken(author);
    }

    /// Publish what the monitor holds of `author`'s turns: its attendance,
    /// question, repeat count and lost park.
    pub(super) fn project(&mut self, author: &AgentLabel, g: &crate::cli::turn_custody::Gathered) {
        self.recovery.project(
            author,
            self.turns.projection(author, g),
            crate::cli::recovery::unix_now(),
        );
    }

    /// Write what an agent's held lock left unwritten of its turn record
    /// and questions.
    pub(super) fn retry_unwritten(&mut self) {
        self.turns.retry_unwritten();
    }

    /// Retire `author`'s runs whose process is gone: whether any was, so
    /// what they held quiet is deliverable now.
    pub(super) fn end_gone_runs(
        &mut self,
        author: &AgentLabel,
        g: &crate::cli::turn_custody::Gathered,
    ) -> bool {
        self.turns.end_gone_runs(author, g)
    }

    /// [`end_gone_runs`] for every bound agent.
    pub(super) fn end_every_gone_run(&mut self, repo: &std::path::Path) {
        end_gone_runs(repo, &mut self.turns);
    }

    pub(super) fn fall_due(
        &mut self,
        now: u64,
        gathered: impl FnMut(&AgentLabel) -> crate::cli::turn_custody::Gathered,
    ) -> Vec<(AgentLabel, crate::cli::turn_custody::Gathered)> {
        due_check_ins(&mut self.turns, now, gathered)
    }

    /// The authors a presentation's carrier has had its time for by `now`.
    pub(super) fn overdue_authors(&self, now: u64) -> Vec<AgentLabel> {
        self.turns.overdue_authors(now)
    }

    /// Fail `author`'s overdue presentation: whether there was one.
    pub(super) fn expire(
        &mut self,
        author: &AgentLabel,
        g: &crate::cli::turn_custody::Gathered,
    ) -> bool {
        self.turns.expire(author, g)
    }

    /// Settle a result from connection `conn` against `author`'s custody:
    /// the commit a delivery makes.
    pub(super) fn result(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        result: crate::cli::turn_wire::TurnResult,
        g: &crate::cli::turn_custody::Gathered,
    ) -> Result<Option<crate::cli::turn_custody::Commit>, String> {
        self.turns.result(author, conn, result, g)
    }

    /// The installation may have moved, which a decision a stale build
    /// could not store waits on.
    pub(super) fn installed(
        &mut self,
        repo: &std::path::Path,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) {
        let acts = self.recovery.installed(crate::cli::recovery::unix_now());
        self.act(repo, clients, acts, owned);
    }

    #[cfg(test)]
    pub(super) fn facts(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        sources: &mut Sources,
    ) -> crate::cli::recovery::Observed {
        recovery_facts(repo, state, sources, &mut self.turns)
    }

    /// A held lock was let go: recovery folds again what it could not.
    pub(super) fn released(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        clients: &Clients,
        sources: &mut Sources,
        owned: &mut OwnedTasks,
    ) {
        let acts = self.recovery.released(
            recovery_facts(repo, state, sources, &mut self.turns),
            crate::cli::recovery::unix_now(),
        );
        self.act(repo, clients, acts, owned);
    }

    /// A fold completed: runs gone while no monitor watched are ended, and
    /// recovery is reconciled with it.
    pub(super) fn reconcile(
        &mut self,
        repo: &std::path::Path,
        state: &crate::repo_state::RepoState,
        clients: &Clients,
        sources: &mut Sources,
        owned: &mut OwnedTasks,
    ) {
        let now = crate::cli::recovery::unix_now();
        let acts = self
            .recovery
            .reconcile(recovery_facts(repo, state, sources, &mut self.turns), now);
        self.act(repo, clients, acts, owned);
    }

    /// Recovery's attempts fall due, each gated by [`due_gate`].
    pub(super) fn due(
        &mut self,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        clients: &Clients,
        sources: &mut Sources,
        owned: &mut OwnedTasks,
    ) {
        let turns = &mut self.turns;
        let acts = self
            .recovery
            .due(crate::cli::recovery::unix_now(), |author, session| {
                due_gate(repo, clients, turns, state, sources, author, session)
            });
        self.act(repo, clients, acts, owned);
    }

    /// A transport asks for the loop's last look: [`authorize`].
    pub(super) fn authorize(
        &mut self,
        repo: &std::path::Path,
        clients: &Clients,
        sources: &Sources,
        a: Authorize,
    ) {
        authorize(
            repo,
            clients,
            sources,
            &mut self.recovery,
            &mut self.turns,
            a,
        );
    }

    pub(super) fn answered(
        &mut self,
        owner: bool,
        repo: &std::path::Path,
        (author, attempt, answer): RecoveryAnswer,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) {
        let acts =
            self.recovery
                .answer(&author, &attempt, answer, crate::cli::recovery::unix_now());
        if owner {
            self.act(repo, clients, acts, owned);
        }
    }

    pub(super) fn records_changed(
        &mut self,
        owner: bool,
        repo: &std::path::Path,
        events: std::collections::BTreeSet<crate::cli::recovery::RecordEvent>,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) {
        let now = crate::cli::recovery::unix_now();
        let mut acts = Vec::new();
        for event in events {
            acts.extend(self.recovery.changed(event, now));
        }
        if owner {
            self.act(repo, clients, acts, owned);
        }
    }

    pub(super) fn reach(
        &mut self,
        repo: &std::path::Path,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) {
        let acts = self.recovery.reach(
            clients.waiters(self.serving_since.elapsed()),
            crate::cli::recovery::unix_now(),
        );
        self.act(repo, clients, acts, owned);
    }

    /// [`recovery_control`], acting on what recovery decides.
    pub(super) fn recovery_request(
        &mut self,
        repo: &std::path::Path,
        request: &Control,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) -> Result<String, String> {
        let (answered, acts) = match recovery_control(
            repo,
            &mut self.recovery,
            request,
            crate::cli::recovery::unix_now(),
        ) {
            Ok((said, acts)) => (Ok(said), acts),
            Err(why) => (Err(why), Vec::new()),
        };
        self.act(repo, clients, acts, owned);
        answered
    }

    /// [`turn_control`], publishing what it changed of the author's turns.
    pub(super) fn turn_request(
        &mut self,
        repo: &std::path::Path,
        request: &Control,
        state: Option<&crate::repo_state::RepoState>,
        sources: &mut Sources,
    ) -> Result<String, Note> {
        let answered = turn_control(repo, &mut self.turns, request, |author| {
            sources.gather(repo, state, author, &mut Reading::afresh())
        });
        answered.map(|(author, said)| {
            let g = sources.gather(repo, state, &author, &mut Reading::afresh());
            self.project(&author, &g);
            said
        })
    }

    /// [`user_command_look`] at `author`'s `session`, with the agents in
    /// `settling` still ending a released turn.
    pub(super) fn look_for_user_command(
        &mut self,
        repo: &std::path::Path,
        clients: &Clients,
        settling: &std::collections::BTreeSet<AgentLabel>,
        author: &AgentLabel,
        session: &clank_core::SessionId,
    ) -> crate::cli::user_command::dispatch::Look {
        user_command_look(
            repo,
            clients,
            &self.turns,
            &mut self.recovery,
            settling,
            author,
            session,
        )
    }

    /// The loop's agents, as a user-command pass sees them: `settling`
    /// still ending a released turn.
    pub(super) fn agents<'a>(
        &'a mut self,
        repo: &'a std::path::Path,
        clients: &'a Clients,
        settling: &'a std::collections::BTreeSet<AgentLabel>,
    ) -> impl crate::cli::user_command::dispatch::Agents + 'a {
        LoopAgents {
            repo,
            clients,
            turns: &self.turns,
            recovery: &mut self.recovery,
            settling,
        }
    }

    /// Have recovery read the records of the agents a user command waits
    /// on.
    pub(super) fn want_records(
        &mut self,
        repo: &std::path::Path,
        wanted: &std::collections::BTreeSet<AgentLabel>,
        clients: &Clients,
        owned: &mut OwnedTasks,
    ) {
        let acts = self.recovery.want_records(wanted);
        self.act(repo, clients, acts, owned);
    }

    /// The parks a verified user command alone stands in the way of, which
    /// their Stops are answered for.
    pub(super) fn release_for_user_commands(
        &mut self,
        released: std::collections::BTreeSet<AgentLabel>,
    ) {
        self.turns.set_user_commands(released);
    }
}

/// Carry out a recovery transport's request: record a turn the harness
/// reported, or apply a helper's answer, for a session still bound. What
/// to tell the requester, and what recovery decided.
fn recovery_control(
    repo: &std::path::Path,
    recovery: &mut crate::cli::recovery::Driver,
    request: &Control,
    now: u64,
) -> Result<(String, Vec<crate::cli::recovery::Act>), String> {
    let bound = |author: &str, tool, session: &str| {
        let author =
            AgentLabel::parse(author).map_err(|e| format!("invalid author `{author}`: {e}"))?;
        match crate::agent_store::binding_matches(repo, &author, tool, session) {
            true => Ok(author),
            false => Err("this session no longer holds the label".to_string()),
        }
    };
    match request {
        Control::Observe {
            author,
            tool,
            session,
            turn,
        } => {
            let author = bound(author, *tool, session)?;
            let dir = crate::agent_store::agents_root(repo).join(author.as_str());
            crate::cli::recovery::observe(&dir, turn)
                .map_err(|e| format!("the turn could not be recorded: {e}"))?;
            Ok((
                crate::cli::recovery::retry_announcement(*tool).to_string(),
                Vec::new(),
            ))
        }
        Control::RecoveryReply {
            author,
            tool,
            reply,
        } => {
            let author = bound(author, *tool, &reply.session)?;
            Ok((
                "recorded".into(),
                recovery.answered(&author, reply.clone(), now),
            ))
        }
        _ => Err("not a recovery request".into()),
    }
}

/// Carry out an attend or clear request against `author`'s turn state:
/// what was done, or the note that refuses it.
fn turn_control(
    repo: &std::path::Path,
    turns: &mut crate::cli::turn_custody::Turns,
    request: &Control,
    gathered: impl FnOnce(&AgentLabel) -> crate::cli::turn_custody::Gathered,
) -> Result<(AgentLabel, String), Note> {
    use clank_core::turn_policy::AttendRequest;
    let (author, request) = match request {
        Control::Run {
            author,
            session,
            desc,
            expect_ms,
            proof,
        } => (
            author,
            AttendRequest::Run {
                session: session.clone(),
                proof: proof.clone(),
                desc: desc.clone(),
                expect: *expect_ms,
            },
        ),
        Control::Attend {
            author,
            session,
            name,
            desc,
            expect_ms,
        } => (
            author,
            AttendRequest::Declare {
                session: session.clone(),
                name: name.clone(),
                desc: desc.clone(),
                expect: *expect_ms,
            },
        ),
        Control::Clear {
            author,
            session,
            which,
        } => (
            author,
            AttendRequest::Clear {
                session: session.clone(),
                which: which.clone(),
            },
        ),
        Control::Questions { .. }
        | Control::Restart { .. }
        | Control::Observe { .. }
        | Control::RecoveryReply { .. } => {
            return Err(refused("not a turn request".into()));
        }
    };
    let author = AgentLabel::parse(author)
        .map_err(|e| refused(format!("invalid author `{author}`: {e}")))?;
    crate::agent_store::resolve_role(repo, &author)
        .map_err(|e| refused(format!("`{author}` is not on this repo's roster: {e:#}")))?;
    let g = gathered(&author);
    turns
        .attend(&author, request, &g)
        .map(|said| (author, said))
        .map_err(refused)
}

/// What the monitor knows about each bound roster agent, for recovery.
///
/// What an agent is owed is what a newly arriving waiter would be handed,
/// derived as a delivery would derive it but into a set no client owns,
/// and kept as a ledger: each item's obligation, and whether it reached
/// the session bound now. It is derived whether or not a deliverer is
/// registered: a wait that is there and has not been handed the work is
/// not a delivery.
///
/// An agent one of whose logs is held by another process is unknown for
/// the pass: its ledger is neither folded nor stored, so nothing it is
/// owed is discharged, and recovery decides nothing for it.
fn recovery_facts(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    sources: &mut Sources,
    turns: &mut crate::cli::turn_custody::Turns,
) -> crate::cli::recovery::Observed {
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Recovery acts on what the config says, so a config it cannot read
    // makes no decisions rather than ones under the defaults.
    let config = match crate::cli::config::load_with_home(repo, home.as_deref()) {
        Ok(config) => config.recovery,
        Err(error) => {
            tracing::warn!(repo = %repo.display(), error = %format!("{error:#}"), "recovery: config unreadable; skipping");
            return Default::default();
        }
    };
    let mut facts = Vec::new();
    let mut unknown = std::collections::BTreeSet::new();
    let mut bound = std::collections::BTreeSet::new();
    for author in roster_authors(repo) {
        let Ok(cfg) = crate::agent_store::load_agent_config(repo, &author) else {
            continue;
        };
        let dir = crate::agent_store::agents_root(repo).join(author.as_str());
        let Ok(Some(session)) = crate::agent_store::binding(repo, &author) else {
            continue;
        };
        bound.insert(author.clone());
        let auto = matches!(
            crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref()),
            Ok(clank_core::AutoMode::On)
        );
        let sidecars = crate::cli::github::log::Sidecars::never_waiting();
        let (admitted, status) =
            sources.admitted(repo, state, &author, &reviews, head.as_ref(), &sidecars);
        let owed = crate::cli::wait::presented(admitted.clone());
        let ended = crate::cli::block::ended(repo);
        let admissions = admissions(repo, &author, status.as_ref(), admitted, &ended, &sidecars);
        if sidecars.contended() {
            unknown.insert(author);
            continue;
        }
        let cwd = std::env::current_dir().ok();
        let g =
            crate::cli::turn_custody::Gathered::of(repo, &author, Some((&owed, cwd.as_deref())));
        if turns.counting(&author) {
            turns.observed(&author, &g);
        }
        let quiet = turns.recovery_quiet(&author, &g);
        let projection = turns.projection(&author, &g);
        let role = crate::agent_store::resolve_role(repo, &author)
            .map(|r| r.to_string())
            .unwrap_or_default();
        let evidence = crate::cli::ledger::Evidence {
            delivered: crate::cli::presentation::delivered_to(&dir, session.id.as_str()),
            in_flight: turns.in_flight(&author),
        };
        let ledger = crate::cli::ledger::fold_stored(
            &dir,
            &admissions,
            &evidence,
            &ended,
            crate::cli::recovery::unix_now(),
        );
        facts.push(crate::cli::recovery::Facts {
            agent_dir: dir,
            work: identities(&owed),
            blocked_on: ledger
                .owed()
                .is_empty()
                .then(|| crate::cli::wait::repo_block(repo))
                .flatten(),
            author,
            session,
            auto,
            ledger,
            role,
            blind_nudge: config.blind_nudge,
            stall: config.stall(),
            attended: quiet == Some(clank_core::turn_policy::Quiet::Attending),
            asking: projection.asking.is_some(),
            projection,
        });
    }
    turns.retire_unbound(&bound);
    crate::cli::recovery::Observed { facts, unknown }
}

/// [`recovery_gate`] for an attempt falling due, against the work the
/// latest fold says is owed. Recovery reconciles only after a fold, so with
/// none yet there is nothing it may act on, nor while what is owed is
/// unknown.
#[allow(clippy::too_many_arguments)]
fn due_gate(
    repo: &std::path::Path,
    clients: &Clients,
    turns: &mut crate::cli::turn_custody::Turns,
    state: Option<&crate::repo_state::RepoState>,
    sources: &mut Sources,
    author: &AgentLabel,
    session: &str,
) -> crate::cli::recovery::Gate {
    let Some(state) = state else {
        return crate::cli::recovery::Gate::Hold;
    };
    let Some(owed) = sources.owed_now(repo, state, author) else {
        return crate::cli::recovery::Gate::Contended;
    };
    recovery_gate(repo, clients, turns, author, session, &owed)
}

/// Whether a due recovery attempt may go ahead now, from the agent as it
/// is at this moment rather than at the last fold: never over its own
/// pending harness question, an attendance that quiets all of `owed`, a
/// presentation still with its carrier or a report not yet answered, and
/// never once a new binding or auto mode turned off has made it someone
/// else's call. `owed` is what the agent is owed now, never a stand-in.
///
/// Repository blocks already withhold review work from `owed`; unrelated
/// events remain recoverable. Nor is a wait that is merely registered: one that has been
/// answered and holds none of `owed` is not delivering it.
fn recovery_gate(
    repo: &std::path::Path,
    clients: &Clients,
    turns: &mut crate::cli::turn_custody::Turns,
    author: &AgentLabel,
    session: &str,
    owed: &[WaitItem],
) -> crate::cli::recovery::Gate {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    recovery_gate_in(repo, home.as_deref(), clients, turns, author, session, owed)
}

fn recovery_gate_in(
    repo: &std::path::Path,
    home: Option<&std::path::Path>,
    clients: &Clients,
    turns: &mut crate::cli::turn_custody::Turns,
    author: &AgentLabel,
    session: &str,
    owed: &[WaitItem],
) -> crate::cli::recovery::Gate {
    use crate::cli::recovery::Gate;
    let Ok(cfg) = crate::agent_store::load_agent_config(repo, author) else {
        return Gate::SessionChanged;
    };
    if crate::agent_store::binding(repo, author)
        .ok()
        .flatten()
        .is_none_or(|s| s.id.as_str() != session)
    {
        return Gate::SessionChanged;
    }
    if !matches!(
        crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home),
        Ok(clank_core::AutoMode::On)
    ) {
        return Gate::AutoOff;
    }
    if clients.awaits_verdict(author)
        || turns.presenting(author)
        || (owed.is_empty() && crate::cli::wait::repo_block(repo).is_some())
    {
        return Gate::Hold;
    }
    let cwd = std::env::current_dir().ok();
    let g = crate::cli::turn_custody::Gathered::of(repo, author, Some((owed, cwd.as_deref())));
    match turns.recovery_quiet(author, &g) {
        Some(_) => Gate::Hold,
        None => Gate::Go,
    }
}

/// Retire every bound agent's `clank run` whose process is gone. Each
/// fold asks, so the first fold after a restart ends those that went
/// while no monitor watched; a run that ends while one does is reported
/// by its exit ([`RunExits`]), and [`retire_gone_runs`] retires it.
fn end_gone_runs(repo: &std::path::Path, turns: &mut crate::cli::turn_custody::Turns) {
    for author in roster_authors(repo) {
        let g = crate::cli::turn_custody::Gathered::of(repo, &author, None);
        turns.end_gone_runs(&author, &g);
    }
}

/// Step every agent with a check-in due by `now`, gathering each as it is
/// evaluated: what was gathered. Each due entry is then owed its check-in,
/// or is retired with its binding, so none is due again.
fn due_check_ins(
    turns: &mut crate::cli::turn_custody::Turns,
    now: u64,
    mut gathered: impl FnMut(&AgentLabel) -> crate::cli::turn_custody::Gathered,
) -> Vec<(AgentLabel, crate::cli::turn_custody::Gathered)> {
    turns
        .due_authors(now)
        .into_iter()
        .map(|author| {
            let g = gathered(&author);
            turns.fall_due(&author, &g);
            (author, g)
        })
        .collect()
}

/// The loop's agents, as a user-command pass sees them.
struct LoopAgents<'a> {
    repo: &'a std::path::Path,
    clients: &'a Clients,
    turns: &'a crate::cli::turn_custody::Turns,
    recovery: &'a mut crate::cli::recovery::Driver,
    settling: &'a std::collections::BTreeSet<AgentLabel>,
}

impl crate::cli::user_command::dispatch::Agents for LoopAgents<'_> {
    fn binding(&self, label: &AgentLabel) -> Option<(clank_core::Tool, clank_core::SessionId)> {
        crate::agent_store::binding(self.repo, label)
            .ok()
            .flatten()
            .map(|s| (s.tool, s.id))
    }

    fn look(
        &mut self,
        label: &AgentLabel,
        session: &clank_core::SessionId,
    ) -> crate::cli::user_command::dispatch::Look {
        user_command_look(
            self.repo,
            self.clients,
            self.turns,
            self.recovery,
            self.settling,
            label,
            session,
        )
    }
}

/// Whether `author` sits idle for a user command now: no question, block or
/// presentation pending, no hook of `session` held here, no released park
/// still settling, and its record's latest turn completed.
fn user_command_look(
    repo: &std::path::Path,
    clients: &Clients,
    turns: &crate::cli::turn_custody::Turns,
    recovery: &mut crate::cli::recovery::Driver,
    settling: &std::collections::BTreeSet<AgentLabel>,
    author: &AgentLabel,
    session: &clank_core::SessionId,
) -> crate::cli::user_command::dispatch::Look {
    use crate::cli::user_command::dispatch::Look;
    let dir = crate::agent_store::agents_root(repo).join(author.as_str());
    if crate::cli::question_state::asking(&dir, session.as_str()).is_some() {
        return Look::Wait("a question to the user is pending".into());
    }
    if crate::cli::wait::blocked(repo, author) {
        return Look::Wait("a block is pending".into());
    }
    if turns.presenting(author) {
        return Look::Wait("a wake is being presented to it".into());
    }
    if clients.holds_report(author, session) {
        return Look::Parked;
    }
    if settling.contains(author) {
        return Look::Wait("its released turn is ending".into());
    }
    match recovery.turn_completed(author, session.as_str()) {
        Ok(()) => Look::Go,
        Err(why) => Look::Wait(why),
    }
}

#[cfg(test)]
#[path = "custody_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "grok_tests.rs"]
mod grok_tests;

#[cfg(test)]
#[path = "turn_tests.rs"]
mod turn_tests;

#[cfg(test)]
#[path = "unwatched_tests.rs"]
mod unwatched_tests;

#[cfg(test)]
#[path = "reach_tests.rs"]
mod reach_tests;
