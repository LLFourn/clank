//! `clank events watch`: repository-owned sources target the master role by
//! default; explicit authors and legacy wait_events remain pinned.

use std::path::Path;

use clank_core::agent_config::{WaitEventSource, WatchClause};
use clank_core::ids::AgentLabel;

use crate::cli::teams_config::RepoConfigFile;
use crate::cli::watch_store::{self, Target};

#[derive(Clone)]
enum Entry {
    Legacy {
        agent: AgentLabel,
        n: usize,
        source: clank_core::agent_config::WaitEventSource,
    },
    Repository {
        id: u64,
        watch: watch_store::Watch,
    },
}
impl Entry {
    fn source(&self) -> &clank_core::agent_config::WaitEventSource {
        match self {
            Self::Legacy { source, .. } => source,
            Self::Repository { watch, .. } => &watch.source,
        }
    }
}

fn target(t: &crate::cli::EventsWatchTarget) -> anyhow::Result<(std::path::PathBuf, Target)> {
    Ok((
        crate::cli::resolve_repo(t.repo.as_deref())?,
        named_target(t.author.as_deref())?,
    ))
}

fn named_target(author: Option<&str>) -> anyhow::Result<Target> {
    Ok(match author {
        Some(name) => Target::Agent {
            name: AgentLabel::parse(name).map_err(anyhow::Error::msg)?,
        },
        None => Target::Master,
    })
}

/// Whether `agent`'s declaration of `source` is still the agent's own, not
/// moved to a watch.
fn own(config: &RepoConfigFile, agent: &AgentLabel, source: &WaitEventSource) -> bool {
    matches!(
        watch_store::Watches(config).log_owner(agent, &watch_store::source_key(source)),
        watch_store::Producer::Agent(_)
    )
}

fn entries(repo: &Path, target: &Target) -> anyhow::Result<Vec<Entry>> {
    let config = watch_store::read(repo)?;
    let mut entries = vec![];
    if let Target::Agent { name } = target {
        entries.extend(
            list(repo, name)?
                .into_iter()
                .enumerate()
                .filter_map(|(i, source)| {
                    own(&config, name, &source).then(|| Entry::Legacy {
                        agent: name.clone(),
                        n: i + 1,
                        source,
                    })
                }),
        );
    }
    entries.extend(
        config
            .event_watches
            .into_iter()
            .filter(|(_, watch)| !watch.removed && &watch.target == target)
            .map(|(id, watch)| Entry::Repository { id, watch }),
    );
    Ok(entries)
}

fn selected(repo: &Path, target: &Target, n: usize) -> anyhow::Result<Entry> {
    let entries = entries(repo, target)?;
    n.checked_sub(1)
        .and_then(|i| entries.get(i).cloned())
        .ok_or_else(|| anyhow::anyhow!("{target} has no watch {n}; see `clank events watch list`"))
}

