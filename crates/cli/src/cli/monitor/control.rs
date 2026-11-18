use clank_core::ids::AgentLabel;

use super::clients::Clients;
use super::custody::Custody;
use super::hosted::Sources;
use super::retire::OwnedTasks;
use super::wire::{Control, Note};

/// Question evidence is persisted only by the monitor, without requiring a
/// remote viewer. Recheck the session at consumption, not just in the adapter.
pub(crate) fn question_control(
    repo: &std::path::Path,
    request: &Control,
) -> Result<String, String> {
    let Control::Questions {
        author,
        session,
        observation,
    } = request
    else {
        return Err("not a question observation".into());
    };
    let author = AgentLabel::parse(author).map_err(|e| e.to_string())?;
    if !crate::agent_store::binding_matches(
        repo,
        &author,
        clank_core::Tool::Claude,
        session.as_str(),
    ) {
        return Err("this session no longer holds the label".into());
    }
    use crate::cli::question_state::Observation;
    let observed_session = match observation {
        Observation::Asked { key, .. } | Observation::Result { key } => &key.session,
        _ => return Err("not a native question observation".into()),
    };
    if observed_session != session.as_str() {
        return Err("question session does not match sender".into());
    }
    let recorded = crate::cli::question_state::try_observe(
        &crate::agent_store::agents_root(repo).join(author.as_str()),
        [observation.clone()],
    )
    .map_err(|e| format!("question evidence could not be recorded: {e:#}"))?;
    match recorded {
        true => Ok("recorded".into()),
        false => Err(
            "another clank process holds the agent's state; the evidence was not recorded".into(),
        ),
    }
}

pub(super) fn refused(why: String) -> Note {
    Note::Refused { why }
}

/// Carry out a control client's `request` with the concern it
/// addresses: what to tell the client.
#[allow(clippy::too_many_arguments)]
pub(super) async fn answer(
    request: &Control,
    owner: bool,
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    clients: &Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    owned: &mut OwnedTasks,
) -> Note {
    let answered = match request {
        _ if !owner => Err(refused(
            "this monitor no longer holds the repo's lease".to_string(),
        )),
        Control::Restart { .. } => match sources.restart(repo, request).await {
            Ok((said, stopped)) => {
                owned.depart_task(stopped);
                if sources.publish(repo) {
                    clients.beat_observers();
                }
                Ok(said)
            }
            Err(why) => Err(refused(why)),
        },
        Control::Questions { .. } => question_control(repo, request).map_err(refused),
        Control::Observe { .. } | Control::RecoveryReply { .. } => custody
            .recovery_request(repo, request, clients, owned)
            .map_err(refused),
        Control::Run { .. } | Control::Attend { .. } | Control::Clear { .. } => {
            custody.turn_request(repo, request, state, sources)
        }
    };
    match answered {
        Ok(what) => Note::Done { what },
        Err(note) => note,
    }
}
