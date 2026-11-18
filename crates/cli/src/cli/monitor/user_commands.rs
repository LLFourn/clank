use std::time::Duration;

use super::clients::Clients;
use super::custody::Custody;
use super::hosted::{Reading, Sources};
use super::retire::OwnedTasks;

/// One evaluation of the repo's user `commands` against the loop's view of
/// its agents: spawn the attempts the pass hands out, have recovery read
/// the records of agents a request waits on, and release the parks that
/// alone stand in a verified request's way, serving them again.
#[allow(clippy::too_many_arguments)]
pub(super) fn user_command_round(
    repo: &std::path::Path,
    commands: &mut crate::cli::user_command::live::UserCommands,
    state: Option<&crate::repo_state::RepoState>,
    clients: &mut Clients,
    sources: &mut Sources,
    custody: &mut Custody,
    owned: &mut OwnedTasks,
) {
    let settling = commands.settling(crate::cli::recovery::unix_now());
    let mut spawned = Vec::new();
    let out = commands.pass(&mut custody.agents(repo, clients, &settling), &mut spawned);
    owned.depart(spawned);
    custody.want_records(repo, &out.watch, clients, owned);
    custody.release_for_user_commands(out.release.clone());
    for author in &out.release {
        clients.serve_reports(
            repo,
            state,
            Some(author),
            &mut Reading::cached(),
            sources,
            custody,
        );
    }
}

/// The repo's user commands as the loop runs them, and what wakes it for
/// them.
pub(super) struct UserCommandLoop {
    pub(super) done: tokio::sync::mpsc::UnboundedReceiver<crate::cli::user_command::live::Done>,
    pub(super) asks: tokio::sync::mpsc::UnboundedReceiver<crate::cli::user_command::live::Ask>,
    pub(super) changed: tokio::sync::mpsc::UnboundedReceiver<()>,
    commands: crate::cli::user_command::live::UserCommands,
}

impl UserCommandLoop {
    pub(super) fn new(repo: &std::path::Path) -> Self {
        let user_home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let (commands, wakes) = crate::cli::user_command::live::UserCommands::new(
            repo,
            user_home.clone(),
            crate::cli::user_command::store::Store::in_home(&user_home),
        );
        let crate::cli::user_command::live::Wakes {
            changed,
            asks,
            done,
        } = wakes;
        UserCommandLoop {
            done,
            asks,
            changed,
            commands,
        }
    }

    /// When a waiting user command expires, or a released park has
    /// settled.
    pub(super) fn next_due(&self) -> Option<tokio::time::Instant> {
        self.commands.next_due().map(|at| {
            let wait = at.saturating_sub(crate::cli::recovery::unix_now());
            tokio::time::Instant::now() + Duration::from_secs(wait)
        })
    }

    pub(super) fn finished(&mut self, done: crate::cli::user_command::live::Done) {
        self.commands.finished(done);
    }

    /// Answer an attempt's ask with the loop's last look at its agent: a
    /// monitor that no longer owns the repo looks at nothing.
    pub(super) fn ask(
        &mut self,
        ask: crate::cli::user_command::live::Ask,
        owner: bool,
        repo: &std::path::Path,
        clients: &Clients,
        custody: &mut Custody,
    ) {
        let answer = if owner {
            let settling = self.commands.settling(crate::cli::recovery::unix_now());
            match custody.look_for_user_command(repo, clients, &settling, &ask.label, &ask.session)
            {
                crate::cli::user_command::dispatch::Look::Go => Ok(()),
                crate::cli::user_command::dispatch::Look::Parked => {
                    Err("its turn is parked".to_string())
                }
                crate::cli::user_command::dispatch::Look::Wait(why) => Err(why),
            }
        } else {
            Err("this monitor no longer holds the repo's lease".to_string())
        };
        let _ = ask.reply.send(answer);
    }

    /// One round of the repo's user commands, after a state change when
    /// `changed`: the parks it releases served again.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn round(
        &mut self,
        changed: bool,
        repo: &std::path::Path,
        state: Option<&crate::repo_state::RepoState>,
        clients: &mut Clients,
        sources: &mut Sources,
        custody: &mut Custody,
        owned: &mut OwnedTasks,
    ) {
        if changed {
            self.commands.state_changed();
        }
        user_command_round(
            repo,
            &mut self.commands,
            state,
            clients,
            sources,
            custody,
            owned,
        );
    }
}

#[cfg(test)]
#[path = "user_command_tests.rs"]
mod user_command_tests;