pub(super) fn run(cmd: crate::cli::EventsWatchCmd) -> anyhow::Result<()> {
    use crate::cli::EventsWatchCmd;
    match cmd {
        EventsWatchCmd::List(a) => {
            let (repo, target) = target(&a.target)?;
            let config = watch_store::read(&repo)?;
            let mut targets = vec![target];
            if a.all {
                targets = vec![Target::Master];
                let mut agents: std::collections::BTreeSet<_> =
                    config.agents.keys().cloned().collect();
                if let Ok(dirs) = std::fs::read_dir(crate::agent_store::agents_root(&repo)) {
                    for dir in dirs.flatten() {
                        if let Some(name) = dir
                            .file_name()
                            .to_str()
                            .and_then(|s| AgentLabel::parse(s).ok())
                        {
                            agents.insert(name);
                        }
                    }
                }
                agents.extend(
                    config
                        .event_watches
                        .values()
                        .filter_map(|w| match &w.target {
                            Target::Agent { name } => Some(name.clone()),
                            Target::Master => None,
                        }),
                );
                targets.extend(agents.into_iter().map(|name| Target::Agent { name }));
            }
            let mut rows = vec![];
            for target in targets {
                let recipient = target.recipient(&config);
                for (i, entry) in entries(&repo, &target)?.iter().enumerate() {
                    if a.json {
                        rows.push(serde_json::json!({"n":i+1, "target":target,
                            "recipient":recipient, "source":entry.source()}));
                    } else {
                        println!(
                            "{} -> {}: {}",
                            target,
                            recipient
                                .as_ref()
                                .map(AgentLabel::as_str)
                                .unwrap_or("unassigned"),
                            entry_line(i + 1, entry.source())
                        );
                    }
                }
            }
            if a.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            }
        }
        EventsWatchCmd::Add(a) => {
            let (repo, target) = target(&a.target)?;
            let source = crate::cli::wait::parse_wait_event_source(&a.source)?;
            watch_store::add(&repo, target.clone(), source.clone())?;
            println!(
                "{target}: {}",
                entry_line(entries(&repo, &target)?.len(), &source)
            );
        }
        EventsWatchCmd::Remove(a) => {
            let (repo, target) = target(&a.target)?;
            match selected(&repo, &target, a.n)? {
                Entry::Legacy { agent, n, source } => {
                    crate::agent_store::repo_config::transact(&repo, |config| {
                        anyhow::ensure!(
                            own(config, &agent, &source),
                            "watch moved; list its scope again"
                        );
                        crate::agent_store::try_update_agent_config(&repo, &agent, |cfg| {
                            anyhow::ensure!(
                                cfg.wait_events.get(n - 1) == Some(&source),
                                "watch changed; list its scope again"
                            );
                            cfg.wait_events.remove(n - 1);
                            Ok(())
                        })
                    })?;
                }
                Entry::Repository { id, .. } => watch_store::remove(&repo, id, &target)?,
            }
            println!("removed {target} watch {}", a.n);
        }
        EventsWatchCmd::Transfer(a) => {
            let (repo, target) = target(&a.target)?;
            let from = named_target(a.from_author.as_deref())?;
            match selected(&repo, &from, a.n)? {
                Entry::Legacy { agent, n, source } => {
                    watch_store::transfer_legacy(&repo, &agent, n, target.clone(), Some(&source))?;
                }
                Entry::Repository { id, .. } => watch_store::retarget(&repo, id, target.clone())?,
            }
            println!(
                "transferred {from} watch {} to {target}; durable state retained",
                a.n
            );
        }
    }
    Ok(())
}

pub(crate) fn list(repo: &Path, author: &AgentLabel) -> anyhow::Result<Vec<WaitEventSource>> {
    Ok(crate::agent_store::load_agent_config(repo, author)?
        .map(|c| c.wait_events)
        .unwrap_or_default())
}

pub(crate) fn entry_line(n: usize, source: &WaitEventSource) -> String {
    match source {
        WaitEventSource::Github(g) => {
            let prompt = if g.prompt.is_some() {
                "prompt"
            } else {
                "no prompt"
            };
            format!("{n}. github {}: {} ({prompt})", g.repo, github_summary(g))
        }
        WaitEventSource::Command(c) => format!(
            "{n}. command {}: {}",
            c.name,
            serde_json::to_string(&c.command).unwrap_or_default()
        ),
        WaitEventSource::Timer(t) => match &t.repeat_every {
            Some(every) => format!("{n}. timer {}: due {}, every {every}", t.name, t.due_at),
            None => format!("{n}. timer {}: due {}", t.name, t.due_at),
        },
    }
}

