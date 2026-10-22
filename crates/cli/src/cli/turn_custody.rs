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
    AttendReply, AttendRequest, Attendance, CheckIn, CheckInRef, Effect, Event, Facts, Handoff,
    Liveness, Ms, Output, OwedItem, Persisted, PresentationId, Quiet, ResultOutcome, Selector,
    SessionState, ShellProof, SilentWhy, TaskKind, Verdict, Withheld,
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
    route: Result<(), String>,
}

impl Gathered {
    /// The agent as it is now, owing `owed` (`None` when not yet known),
    /// with `route` saying whether a check-in could reach it.
    pub(crate) fn of(
        repo: &Path,
        author: &AgentLabel,
        owed: Option<(&[clank_core::wait::WaitItem], Option<&Path>)>,
        route: Result<(), String>,
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
            .map(|cfg| super::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref()))
            .map_err(|e| format!("{e:#}"));
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
            route,
        }
    }

    /// What the hook does when no monitor can decide for it: the local
    /// fold's owed work wakes the agent with a notice, never the work.
    pub(crate) fn alone_verdict(&self, session: &SessionId) -> Verdict {
        let owed = self.owed_now(None);
        clank_core::turn_policy::alone_verdict(session, &facts(self, None, &owed, &Process))
    }

    /// Grok's native Stop, as the monitor would answer it.
    pub(crate) fn handoff_verdict(&self, handoff: &Handoff) -> Verdict {
        clank_core::turn_policy::handoff_verdict(
            handoff,
            &facts(self, Some(Tool::Grok), &[], &Process),
        )
    }

    /// The header of the question the agent is waiting on, if any.
    pub(crate) fn asking(&self) -> Option<&str> {
        self.asking.as_deref()
    }

    #[cfg(test)]
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
struct Process;

impl Liveness for Process {
    fn holds(&self, proof: &ShellProof) -> bool {
        serde_json::from_str::<crate::proc_identity::ProcToken>(&proof.token)
            .is_ok_and(|token| token.still_holds(proof.pid))
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
        check_in_route: g.route.clone(),
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
    /// The identity of every item it presents, which a delivery discharges
    /// from whatever the monitor holds for it in memory alone.
    presented: Vec<String>,
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
        &self.carried.presented
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
        super::presentation::record_delivered(repo, &author, &carried.delivered, current);
        super::wake_receipt::dispatch(repo, &author, carried.prepared, carried.tool, current).await;
    }
}

/// The one check-in an attendance owes, to be typed through the agent's
/// resume route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckInDue {
    pub(crate) author: AgentLabel,
    pub(crate) session: SessionId,
    pub(crate) check_in: CheckInRef,
    pub(crate) text: String,
}

struct Custody {
    state: SessionState,
    carried: Option<Carried>,
    stored: Option<Persisted>,
}

/// Every agent's turn-end state, as the monitor's loop drives it.
pub(crate) struct Turns {
    repo: PathBuf,
    agents: BTreeMap<AgentLabel, Custody>,
}

fn store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("turn.json")
}

