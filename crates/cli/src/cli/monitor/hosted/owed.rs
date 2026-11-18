use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

use crate::cli::watch_store::{Producer, Stream};

use super::{Hosted, hostable};

/// Each item's stable identity, as a delivery names it: never its
/// rendering, age or repeat count.
pub(in crate::cli::monitor) fn identities(owed: &[WaitItem]) -> Vec<String> {
    crate::cli::wake_render::mirror(owed, None)
        .iter()
        .map(crate::cli::wake_render::WaitItem::identity)
        .collect()
}

/// Everything `author` is owed: the repo's work, its sources' unshown rows
/// and what `pending` holds for it from a source no log keeps, all through
/// the one block-aware selection a waiting client is served by. `None`
/// when one of `author`'s logs is held by another process: what it is owed
/// is then unknown, never the partial list the free logs give.
#[allow(clippy::too_many_arguments)]
pub(super) fn owed_items(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(Stream, WaitItem)],
    reviews: &crate::fs_plan_state_lookup::FsPlanStateLookup,
    head: Option<&clank_core::wait::HeadCommit>,
) -> Option<Vec<WaitItem>> {
    let sidecars = crate::cli::github::log::Sidecars::never_waiting();
    let (admitted, _) = admitted_items(
        repo,
        state,
        author,
        hosted,
        timer_backlogs,
        pending,
        reviews,
        head,
        &sidecars,
    );
    (!sidecars.contended()).then(|| crate::cli::wait::presented(admitted))
}

/// [`owed_items`] before it is flattened into a wake: each item with what
/// the pending blocks make of it, and the work status that routed the
/// review items among them. No log is waited for: one whose sidecar
/// `sidecars` finds held is unavailable, and none of its items are here.
#[allow(clippy::too_many_arguments)]
pub(super) fn admitted_items(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(Stream, WaitItem)],
    reviews: &crate::fs_plan_state_lookup::FsPlanStateLookup,
    head: Option<&clank_core::wait::HeadCommit>,
    sidecars: &crate::cli::github::log::Sidecars,
) -> (
    Vec<crate::cli::wait::Admitted>,
    Option<clank_core::wait::WorkStatus>,
) {
    let Ok(inputs) = crate::cli::wait::derive_wait_inputs(repo, author) else {
        return (Vec::new(), None);
    };
    let status = state.fold.derive_status(reviews, &inputs.work_policy, head);
    let current;
    let sources = match hosted.get(&Producer::Agent(author.clone())) {
        Some(host) => &host.spec,
        None => {
            current = hostable(repo, author).unwrap_or_default();
            &current
        }
    };
    let mut shown = Default::default();
    let mut external =
        crate::cli::event_source::owed_from_sources(repo, author, sources, &mut shown, sidecars);
    external.extend(
        timer_backlogs
            .entry(author.clone())
            .or_insert_with(|| {
                crate::cli::timer_source::Backlog::new(
                    crate::agent_store::agents_root(repo)
                        .join(author.as_str())
                        .join("events"),
                    Default::default(),
                )
            })
            .owed(&mut shown, sidecars),
    );
    external.extend(
        pending
            .iter()
            .filter(|(whose, _)| whose.recipient(repo).as_ref() == Some(author))
            .map(|(_, item)| item.clone()),
    );
    let admitted = crate::cli::wait::admitted_for(
        repo,
        author,
        &inputs,
        &status,
        &state.fold,
        &mut external,
        sidecars,
    );
    (admitted, Some(status))
}

