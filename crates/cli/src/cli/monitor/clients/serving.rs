use clank_core::ids::AgentLabel;

use super::super::custody::Custody;
use super::super::hosted::{Reading, Sources};
use super::super::wire::Note;
use super::{Client, Served, Wrote, client_refusal, registered_waiter, send_note};

/// Whether an error means the author is GONE rather than unreadable.
///
/// `wait` treats `NotRegistered` as revocation and stops that waiter;
/// anything else is transient and fail-soft. A monitor that kept a
/// revoked client would hold a connection open forever without beats,
/// and — since that branch never writes — could not even notice the
/// client had disconnected.
fn revoked(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<crate::agent_store::RoleResolutionError>(),
        Some(crate::agent_store::RoleResolutionError::NotRegistered { .. })
    )
}

/// Tell each client what ITS author is owed, from one shared fold.
///
/// The fold and the derived status are repo-level — `WorkPolicy` comes
/// from the repo's config and its roster tiers, not from the author —
/// so both are computed once by the caller. Only the role, and
/// therefore `items_for`, is per author, and the role is re-read every
/// tick so a roster change reaches a connected client rather than
/// stranding it.
/// Test-only: production always has a pending buffer to pass, and
/// passing a fresh one would be the bug this plan spent two rounds on.
#[cfg(test)]
pub(in super::super) fn serve(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
) {
    // Dropped: this is for the tests that assert on what a client
    // RECEIVED, and they hold the runtime open around the call.
    drop(serve_to(
        repo,
        clients,
        state,
        None,
        &mut Sources::hosting(Default::default()),
    ));
}

/// [`serve`], optionally scoped to ONE author's clients.
///
/// Scoped on a source signal, because a client's queue is only
/// `CLIENT_QUEUE` deep: presenting to everybody whenever one author's
/// source logged something would put a frame in every other client's
/// queue, and a burst would retire readers that were keeping up
/// perfectly well.
#[must_use = "a departing client's last frame is still in its writer; drop these only \
              where the runtime is certain to outlive them"]
#[cfg(test)]
pub(in super::super) fn serve_to(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
    only: Option<&AgentLabel>,
    sources: &mut Sources,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    present_to(repo, clients, state, only, sources, &mut Reading::afresh())
}

