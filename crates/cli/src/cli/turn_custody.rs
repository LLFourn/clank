//! The monitor's custody of every agent's turn-ends.
//!
//! One [`SessionState`] per agent, keyed by its bound session, decides
//! what each Stop, attendance request, check-in and delivery result means
//! ([`clank_core::turn_policy`]). The monitor owns it alone: it persists
//! it beside the agent, renders every verdict's text, and commits a
//! presentation's receipts only on the `Delivered` its own connection
//! carries. The hooks report evidence and frame what they are told.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clank_core::turn_policy::{
    AttendReply, AttendRequest, Attending, CheckIn, Effect, Entry, Event, Facts, Handoff, Liveness,
    Ms, Output, OwedItem, Persisted, Phase, Presence, PresentationId, Qualifies, Quiet,
    ResultOutcome, SessionState, ShellProof, SilentWhy, TaskSnapshot, Unqualified, Unwatched,
    Verdict,
};
use clank_core::{AgentLabel, AutoMode, Role, SessionId, SilentReason, Tool};

use super::turn_wire::{Frame, Report, TurnResult, WireVerdict};
use super::wake_render::WaitItem;

/// Unix milliseconds now.
pub(crate) fn now_ms() -> Ms {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Ms)
        .unwrap_or(0)
}

/// What the monitor knows about one agent at the moment it evaluates an
/// event. Gathered afresh for every step; nothing here is cached.
pub(crate) struct Gathered {
    now: Ms,
    binding: Option<(Tool, SessionId)>,
    auto: Result<AutoMode, String>,
    role: Result<Role, String>,
    asking: Option<String>,
    blocked: bool,
    /// `None` before the monitor has folded the repo, when what is owed
    /// is not known.
    owed: Option<Vec<(OwedItem, WaitItem)>>,
    /// The monitor holds a live registration of this agent's session's
    /// background `clank wait`. Never true without a monitor.
    waiter: bool,
}