/// Each admitted item as the ledger is told of it: its stable identity,
/// and when its own durable record says it arrived. An event is dated by
/// its log row, a timer by when it fell due, and review work by the stage
/// boundary that handed it to `author`'s tier. Anything else is undated.
/// A log whose sidecar `sidecars` finds held dates nothing and is noted
/// there, which leaves the whole pass unknown to the ledger.
pub(in crate::cli::monitor) fn admissions(
    repo: &std::path::Path,
    author: &AgentLabel,
    status: Option<&clank_core::wait::WorkStatus>,
    admitted: Vec<crate::cli::wait::Admitted>,
    ended: &crate::cli::ledger::Ended,
    sidecars: &crate::cli::github::log::Sidecars,
) -> Vec<crate::cli::ledger::Admission> {
    use crate::cli::teams_config::RosterRole;
    use clank_core::wait::HandoverTier;
    let events_dir = crate::agent_store::agents_root(repo)
        .join(author.as_str())
        .join("events");
    let mut logs: std::collections::BTreeMap<String, Vec<crate::cli::github::log::EventRow>> =
        Default::default();
    let mut logged = |key: &clank_core::wait::EventPresentation| {
        logs.entry(key.source.clone())
            .or_insert_with(|| {
                let Ok((at, physical)) = crate::cli::watch_store::locate(&events_dir, &key.source)
                else {
                    return Vec::new();
                };
                if !at.join(format!("{physical}.jsonl")).is_file() {
                    return Vec::new();
                }
                match crate::cli::github::log::EventLog::open(&at, &physical)
                    .and_then(|log| sidecars.read_rows(&log, &key.source))
                {
                    Ok(crate::cli::github::log::Tried::Ran(Some(read))) => read.rows,
                    _ => Vec::new(),
                }
            })
            .iter()
            .find(|row| row.seq == key.seq && row.ident == key.ident)
            .map(|row| row.at)
    };
    let tier = crate::agent_store::repo_config::read(repo)
        .ok()
        .flatten()
        .and_then(|cfg| cfg.agents.get(author).map(|a| a.role))
        .map(|role| match role {
            RosterRole::Master => HandoverTier::Master,
            role if role.in_commit_tier() => HandoverTier::Commit,
            _ => HandoverTier::Second,
        });
    let handed = |item: &WaitItem| {
        let (status, tier) = (status?, tier?);
        let at = match item {
            WaitItem::Master { plan, .. } | WaitItem::Reviewer { plan, .. } => status
                .active_plan
                .as_ref()
                .filter(|p| &p.plan == plan)?
                .handover
                .since(tier),
            WaitItem::AdHocReview { sha, .. } | WaitItem::AdHocRevise { sha } => status
                .ad_hoc
                .iter()
                .find(|a| &a.sha == sha)?
                .handover
                .since(tier),
            _ => None,
        };
        at.and_then(|at| u64::try_from(at).ok())
    };
    admitted
        .into_iter()
        .filter_map(|a| {
            let identity = identities(std::slice::from_ref(&a.item))
                .into_iter()
                .next()?;
            let arrived = a
                .item
                .presentation()
                .and_then(&mut logged)
                .or(match &a.item {
                    WaitItem::TimerEvent { due_at, .. } => Some(*due_at),
                    WaitItem::CommandEvent { at, .. } => *at,
                    WaitItem::Unblocked { carries, .. } => {
                        ended.get(&crate::cli::block::key(&carries.name)).copied()
                    }
                    _ => None,
                })
                .or_else(|| handed(&a.item));
            Some(crate::cli::ledger::Admission {
                identity,
                item: a.item,
                standing: a.standing,
                pausable: a.pausable,
                arrived,
            })
        })
        .collect()
}

/// What `author` is owed now, from `state`, with what is held for it from
/// a source no log keeps: `None` while one of its logs is held elsewhere.
#[allow(clippy::too_many_arguments)]
pub(super) fn owed_now(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(Stream, WaitItem)],
) -> Option<Vec<WaitItem>> {
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    owed_items(
        repo,
        state,
        author,
        hosted,
        timer_backlogs,
        pending,
        &reviews,
        head.as_ref(),
    )
}

/// `author`'s facts now: owing what `state` says, when there is a state and
/// none of `author`'s logs is held elsewhere; otherwise owing what is not
/// known.
#[allow(clippy::too_many_arguments)]
pub(super) fn gather(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(Stream, WaitItem)],
) -> crate::cli::turn_custody::Gathered {
    let owed =
        state.and_then(|state| owed_now(repo, state, author, hosted, timer_backlogs, pending));
    let cwd = std::env::current_dir().ok();
    crate::cli::turn_custody::Gathered::of(
        repo,
        author,
        owed.as_deref().map(|owed| (owed, cwd.as_deref())),
    )
}