impl Turns {
    pub(crate) fn new(repo: PathBuf) -> Self {
        Turns {
            repo,
            agents: BTreeMap::new(),
        }
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
            let stored: Option<Persisted> = std::fs::read(store_path(&self.agent_dir(author)))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            let state = match &stored {
                Some(p) if &p.binding == bound => SessionState::restored(p.clone()),
                _ => SessionState::new(bound.clone()),
            };
            self.agents.insert(
                author.clone(),
                Custody {
                    state,
                    carried: None,
                    stored,
                },
            );
        }
        let custody = self.agents.get_mut(author)?;
        if &custody.state.binding != bound {
            custody.state.rebind(bound.clone());
            custody.carried = None;
        }
        Some(custody)
    }

    /// `author` holds no binding: its attendance, the check-in that owes,
    /// its repeat count and any presentation in flight all described a
    /// session that is no longer its, so they end here and on disk.
    pub(crate) fn retire(&mut self, author: &AgentLabel) {
        self.agents.remove(author);
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

    fn persist(&mut self, author: &AgentLabel) {
        let dir = self.agent_dir(author);
        let Some(custody) = self.agents.get_mut(author) else {
            return;
        };
        let now = custody.state.persisted();
        if custody.stored.as_ref() == Some(&now) {
            return;
        }
        let path = store_path(&dir);
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec(&now)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &path)
            });
        match written {
            Ok(()) => custody.stored = Some(now),
            Err(e) => eprintln!("monitor: could not record `{author}`'s turn state: {e}"),
        }
    }

    fn step(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        tool: Option<Tool>,
        event: Event,
    ) -> Option<Output> {
        let custody = self.custody(author, g)?;
        let owed = g.owed_now(Some(&custody.state));
        let out = custody.state.step(event, &facts(g, tool, &owed, &Process));
        self.persist(author);
        Some(out)
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
        let event = match report {
            Report::Handoff { handoff, .. } => return Some(frame_of(g.handoff_verdict(handoff))),
            _ if g.owed.is_none() => return None,
            Report::TurnEnd { stop, .. } if first => Event::TurnEnd(stop.clone()),
            Report::TurnEnd { stop, .. } => Event::Parked(stop.clone()),
            Report::SessionStart { session, .. } => Event::SessionStart(session.clone()),
        };
        let tool = report.tool();
        let Some(Output::Verdict(verdict)) = self.step(author, g, Some(tool), event) else {
            return Some(Frame::bare(WireVerdict::Silent {
                why: SilentReason::StaleBinding,
            }));
        };
        let frame = match verdict {
            Verdict::Deliver {
                presentation,
                repeat,
                escalated,
                notice,
                ..
            } => {
                let items: Vec<&WaitItem> = g.owed.iter().flatten().map(|(_, item)| item).collect();
                let role = g.role.clone().unwrap_or(Role::Reviewer);
                let mut text = super::wake_render::render_wait_items_ref(&items, author, role);
                if let Report::TurnEnd { stop, .. } = report
                    && let Some(recipe) = super::wake_render::live_work_recipe(&stop.tasks)
                {
                    text.push_str(&recipe);
                }
                if escalated {
                    let duties: Vec<String> = items
                        .iter()
                        .filter_map(|item| super::wake_render::duty_line(item))
                        .collect();
                    if let Some(more) = super::wake_repeat::escalation(repeat, &duties) {
                        text.push_str(&more);
                    }
                }
                if let Some(notice) = notice {
                    text.push_str("\n\n");
                    text.push_str(&super::wake_render::notice_text(&notice));
                }
                // A session start's context earns receipts only where it
                // reaches the model: Claude's.
                let commits =
                    !matches!(report, Report::SessionStart { .. }) || tool == Tool::Claude;
                let (delivered, prepared, presented) = if commits {
                    (
                        super::wake_render::carried(items.iter().copied()),
                        super::wake_receipt::prepare(
                            &self.repo,
                            author,
                            super::wake_render::receipt_candidates(items.iter().copied()),
                        ),
                        items.iter().map(|item| item.identity()).collect(),
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
                        presented,
                    });
                }
                Frame::text(WireVerdict::Deliver { presentation }, text)
            }
            other => frame_of(other),
        };
        Some(frame)
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

    fn settle(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
        event: Event,
    ) -> Result<Option<Commit>, String> {
        let carried = self.agents.get_mut(author).and_then(|c| c.carried.take());
        match self.step(author, g, None, event) {
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

    /// An attendance request, from `clank attend` or `clank run`.
    pub(crate) fn attend(
        &mut self,
        author: &AgentLabel,
        request: AttendRequest,
        g: &Gathered,
    ) -> Result<String, String> {
        let said = match &request {
            AttendRequest::Attend {
                selector, expect, ..
            } => Some((selector.clone(), *expect)),
            AttendRequest::Clear { .. } => None,
        };
        match self.step(author, g, None, Event::Attend(request)) {
            Some(Output::Attend(AttendReply::Accepted)) => {
                let (selector, expect) = said.expect("an accepted attend names its selector");
                Ok(format!(
                    "attending {} from your next turn-end: while it is listed running, that \
                     turn-end stays quiet; if it is still going after {} you are checked in on \
                     once",
                    selector_words(&selector),
                    super::attend::duration_arg(expect / 1000),
                ))
            }
            Some(Output::Attend(AttendReply::Cleared { had: true })) => {
                Ok("no longer attending; any check-in it owed is cancelled".into())
            }
            Some(Output::Attend(AttendReply::Cleared { had: false })) => {
                Ok("was not attending anything".into())
            }
            _ => Err("this session no longer holds the label, so nothing was recorded".into()),
        }
    }

    /// The soonest moment a check-in is due, across every agent.
    pub(crate) fn next_deadline(&self) -> Option<Ms> {
        self.agents
            .values()
            .filter_map(|c| c.state.next_deadline())
            .min()
    }

    /// The agents with a check-in due by `now`.
    pub(crate) fn due_authors(&self, now: Ms) -> Vec<AgentLabel> {
        self.agents
            .iter()
            .filter(|(_, c)| c.state.next_deadline().is_some_and(|at| at <= now))
            .map(|(author, _)| author.clone())
            .collect()
    }

    /// Step `author`'s check-in at its deadline: the check-in to type, if
    /// one is owed and the gates let it through.
    pub(crate) fn check_in(&mut self, author: &AgentLabel, g: &Gathered) -> Option<CheckInDue> {
        let shell = match &self.custody(author, g)?.state.attendance {
            Attendance::Standing { kind, shell, .. } => (kind.clone(), shell.clone()),
            _ => return None,
        };
        let Some(Output::Effects(effects)) = self.step(author, g, None, Event::CheckInDue) else {
            return None;
        };
        let session = self.agents.get(author)?.state.binding.clone();
        effects.into_iter().find_map(|e| match e {
            Effect::CheckIn {
                check_in,
                desc,
                elapsed,
                expect,
            } => Some(CheckInDue {
                author: author.clone(),
                session: session.clone(),
                text: super::wake_render::check_in_text(
                    &check_in.task_id,
                    desc.as_deref(),
                    &shell.0,
                    shell.1.as_ref().map(|p| p.pid),
                    elapsed / 1000,
                    expect / 1000,
                ),
                check_in,
            }),
            _ => None,
        })
    }

    /// The check-in fired for `check_in` could not be typed, and why.
    pub(crate) fn check_in_failed(
        &mut self,
        author: &AgentLabel,
        check_in: CheckInRef,
        why: String,
        g: &Gathered,
    ) {
        self.step(
            author,
            g,
            None,
            Event::CheckInUndeliverable { check_in, why },
        );
    }

    /// The last look before the check-in fired for `check_in` is typed,
    /// against `author`'s custody and gates as they are now.
    pub(crate) fn may_type_check_in(
        &mut self,
        author: &AgentLabel,
        check_in: &CheckInRef,
        g: &Gathered,
    ) -> Result<(), String> {
        let custody = self
            .custody(author, g)
            .ok_or("the session no longer holds the label")?;
        custody
            .state
            .may_type_check_in(check_in, &facts(g, None, &[], &Process))
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
            || std::fs::read(store_path(&self.agent_dir(author)))
                .ok()
                .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
                .is_some_and(|p| p.repeat.is_some())
    }

    /// Whether recovery must leave `author` alone: the model's own answer,
    /// from its latest listing.
    pub(crate) fn recovery_quiet(&mut self, author: &AgentLabel, g: &Gathered) -> Option<Quiet> {
        let custody = self.custody(author, g)?;
        let owed = g.owed_now(Some(&custody.state));
        custody
            .state
            .recovery_quiet(&facts(g, None, &owed, &Process))
    }

    /// Whether `author` has an attendance the monitor holds: its harness
    /// version is then worth knowing, for the route a check-in needs.
    pub(crate) fn attending(&self, author: &AgentLabel) -> bool {
        self.agents
            .get(author)
            .is_some_and(|c| c.state.attendance != Attendance::None)
    }

    /// What status and the TUI show of `author`: its attendance and,
    /// once it escalates, its repeat count.
    pub(crate) fn projection(
        &mut self,
        author: &AgentLabel,
        g: &Gathered,
    ) -> (Option<super::agent_state::Attended>, Option<u32>) {
        let Some(custody) = self.custody(author, g) else {
            return (None, None);
        };
        let repeated = custody
            .state
            .repeat
            .as_ref()
            .map(|r| r.count)
            .filter(|n| *n >= clank_core::turn_policy::ESCALATE_AT);
        (attended(&custody.state.attendance), repeated)
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
                SilentWhy::Attending { .. } => SilentReason::AttendingBackgroundTask,
                SilentWhy::WaitArmed => SilentReason::WaitArmed,
            },
        }),
        Verdict::Park => Frame::bare(WireVerdict::Park),
        Verdict::Notify(notice) => Frame::text(
            WireVerdict::Notify,
            super::wake_render::notice_text(&notice),
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

fn attended(attendance: &Attendance) -> Option<super::agent_state::Attended> {
    let token = |shell: &Option<ShellProof>| {
        shell
            .as_ref()
            .and_then(|p| serde_json::from_str(&p.token).ok())
    };
    let expectation = |since: Ms, expect: Ms| super::agent_state::Expectation {
        since: rfc3339(since),
        secs: expect / 1000,
    };
    match attendance {
        Attendance::None => None,
        Attendance::Intent {
            selector,
            desc,
            since,
            expect,
            shell,
        } => Some(super::agent_state::Attended {
            task: selector_text(selector),
            expect: Some(expectation(*since, *expect)),
            desc: desc.clone(),
            pid: shell.as_ref().map(|p| p.pid),
            token: token(shell),
            at: rfc3339(*since),
            subagent: matches!(selector, Selector::Name(_)),
            pending: true,
            check_in: None,
        }),
        Attendance::Standing {
            task_id,
            kind,
            desc,
            since,
            expect,
            shell,
            check_in,
        } => Some(super::agent_state::Attended {
            task: task_id.clone(),
            expect: Some(expectation(*since, *expect)),
            desc: desc.clone(),
            pid: shell.as_ref().map(|p| p.pid),
            token: token(shell),
            at: rfc3339(*since),
            subagent: *kind == TaskKind::Subagent,
            pending: false,
            check_in: match check_in {
                CheckIn::Unarmed | CheckIn::Armed => None,
                CheckIn::Fired => Some("checked in".into()),
                CheckIn::Withheld(why) => Some(format!(
                    "check-in withheld: {}",
                    match why {
                        Withheld::AutoOff => "auto off",
                        Withheld::Asking => "a question was pending",
                        Withheld::Blocked => "a block was pending",
                    }
                )),
                CheckIn::Undeliverable(why) => Some(format!("check-in undeliverable: {why}")),
            },
        }),
    }
}

/// What a selector names, as a status row shows it.
fn selector_text(selector: &Selector) -> String {
    match selector {
        Selector::TaskId(id) => id.clone(),
        Selector::Name(name) => name.clone(),
        Selector::RunDesc(desc) => desc.clone(),
    }
}

/// What a selector names, as a sentence says it.
pub(crate) fn selector_words(selector: &Selector) -> String {
    match selector {
        Selector::TaskId(id) => format!("task `{id}`"),
        Selector::Name(name) => format!("\"{name}\""),
        Selector::RunDesc(desc) => format!("the `clank run` \"{desc}\""),
    }
}

#[cfg(test)]
#[path = "turn_custody_tests.rs"]
mod tests;