/// Present `state` to `clients` — every one, or `only`'s — with what
/// `sources` hold for them, read as `reading` says. The writers of the
/// clients it retires.
pub(in super::super) fn present_to(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
    only: Option<&AgentLabel>,
    sources: &mut Sources,
    reading: &mut Reading,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    if clients.is_empty() {
        return Vec::new();
    }
    let mut leaving: Vec<tokio::task::JoinHandle<Wrote>> = Vec::new();
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);

    let mut keep = Vec::with_capacity(clients.len());
    // Whose client was WOKEN in this pass, and so received whatever was
    // held for them.
    let mut delivered: Vec<AgentLabel> = Vec::new();
    for mut client in clients.drain(..) {
        // A wait whose socket closed is the loop's to end, not a send's.
        if client.report.is_some()
            || matches!(client.who, Served::Agent { .. }) && client.out.is_closed()
        {
            keep.push(client);
            continue;
        }
        if let Some(why) = client_refusal(repo, &client) {
            send_note(&client, &Note::Refused { why });
            if let Some(writer) = client.writer {
                leaving.push(writer);
            }
            continue;
        }
        if let Some(only) = only
            && !matches!(&client.who, Served::Agent { author, .. } if author == only)
        {
            keep.push(client);
            continue;
        }
        let mut unavailable = false;
        let note = match &client.who {
            // No projection is derived for an observer, which is the
            // point: it asked to know the fold moved, and deriving
            // somebody's work to tell it that would be the duplicated
            // refold this plan exists to delete.
            Served::Observer | Served::Helper { .. } => Note::Beat,
            Served::Agent { author, .. } => {
                let inputs = match crate::cli::wait::derive_wait_inputs(repo, author) {
                    Ok(inputs) => inputs,
                    Err(e) if revoked(&e) => {
                        eprintln!("monitor: `{author}` left the roster; dropping it");
                        continue;
                    }
                    // Transient: keep the client, try again next tick.
                    Err(_) => {
                        keep.push(client);
                        continue;
                    }
                };
                let status = state
                    .fold
                    .derive_status(&reviews, &inputs.work_policy, head.as_ref());
                // The author's un-shown, unacked source rows go in as
                // EXTERNALS, through the same decision as the
                // projection. A source item delivered on its own would
                // reach the agent without the block context the local
                // path gives it, and without the durable eligibility
                // that says it is still owed at all (codex on ca2ea69).
                let sidecars = crate::cli::github::log::Sidecars::never_waiting();
                let mut external =
                    sources.external(repo, author, &mut client.shown, &sidecars, reading);

                let items = crate::cli::wait::owed_for(
                    repo,
                    author,
                    &inputs,
                    &status,
                    &state.fold,
                    &mut external,
                    &sidecars,
                );
                unavailable = sidecars.contended();
                if items.is_empty() {
                    Note::Beat
                } else {
                    Note::Items {
                        items,
                        source_cwd: std::env::current_dir().ok(),
                    }
                }
            }
        };
        // An agent connection is ONE delivery opportunity. A client
        // returns on its first `Items` and never reads another, so a
        // frame queued behind that one is written into a socket nobody
        // will read — and anything it carried is gone (codex on
        // 1552f66). Retiring here is what keeps a later event for the
        // next admission instead. The writer task drains what is
        // already queued as its sender drops, so this frame still
        // reaches the agent.
        //
        // The note alone decides, with no second check on who this is
        // or whether the list is empty: `Items` is built just above,
        // only for an agent and only when something is owed. Repeating
        // those conditions here would be two more branches saying what
        // the construction already says, and neither could be driven.
        client.unavailable = unavailable;
        let woke = matches!(&note, Note::Items { .. });
        let author = match &client.who {
            Served::Agent { author, .. } => Some(author.clone()),
            Served::Observer | Served::Helper { .. } => None,
        };
        let sent = send_note(&client, &note);
        if sent && !woke {
            keep.push(client);
            continue;
        }
        if sent
            && woke
            && let Some(author) = author
            && !delivered.contains(&author)
        {
            delivered.push(author);
        }
        // This client is going, and its LAST frame may still be in its
        // queue. The writer drains it as the sender drops — while the
        // runtime lives. Handing the handle back is what lets a
        // retirement wait for that instead of assuming it (codex on
        // 294c18d).
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    *clients = keep;
    sources.delivered(repo, &delivered);
    leaving
}

/// Each agent's waits held here are its deliverers, the newest its park;
/// one whose socket closed under it has [`ended`].
pub(in super::super) fn tend(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    sources: &mut Sources,
    custody: &mut Custody,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut closed = Vec::new();
    for c in clients.iter() {
        let Served::Agent { author } = &c.who else {
            continue;
        };
        // A wait admitted and closed within one round was still held and
        // answered nothing, so it is registered before its close is judged.
        if c.report.as_ref().is_none_or(|r| r.stepped) && client_refusal(repo, c).is_none() {
            custody.watch(author, c.conn, || {
                crate::cli::turn_custody::Gathered::of(repo, author, None)
            });
        }
        if c.out.is_closed() {
            closed.push(c.conn);
        }
    }
    closed
        .into_iter()
        .flat_map(|conn| ended(conn, repo, clients, state, sources, custody))
        .collect()
}

/// Connection `conn` ended. A deliverer the monitor still held, or one
/// whose presentation it carried failed with it, answered nothing: if it
/// was its agent's park, the park is lost. One answered is neither.
pub(in super::super) fn ended(
    conn: u64,
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    sources: &mut Sources,
    custody: &mut Custody,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let (gone, kept): (Vec<Client>, Vec<Client>) = std::mem::take(clients)
        .into_iter()
        .partition(|c| c.conn == conn);
    *clients = kept;
    let held = gone.iter().find_map(|c| match &c.who {
        Served::Agent { author } => Some(author.clone()),
        _ => None,
    });
    let mut leaving: Vec<_> = gone.into_iter().filter_map(|c| c.writer).collect();
    let Some(author) = held.or_else(|| custody.carrier(conn)) else {
        return leaving;
    };
    let g = sources.gather(repo, state, &author, &mut Reading::afresh());
    custody.ended(&author, conn, &g);
    leaving.extend(serve_reports(
        repo,
        clients,
        state,
        Some(&author),
        &mut Reading::afresh(),
        sources,
        custody,
    ));
    leaving
}

