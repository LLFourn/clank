use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use super::super::clients::Clients;
use super::super::fold::folded;
use super::super::hosted::{Sources, identities};
use super::super::retire::OwnedTasks;
use super::super::wire::Note;
use super::recovery_gate;

/// A transport's answer, and the attempt it answers.
pub(in crate::cli::monitor) type RecoveryAnswer = (
    AgentLabel,
    crate::cli::recovery::AttemptRef,
    crate::cli::recovery::Answer,
);

/// A transport, its preparation done off the loop, asking the loop that
/// owns the live state whether it may make its one mutation now.
#[derive(Debug)]
pub(in crate::cli::monitor) struct Authorize {
    pub(super) author: AgentLabel,
    pub(super) attempt: crate::cli::recovery::AttemptRef,
    /// The tool the roster gave the pane and its screen as just read, for a
    /// send typed into a pane.
    pub(super) screen: Option<(clank_core::Tool, Option<String>)>,
    /// The transport that will make the mutation.
    pub(super) transport: crate::cli::recovery::Transport,
    /// The repo folded afresh for this send, so the last look can derive
    /// the agent's work from disk as it is then.
    pub(super) state: Option<crate::repo_state::RepoState>,
    pub(super) reply: tokio::sync::oneshot::Sender<bool>,
}

/// Fold the repo afresh, then ask the loop whether `attempt` may be sent
/// now. Refused when the loop is gone or does not answer.
pub(super) async fn authorized(
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
    repo: &std::path::Path,
    author: AgentLabel,
    attempt: crate::cli::recovery::AttemptRef,
    screen: Option<(clank_core::Tool, Option<String>)>,
    transport: crate::cli::recovery::Transport,
) -> bool {
    let state = folded(repo).await.ok();
    let (reply, answer) = tokio::sync::oneshot::channel();
    let asked = asks.send(Authorize {
        author,
        attempt,
        screen,
        transport,
        state,
        reply,
    });
    asked.is_ok() && answer.await == Ok(true)
}

/// Answer a transport's request with the loop's last look.
pub(super) fn authorize(
    repo: &std::path::Path,
    clients: &Clients,
    sources: &Sources,
    recovery: &mut crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    a: Authorize,
) {
    let allowed = may_send_now(repo, clients, sources, recovery, turns, &a);
    let _ = a.reply.send(allowed);
}

/// The last look before any transport's mutation, made on the loop that
/// owns the live state, so a change already on disk vetoes the send
/// whether or not a fold has seen it: the same gate a deadline asks
/// (binding, deliverer, auto mode, a question, block or attendance); for a
/// typed send, the input read as empty and the pane's tool still the
/// roster's; and then the driver's own view,
/// given the role on the roster and the work derived from the repo as
/// freshly folded for this send, with the record read afresh nearest the
/// mutation. Not atomic: what changes after it is the race the capability
/// evidence must accept.
fn may_send_now(
    repo: &std::path::Path,
    clients: &Clients,
    sources: &Sources,
    recovery: &mut crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    a: &Authorize,
) -> bool {
    let Some((crate::cli::recovery::Gate::Go, owed)) = last_look(repo, clients, sources, turns, a)
    else {
        return false;
    };
    if let Some((tool, screen)) = &a.screen
        && screen
            .as_deref()
            .map(|s| crate::cli::composer::composer(*tool, s))
            != Some(crate::cli::composer::Composer::Empty)
    {
        return false;
    }
    let Ok(role) = crate::agent_store::resolve_role(repo, &a.author) else {
        return false;
    };
    let typing_into = a.screen.as_ref().map(|(tool, _)| *tool).filter(|tool| {
        crate::agent_store::resolve_tool(repo, &a.author).is_ok_and(|now| now == *tool)
    });
    let live = crate::cli::recovery::Live {
        role: role.to_string(),
        work: identities(&owed),
        bound: crate::agent_store::binding(repo, &a.author)
            .ok()
            .flatten()
            .as_ref()
            .map(crate::cli::recovery::Bound::of),
        transport: a.transport,
        typing_into,
    };
    recovery.may_send(&a.author, &a.attempt, &live)
}

/// The deadline's gate asked again at the last look, against the work
/// derived from the repo as folded for this send, and that work. `None`
/// when the send carries no fold; a hold when what is owed is unknown.
pub(super) fn last_look(
    repo: &std::path::Path,
    clients: &Clients,
    sources: &Sources,
    turns: &mut crate::cli::turn_custody::Turns,
    a: &Authorize,
) -> Option<(crate::cli::recovery::Gate, Vec<WaitItem>)> {
    let state = a.state.as_ref()?;
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    let Some(owed) = sources.owed_inspected(repo, state, &a.author, &reviews, head.as_ref()) else {
        return Some((crate::cli::recovery::Gate::Contended, Vec::new()));
    };
    let gate = recovery_gate(repo, clients, turns, &a.author, &a.attempt.session, &owed);
    Some((gate, owed))
}

/// An agent's pane, and its screen as read when it was found.
pub(crate) type Located = (crate::cli::pane_target::Target, Option<String>);