impl Gathered {
    /// The agent as it is now, owing `owed` (`None` when not yet known).
    pub(crate) fn of(
        repo: &Path,
        author: &AgentLabel,
        owed: Option<(&[clank_core::wait::WaitItem], Option<&Path>)>,
    ) -> Self {
        let config = crate::agent_store::load_agent_config(repo, author);
        let binding = config
            .as_ref()
            .ok()
            .and_then(|c| c.as_ref())
            .and_then(|c| c.session.as_ref())
            .map(|s| (s.tool, s.id.clone()));
        let home = match &binding {
            Some((Tool::Grok, _)) => super::grok_setup::launch_home(repo, author),
            _ => std::env::var_os("HOME").map(PathBuf::from),
        };
        let auto = config
            .as_ref()
            .map_err(|e| format!("{e:#}"))
            .and_then(|cfg| {
                super::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref())
                    .map_err(|e| format!("{e:#}"))
            });
        let role =
            crate::agent_store::resolve_role(repo, author).map_err(|e| role_failure(author, &e));
        let dir = crate::agent_store::agents_root(repo).join(author.as_str());
        let asking = binding
            .as_ref()
            .and_then(|(_, s)| super::question_state::asking(&dir, s.as_str()))
            .map(|q| q.header);
        Gathered {
            now: now_ms(),
            binding,
            auto,
            role,
            asking,
            blocked: super::wait::blocked(repo, author),
            owed: owed.map(|(items, cwd)| owed_items(items, cwd)),
            waiter: false,
        }
    }

    /// The same agent, with the monitor's word on whether its session's
    /// background wait is registered.
    pub(crate) fn waiting(mut self, registered: bool) -> Self {
        self.waiter = registered;
        self
    }

    /// What a hook with no monitor says to `session`: `notice`, unless
    /// clank does not drive it.
    pub(crate) fn unmonitored_verdict(
        &self,
        session: &SessionId,
        notice: clank_core::turn_policy::Notice,
    ) -> Verdict {
        clank_core::turn_policy::unmonitored_verdict(
            session,
            &facts(self, None, &[], &Process),
            notice,
        )
    }

    /// Grok's native Stop for `session`, as the monitor would answer it
    /// before its listing is applied to any declaration.
    pub(crate) fn handoff_verdict(&self, session: &SessionId, continued: bool) -> Verdict {
        clank_core::turn_policy::handoff_verdict(
            &self.handoff(session, continued, TaskSnapshot::Unavailable, None),
            &facts(self, Some(Tool::Grok), &[], &Process),
        )
    }

    fn handoff(
        &self,
        session: &SessionId,
        continued: bool,
        tasks: TaskSnapshot,
        hook_started: Option<Ms>,
    ) -> Handoff {
        Handoff {
            session: session.clone(),
            wait_registered: self.waiter,
            continued,
            tasks,
            hook_started,
        }
    }

    /// The header of the question the agent is waiting on, if any.
    pub(crate) fn asking(&self) -> Option<&str> {
        self.asking.as_deref()
    }

    /// The same agent, judged as of `now`: a round that found a deadline
    /// passed at `now` steps it at that same moment.
    pub(crate) fn at(mut self, now: Ms) -> Self {
        self.now = now;
        self
    }

    fn owed_now(&self, custody: Option<&SessionState>) -> Vec<OwedItem> {
        match &self.owed {
            Some(owed) => owed.iter().map(|(o, _)| o.clone()).collect(),
            // Unknown is not "nothing": standing in the counted items
            // keeps an unfolded step from ending the repeat episode.
            None => custody
                .and_then(|s| s.repeat.as_ref())
                .map(|r| {
                    r.items
                        .iter()
                        .map(|identity| OwedItem {
                            identity: identity.clone(),
                            receipts: Vec::new(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// The monitor's words for a role it cannot resolve: it arms nothing and
/// says so, because a guessed role arms a poll that can never return.
pub(crate) fn role_failure(author: &AgentLabel, e: &anyhow::Error) -> String {
    format!(
        "hook: cannot resolve `{}`'s role ({e:#}); arming no wait. Fix the roster and the next \
         turn-end will arm one.",
        author.as_str()
    )
}

/// Each owed item with the receipts a presentation of it would commit.
fn owed_items(
    items: &[clank_core::wait::WaitItem],
    source_cwd: Option<&Path>,
) -> Vec<(OwedItem, WaitItem)> {
    super::wake_render::mirror(items, source_cwd)
        .into_iter()
        .map(|item| {
            let mut receipts: Vec<String> =
                super::wake_render::selected_presentations(std::iter::once(&item))
                    .iter()
                    .map(|p| p.selector())
                    .collect();
            receipts.extend(
                super::wake_render::receipt_candidates(std::iter::once(&item))
                    .map(|c| c.selector.to_string()),
            );
            (
                OwedItem {
                    identity: item.identity(),
                    receipts,
                },
                item,
            )
        })
        .collect()
}

/// Whether a recorded process is still the one recorded, asked when a
/// transition evaluates it.
pub(crate) struct Process;

impl Liveness for Process {
    fn presence(&self, proof: &ShellProof) -> Presence {
        presence_of(proof, crate::proc_identity::inspect::inspect)
    }
}

/// What `inspect` says of `proof`'s process, by the kernel's own
/// distinctions: only an explicit end or another birth is gone. A token
/// this build cannot read, and any read that fails, claim nothing.
fn presence_of(
    proof: &ShellProof,
    inspect: impl FnOnce(
        i32,
        &crate::proc_identity::ProcToken,
    ) -> crate::proc_identity::inspect::Inspection,
) -> Presence {
    use crate::proc_identity::inspect::{Inspection, ProcessState};
    let Ok(token) = serde_json::from_str::<crate::proc_identity::ProcToken>(&proof.token) else {
        return Presence::Unknown;
    };
    match inspect(proof.pid, &token) {
        Inspection::Verified(details)
            if matches!(details.state, Ok(ProcessState::Zombie | ProcessState::Dead)) =>
        {
            Presence::Gone
        }
        Inspection::Verified(_) => Presence::Live,
        Inspection::Mismatched | Inspection::Ended => Presence::Gone,
        Inspection::Unavailable(_) => Presence::Unknown,
    }
}

/// A process's proof as the model carries it.
pub(crate) fn shell_proof(pid: i32, token: &crate::proc_identity::ProcToken) -> ShellProof {
    ShellProof {
        pid,
        token: serde_json::to_string(token).unwrap_or_default(),
    }
}

fn facts<'a>(
    g: &'a Gathered,
    tool: Option<Tool>,
    owed: &'a [OwedItem],
    live: &'a dyn Liveness,
) -> Facts<'a> {
    Facts {
        now: g.now,
        binding: g
            .binding
            .as_ref()
            .filter(|(bound, _)| tool.is_none_or(|t| t == *bound))
            .map(|(_, s)| s),
        auto: g.auto.clone(),
        role: g.role.clone(),
        asking: g.asking.is_some(),
        blocked: g.blocked,
        owed,
        liveness: live,
        user_command: false,
    }
}

/// A presentation in flight, and what its `Delivered` commits.
pub(crate) struct Carried {
    conn: u64,
    id: PresentationId,
    tool: Tool,
    session: SessionId,
    delivered: super::presentation::Carried,
    prepared: super::wake_receipt::Prepared,
    /// Taken before the presentation left, so its result is recorded and
    /// its receipts written even if a newer clank is installed meanwhile.
    settling: crate::upgrade::Permit,
}

/// What a `Delivered` commits: carried out by the monitor off its loop.
pub(crate) struct Commit {
    pub(crate) author: AgentLabel,
    carried: Carried,
}

impl Commit {
    /// The items the delivery discharged: a transient one, kept by the
    /// monitor until now, is owed no longer.
    pub(crate) fn presented(&self) -> &[String] {
        &self.carried.delivered.identities
    }

    /// Record the presentation and run its receipt callbacks, while the
    /// session that saw it still holds the label.
    pub(crate) async fn run(self, repo: &Path) {
        let Commit { author, carried } = self;
        let current = || {
            crate::agent_store::binding_matches(
                repo,
                &author,
                carried.tool,
                carried.session.as_str(),
            )
        };
        if !current() {
            return;
        }
        super::presentation::record_delivered(
            repo,
            &author,
            carried.tool,
            carried.session.as_str(),
            &carried.delivered,
            current,
            &carried.settling,
        );
        super::wake_receipt::dispatch(
            repo,
            &author,
            carried.prepared,
            carried.tool,
            current,
            &carried.settling,
        )
        .await;
    }
}

struct Custody {
    state: SessionState,
    carried: Option<Carried>,
    /// The newest of its deliverers' connections: its park.
    park: Option<u64>,
    stored: Option<Persisted>,
}

fn refused_as_stale(e: &std::io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<crate::upgrade::Stale>())
}

/// Every agent's turn-end state, as the monitor's loop drives it.
pub(crate) struct Turns {
    repo: PathBuf,
    agents: BTreeMap<AgentLabel, Custody>,
    /// The agents whose park a verified user command is waiting on.
    user_commands: std::collections::BTreeSet<AgentLabel>,
    authority: crate::upgrade::Authority,
}

fn store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("turn.json")
}

/// The turn state saved in `agent_dir`, `None` when there is none or it
/// cannot be read: an unreadable record starts the binding afresh.
pub(crate) fn load_stored(agent_dir: &Path) -> Option<Persisted> {
    std::fs::read(store_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

pub(crate) fn write_stored(
    agent_dir: &Path,
    state: &Persisted,
    _permit: &crate::upgrade::Permit,
) -> std::io::Result<()> {
    let path = store_path(agent_dir);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(agent_dir)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

impl Turns {
    pub(crate) fn new(repo: PathBuf) -> Self {
        Turns {
            repo,
            agents: BTreeMap::new(),
            user_commands: Default::default(),
            authority: crate::upgrade::process().clone(),
        }
    }

    /// Answer to `authority` instead of this process's own.
    #[cfg(test)]
    pub(crate) fn guarded_by(mut self, authority: crate::upgrade::Authority) -> Self {
        self.authority = authority;
        self
    }

    fn agent_dir(&self, author: &AgentLabel) -> PathBuf {
        crate::agent_store::agents_root(&self.repo).join(author.as_str())
    }

    /// `author`'s state for its binding now: loaded from disk the first
    /// time, and started afresh when the binding has moved on.
    fn custody(&mut self, author: &AgentLabel, g: &Gathered) -> Option<&mut Custody> {
        let Some((_, bound)) = g.binding.as_ref() else {
            self.retire(author);
            return None;
        };
        if !self.agents.contains_key(author) {
            let stored = load_stored(&self.agent_dir(author));
            let state = match &stored {
                Some(p) if &p.binding == bound => SessionState::restored(p.clone()),
                _ => SessionState::new(bound.clone()),
            };
            self.agents.insert(
                author.clone(),
                Custody {
                    state,
                    carried: None,
                    park: None,
                    stored,
                },
            );
        }
        let custody = self.agents.get_mut(author)?;
        if &custody.state.binding != bound {
            custody.state.rebind(bound.clone());
            custody.carried = None;
            custody.park = None;
        }
        Some(custody)
    }

    /// `author` holds no binding: its attendance, the check-in that owes,
    /// its repeat count and any presentation in flight all described a
    /// session that is no longer its, so they end here and on disk.
    pub(crate) fn retire(&mut self, author: &AgentLabel) {
        self.agents.remove(author);
        // Left for the monitor that follows, which retires it the same way.
        let Ok(_permit) = self.authority.permit() else {
            return;
        };
        match std::fs::remove_file(store_path(&self.agent_dir(author))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                eprintln!("monitor: could not retire `{author}`'s turn state: {e}")
            }
            _ => {}
        }
    }

    /// Retire every agent held here that `bound` no longer names.
    pub(crate) fn retire_unbound(&mut self, bound: &std::collections::BTreeSet<AgentLabel>) {
        let gone: Vec<AgentLabel> = self
            .agents
            .keys()
            .filter(|author| !bound.contains(*author))
            .cloned()
            .collect();
        for author in gone {
            self.retire(&author);
        }
    }

    fn persist(&mut self, author: &AgentLabel, held: Option<&crate::upgrade::Permit>) {
        match self.try_persist(author, held) {
            Ok(()) => {}
            Err(e) if refused_as_stale(&e) => {
                tracing::debug!(%author, %e, "turn state left unwritten")
            }
            Err(e) => eprintln!("monitor: could not record `{author}`'s turn state: {e}"),
        }
    }

    /// Write `author`'s state if it changed, under `held`, else under its
    /// presentation in flight's permit, else under a new one.
    fn try_persist(
        &mut self,
        author: &AgentLabel,
        held: Option<&crate::upgrade::Permit>,
    ) -> std::io::Result<()> {
        let dir = self.agent_dir(author);
        let authority = &self.authority;
        let Some(custody) = self.agents.get_mut(author) else {
            return Ok(());
        };
        let now = custody.state.persisted();
        if custody.stored.as_ref() == Some(&now) {
            return Ok(());
        }
        let fresh;
        let permit = match held.or(custody.carried.as_ref().map(|c| &c.settling)) {
            Some(permit) => permit,
            None => {
                fresh = authority.permit()?;
                &fresh
            }
        };
        write_stored(&dir, &now, permit)?;
        custody.stored = Some(now);
        Ok(())
    }

    fn step(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        tool: Option<Tool>,
        event: Event,
    ) -> Option<Output> {
        let out = self.step_in_memory(author, g, tool, event);
        self.persist(author, None);
        out
    }

    fn step_under(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        tool: Option<Tool>,
        event: Event,
        permit: &crate::upgrade::Permit,
    ) -> Option<Output> {
        let out = self.step_in_memory(author, g, tool, event);
        self.persist(author, Some(permit));
        out
    }

    fn step_in_memory(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        tool: Option<Tool>,
        event: Event,
    ) -> Option<Output> {
        let releasing = self.user_commands.contains(author);
        let custody = self.custody(author, g)?;
        let owed = g.owed_now(Some(&custody.state));
        Some(custody.state.step(
            event,
            &Facts {
                user_command: releasing,
                ..facts(g, tool, &owed, &Process)
            },
        ))
    }

    /// Apply a subagent's report to what `author` attends, and say whether
    /// it was its bound session's. Needs no fold: it presents nothing.
    fn child(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        tool: Tool,
        seen: &clank_core::turn_policy::ChildReport,
        permit: &crate::upgrade::Permit,
    ) -> bool {
        let Some(custody) = self.custody(author, g) else {
            return false;
        };
        let recorded = custody
            .state
            .child(seen, &facts(g, Some(tool), &[], &Process));
        self.persist(author, Some(permit));
        recorded
    }

    /// The agents whose next Stop, or parked report, ends rather than parks
    /// because a verified user command waits for them. Replaces the last set.
    pub(crate) fn set_user_commands(&mut self, waiting: std::collections::BTreeSet<AgentLabel>) {
        self.user_commands = waiting;
    }

    /// Whether `author`'s custody holds a presentation in flight.
    pub(crate) fn presenting(&self, author: &AgentLabel) -> bool {
        self.agents
            .get(author)
            .is_some_and(|c| c.state.outstanding.is_some())
    }

    /// Apply what a Stop proves about the agent's questions, before its
    /// facts are gathered: the question it ended is over.
    pub(crate) fn observe_questions(&self, author: &AgentLabel, report: &Report) {
        if let Report::TurnEnd { questions, .. } = report
            && !questions.is_empty()
        {
            super::question_state::observe(&self.agent_dir(author), questions.clone());
        }
    }

    /// Answer `report`, carried by connection `conn`. `first` is the
    /// report's first step; a parked one is stepped again later. `None`
    /// when what is owed is not known yet: the report waits for a fold.
    pub(crate) fn answer(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        report: &Report,
        first: bool,
        g: &Gathered,
    ) -> Option<Frame> {
        // Before anything is stepped: a stale monitor answers nothing, so
        // the report is still whole for the monitor that follows it.
        let permit = self.authority.permit().ok()?;
        let event = match report {
            Report::Handoff {
                session,
                continued,
                tasks,
                hook_started,
                ..
            } => {
                let event = Event::NativeTurnEnd(g.handoff(
                    session,
                    *continued,
                    tasks.clone(),
                    *hook_started,
                ));
                let stepped = first
                    .then(|| self.step_under(author, g, Some(Tool::Grok), event, &permit))
                    .flatten();
                let verdict = match stepped {
                    Some(Output::Verdict(verdict)) => verdict,
                    _ => g.handoff_verdict(session, *continued),
                };
                return Some(self.frame(author, conn, report, verdict, permit, g));
            }
            Report::Child { tool, seen, .. } => {
                let recorded = first && self.child(author, g, *tool, seen, &permit);
                return Some(Frame::bare(match recorded {
                    true => WireVerdict::Recorded,
                    false => WireVerdict::Silent {
                        why: SilentReason::StaleBinding,
                    },
                }));
            }
            _ if g.owed.is_none() => return None,
            Report::TurnEnd { stop, .. } if first => Event::TurnEnd(stop.clone()),
            Report::TurnEnd { stop, .. } => Event::Parked(stop.clone()),
            Report::Compacted { stop, .. } => Event::Compacted(stop.clone()),
            Report::SessionStart { session, .. } => Event::SessionStart(session.clone()),
        };
        let tool = report.tool();
        let Some(Output::Verdict(verdict)) = self.step_under(author, g, Some(tool), event, &permit)
        else {
            return Some(Frame::bare(WireVerdict::Silent {
                why: SilentReason::StaleBinding,
            }));
        };
        Some(self.frame(author, conn, report, verdict, permit, g))
    }

    /// `verdict` as `report`'s carrier on `conn` is told it. A presentation
    /// is held as carried by `conn` under `permit` until its result.
    fn frame(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        report: &Report,
        verdict: Verdict,
        permit: crate::upgrade::Permit,
        g: &Gathered,
    ) -> Frame {
        let tool = report.tool();
        match verdict {
            Verdict::Deliver {
                presentation,
                identities,
                repeat,
                escalated,
                notices,
            } => {
                let items: Vec<&WaitItem> = g
                    .owed
                    .iter()
                    .flatten()
                    .filter(|(owed, _)| identities.contains(&owed.identity))
                    .map(|(_, item)| item)
                    .collect();
                let role = g.role.clone().unwrap_or(Role::Reviewer);
                let mut text = if items.is_empty() {
                    String::new()
                } else {
                    super::wake_render::render_wait_items_ref(&items, author, role)
                };
                if escalated {
                    let duties: Vec<String> = items
                        .iter()
                        .filter_map(|item| super::wake_render::duty_line(item))
                        .collect();
                    if let Some(more) = super::wake_repeat::escalation(repeat, &duties) {
                        text.push_str(&more);
                    }
                }
                for notice in &notices {
                    text.push_str("\n\n");
                    text.push_str(&super::wake_render::notice_text(notice));
                }
                let text = text.trim_start().to_string();
                // A session start's context earns receipts only where it
                // reaches the model: Claude's.
                let commits =
                    !matches!(report, Report::SessionStart { .. }) || tool == Tool::Claude;
                let (delivered, prepared) = if commits {
                    (
                        super::wake_render::carried(items.iter().copied()),
                        super::wake_receipt::prepare(
                            &self.repo,
                            author,
                            super::wake_render::receipt_candidates(items.iter().copied()),
                        ),
                    )
                } else {
                    Default::default()
                };
                if let Some(custody) = self.agents.get_mut(author) {
                    custody.carried = Some(Carried {
                        conn,
                        id: presentation.clone(),
                        tool,
                        session: report.session().clone(),
                        delivered,
                        prepared,
                        settling: permit,
                    });
                }
                Frame::text(WireVerdict::Deliver { presentation }, text)
            }
            other => frame_of(other),
        }
    }

    /// The terminal verdict a parked report gets at its hook's deadline.
    pub(crate) fn deadline_frame(tool: Tool) -> Frame {
        match clank_core::turn_policy::deadline_verdict(tool) {
            clank_core::turn_policy::DeadlineVerdict::ReArm => Frame::text(
                WireVerdict::Notify,
                super::wake_render::REARM_REASON.to_string(),
            ),
            clank_core::turn_policy::DeadlineVerdict::RenewWait => {
                Frame::bare(WireVerdict::RenewWait)
            }
            clank_core::turn_policy::DeadlineVerdict::Failed(message) => {
                Frame::bare(WireVerdict::Diagnostic { message })
            }
        }
    }

    /// The agent whose in-flight presentation connection `conn` carries.
    pub(crate) fn carrier(&self, conn: u64) -> Option<&AgentLabel> {
        self.agents
            .iter()
            .find(|(_, c)| c.carried.as_ref().is_some_and(|c| c.conn == conn))
            .map(|(author, _)| author)
    }

    /// A result from connection `conn`. Only the connection that carried a
    /// presentation can answer for it: a result from any other is refused
    /// and commits nothing.
    pub(crate) fn result(
        &mut self,
        author: &AgentLabel,
        conn: u64,
        result: TurnResult,
        g: &Gathered,
    ) -> Result<Option<Commit>, String> {
        let carries = self
            .agents
            .get(author)
            .and_then(|c| c.carried.as_ref())
            .is_some_and(|c| c.conn == conn && &c.id == result.presentation());
        if !carries {
            return Err(format!(
                "a result for {:?} came from a connection that did not carry it",
                result.presentation()
            ));
        }
        let event = match &result {
            TurnResult::Delivered { presentation } => Event::Delivered(presentation.clone()),
            TurnResult::EmitFailed { presentation, .. } => Event::EmitFailed(presentation.clone()),
        };
        self.settle(author, g, event)
    }

    /// Connection `conn` closed. A presentation it carried and never
    /// answered for failed, terminally.
    pub(crate) fn closed(&mut self, author: &AgentLabel, conn: u64, g: &Gathered) {
        let Some(id) = self
            .agents
            .get(author)
            .and_then(|c| c.carried.as_ref())
            .filter(|c| c.conn == conn)
            .map(|c| c.id.clone())
        else {
            return;
        };
        let _ = self.settle(author, g, Event::EmitFailed(id));
    }

    /// Connection `conn` is one of `author`'s deliverers. The newest is its
    /// park, and a new park means it is watched again.
    pub(crate) fn watch(&mut self, author: &AgentLabel, conn: u64, g: impl FnOnce() -> Gathered) {
        if self
            .agents
            .get(author)
            .is_some_and(|c| c.park.is_some_and(|p| p >= conn))
        {
            return;
        }
        let Some(custody) = self.custody(author, &g()) else {
            return;
        };
        if custody.park.is_some_and(|p| p >= conn) {
            return;
        }
        custody.park = Some(conn);
        custody.state.unwatched = None;
        self.persist(author, None);
    }

    /// Deliverer `conn` ended unanswered. If it was `author`'s park, the
    /// agent has had no waiter since now. Whether its turn is over is the
    /// Stop that park reported to say, never the park's ending.
    pub(crate) fn lost(&mut self, author: &AgentLabel, conn: u64, g: &Gathered) {
        let Some(custody) = self.custody(author, g) else {
            return;
        };
        if custody.park != Some(conn) {
            return;
        }
        custody.state.unwatched = Some(Unwatched { since: g.now });
        self.persist(author, None);
    }

    /// `author`'s hook was told to wake it at its deadline, outside any
    /// step: it is past the Stop that hook reported.
    pub(crate) fn woken(&mut self, author: &AgentLabel) {
        let Some(custody) = self
            .agents
            .get_mut(author)
            .filter(|c| c.state.stopped.is_some())
        else {
            return;
        };
        custody.state.stopped = None;
        self.persist(author, None);
    }

    fn settle(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        event: Event,
    ) -> Result<Option<Commit>, String> {
        // Stepped and written while the presentation still holds its
        // permit: taking it first would leave the result unwritten by a
        // monitor that has since seen a newer clank installed.
        let out = self.step(author, g, None, event);
        let carried = self.agents.get_mut(author).and_then(|c| c.carried.take());
        match out {
            Some(Output::Result(ResultOutcome::Resolved(effects))) => Ok(effects
                .iter()
                .any(|e| matches!(e, Effect::CommitReceipts(_)))
                .then_some(carried)
                .flatten()
                .map(|carried| Commit {
                    author: author.clone(),
                    carried,
                })),
            Some(Output::Result(ResultOutcome::Duplicate)) => Ok(None),
            Some(Output::Result(ResultOutcome::ProtocolViolation(id))) => {
                Err(format!("a result for {id:?}, which is not in flight"))
            }
            _ => Err("the result's session is no longer bound".into()),
        }
    }

    /// An attendance request, from `clank attend` or `clank run`, or why
    /// it was not carried out.
    pub(crate) fn attend(
        &mut self,
        author: &AgentLabel,
        request: AttendRequest,
        g: &Gathered,
    ) -> Result<String, String> {
        let said = match &request {
            AttendRequest::Run { desc, expect, .. } => {
                Some((format!("the `clank run` \"{desc}\""), true, *expect))
            }
            AttendRequest::Declare { name, expect, .. } => {
                Some((format!("\"{name}\""), false, *expect))
            }
            AttendRequest::Clear { .. } => None,
        };
        let listed = matches!(g.binding, Some((Tool::Claude | Tool::Grok, _)));
        let attending = |verb: &str| {
            let (what, run, expect) = said.clone().expect("an attend names what it attends");
            let ends = match (run, listed) {
                (true, _) => "until its process ends, which wakes you once",
                (false, true) => {
                    "until your harness stops listing the task with that exact description and \
                     any background work it left running"
                }
                (false, false) => {
                    "until you clear it with `clank attend --clear` once your harness reports it \
                     finished"
                }
            };
            format!(
                "{verb} {what} {ends}. Work you have already been shown stays quiet and new work \
                 still wakes you; if it is still going after {} you are checked in on once, \
                 through your wait when your turn has ended. Anything else you attend is \
                 unchanged.",
                super::attend::duration_arg(expect / 1000),
            )
        };
        let before = self.custody(author, g).map(|c| c.state.clone());
        let out = self.step_in_memory(author, g, None, Event::Attend(request));
        // A run acknowledged here execs at once, and only turn.json brings
        // its registration back after a restart: an unwritten one is not
        // accepted, and is undone so memory never claims more than disk.
        if let Err(e) = self.try_persist(author, None) {
            if let (Some(custody), Some(before)) = (self.agents.get_mut(author), before) {
                custody.state = before;
            }
            return Err(format!(
                "could not record this in `{author}`'s turn state ({e}), so nothing was recorded"
            ));
        }
        match out {
            Some(Output::Attend(AttendReply::Accepted)) => Ok(attending("attending")),
            Some(Output::Attend(AttendReply::Renewed)) => Ok(attending("renewed: attending")),
            Some(Output::Attend(AttendReply::Cleared { removed: 0 })) => {
                Ok("that was not attended; nothing changed".into())
            }
            Some(Output::Attend(AttendReply::Cleared { removed })) => Ok(format!(
                "no longer attending {removed} {}; any check-in {} owed is cancelled",
                if removed == 1 { "thing" } else { "things" },
                if removed == 1 { "it" } else { "they" },
            )),
            _ => Err("this session no longer holds the label, so nothing was recorded".into()),
        }
    }

    /// Write the one terminal record of every run of `author`'s whose
    /// registered process is gone, then forget it. The record is written
    /// first and is idempotent per run, so a crash or restart between the
    /// two writes it again rather than losing it, and never twice.
    pub(crate) fn end_gone_runs(&mut self, author: &AgentLabel, g: &Gathered) {
        let repo = self.repo.clone();
        let Some(custody) = self.custody(author, g) else {
            return;
        };
        let mut ended = false;
        for run in custody.state.ended_runs(&Process, g.now) {
            match crate::inbox::record_run_end(&repo, author, &run) {
                Ok(()) => {
                    custody.state.end_run(&run.proof);
                    ended = true;
                }
                Err(e) => eprintln!(
                    "monitor: could not record that `{author}`'s run \"{}\" ended: {e:#}",
                    run.desc
                ),
            }
        }
        if ended {
            self.persist(author, None);
        }
    }

    /// Every run registered with an agent in custody, by its proof: the
    /// processes whose exits [`Self::end_gone_runs`] records.
    pub(crate) fn runs(&self) -> Vec<(AgentLabel, clank_core::turn_policy::ShellProof)> {
        self.agents
            .iter()
            .flat_map(|(author, c)| {
                c.state
                    .attendances
                    .iter()
                    .filter_map(|e| e.proof().map(|p| (author.clone(), p.clone())))
            })
            .collect()
    }

    /// The soonest moment a check-in is due or a presentation in flight
    /// runs out of time, across every agent.
    pub(crate) fn next_deadline(&self) -> Option<Ms> {
        self.agents
            .values()
            .flat_map(|c| [c.state.next_deadline(), c.state.delivery_deadline()])
            .flatten()
            .min()
    }

    /// The agents whose presentation in flight has had its time by `now`.
    pub(crate) fn overdue_authors(&self, now: Ms) -> Vec<AgentLabel> {
        self.agents
            .iter()
            .filter(|(_, c)| c.state.delivery_deadline().is_some_and(|at| at <= now))
            .map(|(author, _)| author.clone())
            .collect()
    }

    /// Fail `author`'s presentation in flight if its carrier has had
    /// [`clank_core::turn_policy::DELIVERY_RESULT_WITHIN`] and not answered.
    /// Whether one was failed: what it carried stays owed and undelivered,
    /// and a result its carrier sends later is refused, as any result for
    /// nothing in flight is.
    pub(crate) fn expire(&mut self, author: &AgentLabel, g: &Gathered) -> bool {
        let expired = matches!(
            self.step(author, g, None, Event::ResultOverdue),
            Some(Output::Result(ResultOutcome::Resolved(_)))
        );
        if expired && let Some(custody) = self.agents.get_mut(author) {
            custody.carried = None;
        }
        expired
    }

    /// What `author`'s presentation in flight carries, and when it stops
    /// counting as a delivery (unix seconds).
    pub(crate) fn in_flight(&self, author: &AgentLabel) -> Option<(Vec<String>, u64)> {
        let p = self.agents.get(author)?.state.outstanding.as_ref()?;
        Some((p.identities.clone(), p.expires().div_ceil(1000)))
    }

    /// The agents with a check-in due by `now`.
    pub(crate) fn due_authors(&self, now: Ms) -> Vec<AgentLabel> {
        self.agents
            .iter()
            .filter(|(_, c)| c.state.next_deadline().is_some_and(|at| at <= now))
            .map(|(author, _)| author.clone())
            .collect()
    }

    /// Step `author` at a check-in's deadline: every entry it passed is
    /// owed its check-in from now, which the agent's next park presents.
    pub(crate) fn fall_due(&mut self, author: &AgentLabel, g: &Gathered) {
        self.step(author, g, None, Event::CheckInDue);
    }

    /// The facts changed: an owed-work change ends the repeat episode.
    pub(crate) fn observed(&mut self, author: &AgentLabel, g: &Gathered) {
        if g.owed.is_some() {
            self.step(author, g, None, Event::Observed);
        }
    }

    /// Whether `author` has a repeat count an owed-work change could end.
    pub(crate) fn counting(&self, author: &AgentLabel) -> bool {
        self.agents
            .get(author)
            .is_some_and(|c| c.state.repeat.is_some())
            || load_stored(&self.agent_dir(author)).is_some_and(|p| p.repeat.is_some())
    }

    /// Whether recovery must leave `author` alone: the model's own answer.
    pub(crate) fn recovery_quiet(&mut self, author: &AgentLabel, g: &Gathered) -> Option<Quiet> {
        let custody = self.custody(author, g)?;
        let owed = g.owed_now(Some(&custody.state));
        custody
            .state
            .recovery_quiet(&facts(g, None, &owed, &Process))
    }

    /// What recovery, status and the TUI take of `author`: its attendance,
    /// its question, once it escalates its repeat count, a lost park, and
    /// the Stop it last reported.
    pub(crate) fn projection(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
    ) -> super::recovery::Projection {
        let asking = g.asking().map(str::to_string);
        let Some(custody) = self.custody(author, g) else {
            return super::recovery::Projection {
                asking,
                ..Default::default()
            };
        };
        let owed = g.owed_now(Some(&custody.state));
        let attended = custody
            .state
            .standing(&facts(g, None, &owed, &Process))
            .into_iter()
            .map(|(entry, qualifies)| attended(entry, &qualifies))
            .collect();
        super::recovery::Projection {
            attended,
            asking,
            repeated: custody
                .state
                .repeat
                .as_ref()
                .map(|r| r.count)
                .filter(|n| *n >= clank_core::turn_policy::ESCALATE_AT),
            unwatched: custody.state.unwatched,
            stopped: custody.state.stopped.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self, author: &AgentLabel) -> Option<&SessionState> {
        self.agents.get(author).map(|c| &c.state)
    }
}

/// The frame a verdict that presents nothing is: its wire form and the
/// text it wakes with, if any.
pub(crate) fn frame_of(verdict: Verdict) -> Frame {
    match verdict {
        Verdict::Silent(why) => Frame::bare(WireVerdict::Silent {
            why: match why {
                SilentWhy::StaleBinding => SilentReason::StaleBinding,
                SilentWhy::AutoOff => SilentReason::AutoOff,
                SilentWhy::WaitArmed => SilentReason::WaitArmed,
                SilentWhy::UserCommand => SilentReason::UserCommand,
            },
        }),
        Verdict::Park => Frame::bare(WireVerdict::Park),
        Verdict::Notify(notices) => Frame::text(
            WireVerdict::Notify,
            notices
                .iter()
                .map(super::wake_render::notice_text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        Verdict::Failed(message) => Frame::bare(WireVerdict::Diagnostic { message }),
        Verdict::Deliver { .. } => Frame::bare(WireVerdict::Diagnostic {
            message: "hook: a presentation reached a path that cannot carry one".into(),
        }),
    }
}

fn rfc3339(ms: Ms) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// One entry as status and the TUI show it, with why it covers nothing
/// when it does not.
fn attended(entry: &Entry, qualifies: &Qualifies) -> super::agent_state::Attended {
    let shell = entry.proof();
    super::agent_state::Attended {
        task: entry.name().to_string(),
        expect: Some(super::agent_state::Expectation {
            since: rfc3339(entry.window.since),
            secs: entry.window.expect / 1000,
        }),
        desc: Some(entry.desc.clone()),
        pid: shell.map(|p| p.pid),
        token: shell.and_then(|p| serde_json::from_str(&p.token).ok()),
        at: rfc3339(entry.window.since),
        declared: matches!(entry.work, Attending::Declared { .. }),
        check_in: match &entry.window.check_in {
            CheckIn::Armed => None,
            CheckIn::Due => Some("check-in due".into()),
            CheckIn::Fired => Some("checked in".into()),
        },
        unqualified: match qualifies {
            Qualifies::Yes => None,
            Qualifies::No(why) => Some(unqualified_text(why).into()),
        },
        rests_on: entry.phase().and_then(rests_on),
    }
}

/// What a declared subagent that is not running is attended for, as a
/// status row says it. An unsure park says the work may be the agent's
/// own: that is all the evidence could establish.
fn rests_on(phase: &Phase) -> Option<String> {
    match phase {
        Phase::Live | Phase::Ended => None,
        Phase::Unsettled { .. } => Some("may be running again".into()),
        Phase::Parked { on, sure } => {
            let tasks: Vec<String> = on
                .iter()
                .map(|task| {
                    let kind = task.kind.as_deref().unwrap_or("task");
                    match &task.description {
                        Some(description) => format!("{kind} \"{description}\""),
                        None => format!("{kind} {}", task.id),
                    }
                })
                .collect();
            let unsure = match sure {
                true => "",
                false => " (may be yours)",
            };
            Some(format!("waiting on {}{unsure}", tasks.join(", ")))
        }
    }
}

/// Why an entry covers nothing, as a status row says it.
pub(crate) fn unqualified_text(why: &Unqualified) -> &'static str {
    match why {
        Unqualified::ProcessGone => "process gone",
        Unqualified::ProcessUnknown => "process cannot be seen",
        Unqualified::PastDeadline => "past its deadline",
    }
}

#[cfg(test)]
#[path = "turn_custody_tests.rs"]
mod tests;