/// Serve `author`'s reports on the arrival of one. A child's report
/// changes what status shows of an attendance and no fold follows it, so
/// that is published here.
#[allow(clippy::too_many_arguments)]
pub(in super::super) fn serve_newcomer(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    author: &AgentLabel,
    child: bool,
    sources: &mut Sources,
    custody: &mut Custody,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let leaving = serve_reports(
        repo,
        clients,
        state,
        Some(author),
        &mut Reading::afresh(),
        sources,
        custody,
    );
    if child {
        let g = sources.gather(repo, state, author, &mut Reading::afresh());
        custody.project(author, &g);
    }
    leaving
}

/// Step each held report — every author's, or `only`'s — with `custody`,
/// against what `sources` say each author is owed, read as `reading`
/// says, and answer the ones whose verdict is terminal, retiring them. A
/// parked report is told so once and then kept alive with beats.
pub(in super::super) fn serve_reports(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    only: Option<&AgentLabel>,
    reading: &mut Reading,
    sources: &mut Sources,
    custody: &mut Custody,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    let waiters: Vec<(AgentLabel, String)> = clients
        .iter()
        .filter_map(registered_waiter)
        .map(|(author, session)| (author.clone(), session.to_string()))
        .collect();
    // A child's report is applied before the park it may release: what
    // it ends is then handed to that park in this same pass, with no Stop
    // of the agent's to wait for.
    clients.sort_by_key(|c| c.report.as_ref().is_none_or(|r| r.report.reaches_agent()));
    let mut keep = Vec::with_capacity(clients.len());
    for mut client in clients.drain(..) {
        let (Served::Agent { author }, Some(reporting)) = (&client.who, client.report.as_mut())
        else {
            keep.push(client);
            continue;
        };
        // A park whose socket closed is the loop's to end, not a send's.
        if only.is_some_and(|only| only != author) || reporting.stepped && client.out.is_closed() {
            keep.push(client);
            continue;
        }
        let author = author.clone();
        if !reporting.stepped {
            custody.observe_questions(&author, &reporting.report);
        }
        let session = reporting.report.session().as_str();
        let g = sources
            .gather(repo, state, &author, reading)
            .waiting(waiters.iter().any(|(a, s)| a == &author && s == session));
        client.unavailable = state.is_some() && !g.owed_known();
        let first = !reporting.stepped;
        let Some(frame) = custody.answer(&author, client.conn, &reporting.report, first, &g) else {
            keep.push(client);
            continue;
        };
        // Resolve legacy outage continuations before clearing the marker. A
        // verdict refused by the delivery queue has not served this session.
        let session = reporting.report.session().clone();
        let clear_outage = || {
            if !first {
                return;
            }
            match crate::agent_store::outage_served(repo, &author, &session) {
                Ok(crate::agent_store::OutageServed::Contended) => {
                    tracing::debug!("the agent's state lock is held; the outage notice stands")
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(%e, "could not clear the served outage"),
            }
        };
        reporting.stepped = true;
        let parks = reporting.report.parks();
        if !frame.terminal(parks) {
            let note = if first {
                Note::Verdict(frame)
            } else {
                Note::Beat
            };
            if send_note(&client, &note) {
                clear_outage();
                keep.push(client);
            } else if let Some(writer) = client.writer {
                leaving.push(writer);
            }
            continue;
        }
        let presents = frame.presentation().is_some();
        // Nothing reached the carrier, so nothing can answer for the
        // presentation: it fails now, and what it held stays owed.
        if send_note(&client, &Note::Verdict(frame)) {
            clear_outage();
        } else if presents {
            custody.closed(&author, client.conn, &g);
        }
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    *clients = keep;
    leaving
}

#[cfg(test)]
#[path = "serving_tests.rs"]
mod tests;
