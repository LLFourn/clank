//! `clank events watch`: repository-owned sources target the master role by
//! default; explicit authors and legacy wait_events remain pinned.

use std::path::Path;

use clank_core::agent_config::{WaitEventSource, WatchClause};
use clank_core::ids::AgentLabel;

#[cfg(test)]
use crate::cli::event_source::SourceOrigin;

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

fn entries(repo: &Path, target: &Target) -> anyhow::Result<Vec<Entry>> {
    let config = watch_store::read(repo)?;
    let mut entries = vec![];
    if let Target::Agent { name } = target {
        entries.extend(
            list(repo, name)?
                .into_iter()
                .enumerate()
                .filter_map(|(i, source)| {
                    (!watch_store::transferred(&config, name, &source)).then(|| Entry::Legacy {
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
                            !watch_store::transferred(config, &agent, &source),
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

/// Append the source `raw` describes, returning its number and the
/// source. Refused, leaving the config untouched, for a source an arm
/// would refuse alongside the ones already configured, or one already
/// configured.
#[cfg(test)]
pub(crate) fn add(
    repo: &Path,
    author: &AgentLabel,
    raw: &str,
) -> anyhow::Result<(usize, WaitEventSource)> {
    let source = crate::cli::wait::parse_wait_event_source(raw)?;
    crate::agent_store::try_update_agent_config(repo, author, |cfg| {
        if let Some(i) = cfg.wait_events.iter().position(|s| *s == source) {
            anyhow::bail!("{author} already has this source as entry {}", i + 1);
        }
        let mut candidate = cfg.wait_events.clone();
        candidate.push(source.clone());
        validate(&candidate)?;
        cfg.wait_events = candidate;
        Ok((cfg.wait_events.len(), source))
    })
}

/// Remove entry `n` (from 1, as [`list`] numbers them) and return it.
/// The entries left are not validated: removal is how a config holding
/// a source an arm refuses gets repaired.
#[cfg(test)]
pub(crate) fn remove(
    repo: &Path,
    author: &AgentLabel,
    n: usize,
) -> anyhow::Result<WaitEventSource> {
    crate::agent_store::try_update_agent_config(repo, author, |cfg| {
        let count = cfg.wait_events.len();
        anyhow::ensure!(
            (1..=count).contains(&n),
            "{author} has no wake source {n}: `clank events watch list` numbers {count}"
        );
        Ok(cfg.wait_events.remove(n - 1))
    })
}

#[cfg(test)]
fn validate(sources: &[WaitEventSource]) -> anyhow::Result<()> {
    let configured: Vec<_> = sources
        .iter()
        .map(|s| (s.clone(), SourceOrigin::Config))
        .collect();
    crate::cli::wait::validate_wait_event_sources(&configured)
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

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        author: AgentLabel,
    }

    impl Fixture {
        /// An agent whose config already carries a binding, an explicit
        /// auto mode and a pending resume, so a write that drops any of
        /// them shows.
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().to_path_buf();
            let author = AgentLabel::parse("master").unwrap();
            let cfg: clank_core::agent_config::AgentConfig = serde_json::from_value(
                serde_json::json!({
                    "auto_mode": "on",
                    "session": {"id": "session-0001", "tool": "claude", "updated_at": "2026-11-04T00:00:00Z"},
                    "resume_pending": {"session": "session-0001", "at": "2026-11-04T00:00:00Z"},
                }),
            )
            .unwrap();
            let path = crate::agent_store::agent_config_path(&repo, &author);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
            Self {
                _dir: dir,
                repo,
                author,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(crate::agent_store::agent_config_path(
                &self.repo,
                &self.author,
            ))
            .unwrap()
        }

        fn config(&self) -> clank_core::agent_config::AgentConfig {
            crate::agent_store::load_agent_config(&self.repo, &self.author)
                .unwrap()
                .unwrap()
        }

        fn add(&self, raw: &str) -> anyhow::Result<(usize, WaitEventSource)> {
            add(&self.repo, &self.author, raw)
        }

        fn lines(&self) -> Vec<String> {
            list(&self.repo, &self.author)
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, s)| entry_line(i + 1, s))
                .collect()
        }

        /// `raw` is refused with a message containing `why`, and the
        /// config file is byte-for-byte what it was.
        fn refused(&self, raw: &str, why: &str) {
            let before = self.bytes();
            let err = format!("{:#}", self.add(raw).expect_err(raw));
            assert!(err.contains(why), "{raw}: {err}");
            assert_eq!(self.bytes(), before, "{raw} changed the config");
        }
    }

    #[test]
    fn add_writes_each_kind_and_list_shows_it() {
        let f = Fixture::new();
        let mut want = Vec::new();
        for raw in [GITHUB, COMMAND, TIMER] {
            let source = crate::cli::wait::parse_wait_event_source(raw).unwrap();
            let (n, added) = f.add(raw).unwrap();
            assert_eq!(added, source);
            want.push(source);
            assert_eq!(n, want.len());
        }
        assert_eq!(f.config().wait_events, want);
        assert_eq!(
            f.lines(),
            [
                "1. github o/r: pr #7 ci_passed,ci_failed (prompt)",
                r#"2. command ci: ["sh","-c","tail -f log"]"#,
                "3. timer check-deploy: due 2030-10-04T09:00:00+11:00, every 24h",
            ]
        );
    }

    #[test]
    fn a_flat_watch_without_a_prompt_says_so() {
        let f = Fixture::new();
        f.add(r#"{"kind":"github","repo":"o/r","events":["pr_comment","branch_push"],"prs":[7,9],"branches":["main"]}"#)
            .unwrap();
        assert_eq!(
            f.lines(),
            ["1. github o/r: pr_comment,branch_push on #7,#9 on branches main (no prompt)"]
        );
    }

    #[test]
    fn writes_keep_the_agents_other_fields() {
        let f = Fixture::new();
        let before = f.config();
        f.add(GITHUB).unwrap();
        f.add(TIMER).unwrap();
        remove(&f.repo, &f.author, 1).unwrap();
        let after = f.config();
        assert_eq!(after.session, before.session);
        assert_eq!(after.auto_mode, before.auto_mode);
        assert_eq!(after.resume_pending, before.resume_pending);
        assert!(after.session.is_some() && after.auto_mode.is_some());
        assert!(after.resume_pending.is_some());
    }

    #[test]
    fn add_refuses_what_an_arm_refuses_and_writes_nothing() {
        let f = Fixture::new();
        f.add(GITHUB).unwrap();
        f.refused("{not json", "invalid wake source");
        f.refused(
            r#"{"kind":"carrier_pigeon","name":"x"}"#,
            "unknown variant `carrier_pigeon`",
        );
        f.refused(
            r#"{"kind":"github","repo":"o/r","events":["pr_bogus"]}"#,
            "unknown variant `pr_bogus`",
        );
        f.refused(
            r#"{"kind":"github","repo":"o/r","watch":[{"commit":{"events":["push"]}}]}"#,
            "unknown variant `commit`",
        );
        f.refused(
            r#"{"kind":"github","repo":"o/r","watch":[{"pr":{"events":[]}}]}"#,
            "its `pr` clause names no events",
        );
        f.refused(GITHUB, "already has this source as entry 1");
    }

    /// The arm's own words, not a copy of them: each refusal is the
    /// message `resolve_wait_event_sources` gives the same source.
    #[test]
    fn add_refuses_with_the_arms_message() {
        let f = Fixture::new();
        for raw in [
            r#"{"kind":"github","repo":"o/r","events":["pr_bogus"]}"#,
            r#"{"kind":"command","name":"x","command":[]}"#,
            r#"{"kind":"timer","name":"t","due_at":"tomorrow"}"#,
        ] {
            let arm = crate::cli::wait::resolve_wait_event_sources(
                &f.repo,
                &f.author,
                &[raw.to_string()],
            )
            .expect_err(raw);
            let arm = format!("{:#}", arm);
            let add = format!("{:#}", f.add(raw).expect_err(raw));
            assert!(arm.ends_with(&add), "arm: {arm}\nadd: {add}");
        }
    }

    /// Sources that differ but cannot run side by side.
    #[test]
    fn add_refuses_a_source_that_collides_with_the_list() {
        let f = Fixture::new();
        f.add(TIMER).unwrap();
        f.add(COMMAND).unwrap();
        f.refused(
            r#"{"kind":"timer","name":"check-deploy","due_at":"2031-01-01T00:00:00+00:00"}"#,
            "duplicate timer name",
        );
        f.refused(
            r#"{"kind":"command","name":"ci","command":["other"]}"#,
            "share the storage key",
        );
    }

    #[test]
    fn a_command_source_may_carry_a_receipt_command() {
        let f = Fixture::new();
        f.add(COMMAND).unwrap();
        assert!(matches!(
            &f.config().wait_events[0],
            WaitEventSource::Command(c) if c.receipt_command.is_some()
        ));
    }

    #[test]
    fn remove_deletes_exactly_entry_n() {
        let f = Fixture::new();
        for raw in [GITHUB, COMMAND, TIMER] {
            f.add(raw).unwrap();
        }
        let all = f.config().wait_events;
        let removed = remove(&f.repo, &f.author, 2).unwrap();
        assert_eq!(removed, all[1]);
        assert_eq!(f.config().wait_events, [all[0].clone(), all[2].clone()]);
    }

    #[test]
    fn remove_repairs_a_config_whose_other_sources_an_arm_refuses() {
        let f = Fixture::new();
        let broken = |name: &str| {
            WaitEventSource::Command(
                serde_json::from_value(serde_json::json!({"name": name, "command": []})).unwrap(),
            )
        };
        let mut cfg = f.config();
        let others = cfg.clone();
        cfg.wait_events = vec![broken("first"), broken("second")];
        std::fs::write(
            crate::agent_store::agent_config_path(&f.repo, &f.author),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
        crate::cli::wait::resolve_wait_event_sources(&f.repo, &f.author, &[])
            .expect_err("an arm refuses this config");

        assert_eq!(remove(&f.repo, &f.author, 1).unwrap(), broken("first"));
        assert_eq!(f.config().wait_events, [broken("second")]);
        assert_eq!(remove(&f.repo, &f.author, 1).unwrap(), broken("second"));
        let after = f.config();
        assert!(after.wait_events.is_empty());
        assert_eq!(after.session, others.session);
        assert_eq!(after.auto_mode, others.auto_mode);
        assert_eq!(after.resume_pending, others.resume_pending);
    }

    #[test]
    fn remove_refuses_an_entry_list_does_not_show() {
        let f = Fixture::new();
        f.add(GITHUB).unwrap();
        for n in [0, 2] {
            let before = f.bytes();
            let err = remove(&f.repo, &f.author, n).expect_err("out of range");
            assert!(
                err.to_string().contains(&format!("no wake source {n}")),
                "{err}"
            );
            assert_eq!(f.bytes(), before);
        }
    }

    #[test]
    fn a_refusal_creates_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("nobody").unwrap();
        add(
            dir.path(),
            &author,
            r#"{"kind":"github","repo":"o/r","events":[]}"#,
        )
        .expect_err("no events");
        remove(dir.path(), &author, 1).expect_err("nothing to remove");
        assert!(!crate::agent_store::agent_config_path(dir.path(), &author).exists());
    }
}