fn github_summary(g: &clank_core::agent_config::GithubSource) -> String {
    if g.watch.is_empty() {
        let mut out = words(&g.events);
        if !g.prs.is_empty() {
            out.push_str(&format!(" on {}", numbers(&g.prs)));
        }
        if !g.branches.is_empty() {
            out.push_str(&format!(" on branches {}", g.branches.join(",")));
        }
        return out;
    }
    g.watch
        .iter()
        .map(|clause| match clause {
            WatchClause::Pr(w) if w.numbers.is_empty() => format!("pr {}", words(&w.events)),
            WatchClause::Pr(w) => format!("pr {} {}", numbers(&w.numbers), words(&w.events)),
            WatchClause::Issue(w) => format!("issue {}", words(&w.events)),
            WatchClause::Branch(w) if w.names.is_empty() => {
                format!("branch {}", words(&w.events))
            }
            WatchClause::Branch(w) => {
                format!("branch {} {}", w.names.join(","), words(&w.events))
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Each value in its config spelling.
fn words<T: serde::Serialize>(values: &[T]) -> String {
    values
        .iter()
        .filter_map(|v| serde_json::to_value(v).ok())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join(",")
}

fn numbers(prs: &[u64]) -> String {
    prs.iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_the_role_and_keeps_literal_master_pinned() {
        use crate::cli::command::{Cli, Command};
        use crate::cli::{EventsArgs, EventsCmd, EventsWatchArgs};
        use clap::Parser;
        let repo = tempfile::tempdir().unwrap();
        let invoke = |args: &[&str]| {
            let mut argv = vec!["clank", "events", "watch"];
            argv.extend(args);
            argv.extend(["--repo", repo.path().to_str().unwrap()]);
            let Command::Events(EventsArgs {
                command: EventsCmd::Watch(EventsWatchArgs { command }),
            }) = Cli::try_parse_from(argv).unwrap().command
            else {
                panic!("watch command");
            };
            run(command)
        };
        // No roster, binding, or agent config is needed to create role work.
        invoke(&["add", TIMER]).unwrap();
        assert_eq!(entries(repo.path(), &Target::Master).unwrap().len(), 1);
        assert!(!crate::agent_store::agents_root(repo.path()).exists());
        crate::agent_store::repo_config::transact(repo.path(), |cfg| {
            cfg.agents = serde_json::from_value(serde_json::json!({
                "master":{"tool":"codex","role":"commit"},
                "lead":{"tool":"codex","role":"master"}
            }))
            .unwrap();
            Ok(())
        })
        .unwrap();
        invoke(&["add", "--author", "master", COMMAND]).unwrap();
        invoke(&["add", GITHUB]).unwrap();
        let config = watch_store::read(repo.path()).unwrap();
        let lead = AgentLabel::parse("lead").unwrap();
        let named_master = AgentLabel::parse("master").unwrap();
        assert_eq!(watch_store::assigned(&config, &lead).len(), 2);
        assert_eq!(watch_store::assigned(&config, &named_master).len(), 1);
        invoke(&["transfer", "--from-author", "master", "1"]).unwrap();
        assert_eq!(entries(repo.path(), &Target::Master).unwrap().len(), 3);
        invoke(&["remove", "1"]).unwrap();
        assert_eq!(entries(repo.path(), &Target::Master).unwrap().len(), 2);
        assert!(invoke(&["remove", "0"]).is_err());
        assert!(invoke(&["add", "--author", "missing", TIMER]).is_err());
    }

    const GITHUB: &str = r#"{"kind":"github","repo":"o/r","watch":[{"pr":{"numbers":[7],"events":["ci_passed","ci_failed"]}}],"prompt":"Merge it when green."}"#;
    const COMMAND: &str = r#"{"kind":"command","name":"ci","command":["sh","-c","tail -f log"],"receipt_command":["true"]}"#;
    const TIMER: &str = r#"{"kind":"timer","name":"check-deploy","due_at":"2030-10-04T09:00:00+11:00","repeat_every":"24h"}"#;

    fn lines(sources: &[&str]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("master").unwrap();
        crate::agent_store::update_agent_config(dir.path(), &author, |cfg| {
            cfg.wait_events = sources
                .iter()
                .map(|raw| crate::cli::wait::parse_wait_event_source(raw).unwrap())
                .collect();
        })
        .unwrap();
        list(dir.path(), &author)
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, s)| entry_line(i + 1, s))
            .collect()
    }

    #[test]
    fn list_shows_each_kind() {
        assert_eq!(
            lines(&[GITHUB, COMMAND, TIMER]),
            [
                "1. github o/r: pr #7 ci_passed,ci_failed (prompt)",
                r#"2. command ci: ["sh","-c","tail -f log"]"#,
                "3. timer check-deploy: due 2030-10-04T09:00:00+11:00, every 24h",
            ]
        );
    }

    #[test]
    fn a_flat_watch_without_a_prompt_says_so() {
        assert_eq!(
            lines(&[
                r#"{"kind":"github","repo":"o/r","events":["pr_comment","branch_push"],"prs":[7,9],"branches":["main"]}"#
            ]),
            ["1. github o/r: pr_comment,branch_push on #7,#9 on branches main (no prompt)"]
        );
    }
}