/// Where recovery's typed sends are typed: the agent's pane.
pub(crate) trait Typist: Send + Sync + 'static {
    /// The agent's pane, and its screen as read now.
    fn locate(
        &self,
        repo: std::path::PathBuf,
        author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>>;

    /// Type `text` into the pane and submit it.
    fn submit(
        &self,
        target: crate::cli::pane_target::Target,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Submitted> + Send>>;
}

/// What typing a prompt into an agent's pane came to.
pub(crate) type Submitted =
    Result<crate::cli::pane_input::Delivered, crate::cli::open_zellij::PaneWrite>;

/// The agent's zellij pane.
struct Panes;

impl Typist for Panes {
    fn locate(
        &self,
        repo: std::path::PathBuf,
        author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>> {
        Box::pin(async move {
            let target = crate::cli::pane_target::discover(repo, author)
                .await
                .map_err(|e| format!("its pane was not found: {e:#}"))?;
            let screen =
                crate::cli::open_zellij::dump_pane_screen(target.session.endpoint(), &target.pane)
                    .await;
            Ok((target, screen))
        })
    }

    fn submit(
        &self,
        target: crate::cli::pane_target::Target,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Submitted> + Send>> {
        Box::pin(async move {
            let text = crate::cli::pane_input::Admitted::new(&text);
            crate::cli::pane_input::submit_async(target.input(), &text).await
        })
    }
}

/// One typed recovery send through `typist`: find the agent's pane, ask the
/// loop for its last look, then submit.
pub(super) async fn typed_send(
    repo: std::path::PathBuf,
    author: AgentLabel,
    attempt: crate::cli::recovery::AttemptRef,
    text: String,
    typist: &dyn Typist,
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
) -> crate::cli::recovery::Answer {
    use crate::cli::recovery::{Answer, Transport};
    let Ok(_permit) = crate::upgrade::writing() else {
        return Answer::Unavailable;
    };
    let (target, screen) = match typist.locate(repo.clone(), author.clone()).await {
        Ok(located) => located,
        Err(why) => return Answer::NoPane(why),
    };
    let screen = Some((target.tool, screen));
    if !authorized(asks, &repo, author, attempt, screen, Transport::Typed).await {
        return Answer::Unavailable;
    }
    match typist.submit(target, text).await {
        Ok(_) => Answer::Submitted { started: None },
        Err(crate::cli::open_zellij::PaneWrite::NotStarted(_)) => Answer::Unavailable,
        Err(crate::cli::open_zellij::PaneWrite::Unknown(_)) => Answer::Indeterminate,
    }
}

/// Carry out what recovery decided.
pub(super) fn act_on_recovery(
    repo: &std::path::Path,
    clients: &Clients,
    acts: Vec<crate::cli::recovery::Act>,
    answers: &tokio::sync::mpsc::UnboundedSender<RecoveryAnswer>,
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
    records: &crate::cli::recovery::RecordWatch,
    owned: &mut OwnedTasks,
) {
    use crate::cli::recovery::{Act, Answer, Transport};
    // One look for the batch: a monitor on a replaced build sends none of
    // it, by any transport, and says so to recovery as a transport that
    // was not there.
    let permit = crate::upgrade::writing();
    for act in acts {
        match act {
            Act::Watch(path) => records.watch(path),
            Act::Unwatch(path) => records.unwatch(path),
            Act::Dispatch {
                author, attempt, ..
            } if permit.is_err() => {
                let _ = answers.send((author, attempt, Answer::Unavailable));
            }
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::Helper,
                tag,
                text,
                ..
            } => {
                let sent = clients.to_helper(
                    &author,
                    &attempt.session,
                    &Note::Recover {
                        opened_by: attempt.opened_by.clone(),
                        n: attempt.n,
                        wake: crate::cli::pane_input::Admitted::new(&text)
                            .as_str()
                            .to_string(),
                        tag,
                    },
                );
                if !sent {
                    let _ = answers.send((author, attempt, Answer::Unavailable));
                }
            }
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::Typed,
                text,
                ..
            } => {
                let repo = repo.to_path_buf();
                let answers = answers.clone();
                let asks = asks.clone();
                owned.depart_task(tokio::spawn(async move {
                    let answer =
                        typed_send(repo, author.clone(), attempt.clone(), text, &Panes, &asks)
                            .await;
                    let _ = answers.send((author, attempt, answer));
                }));
            }
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::CodexStart,
                target,
                text,
                ..
            } => {
                let repo = repo.to_path_buf();
                let answers = answers.clone();
                let asks = asks.clone();
                owned.depart_task(tokio::spawn(async move {
                    let go = authorized(
                        &asks,
                        &repo,
                        author.clone(),
                        attempt.clone(),
                        None,
                        Transport::CodexStart,
                    );
                    let text = crate::cli::pane_input::Admitted::new(&text);
                    let answer = crate::cli::codex_interrupt::start_if_failed(
                        &repo, &author, &target, &text, go,
                    )
                    .await;
                    let _ = answers.send((author, attempt, answer));
                }));
            }
        }
    }
}
