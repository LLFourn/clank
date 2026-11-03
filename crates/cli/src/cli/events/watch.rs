//! `clank events watch`: an agent's standing wake sources
//! (`wait_events`), edited under the binding lock and refused by the
//! same validation an arm runs.

use std::path::Path;

use clank_core::agent_config::{WaitEventSource, WatchClause};
use clank_core::ids::AgentLabel;

use crate::cli::event_source::SourceOrigin;

pub(super) fn run(cmd: crate::cli::EventsWatchCmd) -> anyhow::Result<()> {
    use crate::cli::EventsWatchCmd;
    match cmd {
        EventsWatchCmd::List(a) => {
            let (repo, author) = target(&a.target)?;
            let sources = list(&repo, &author)?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&sources)?);
            } else if sources.is_empty() {
                println!("{author} has no wake sources");
            } else {
                for (i, s) in sources.iter().enumerate() {
                    println!("{}", entry_line(i + 1, s));
                }
            }
        }
        EventsWatchCmd::Add(a) => {
            let (repo, author) = target(&a.target)?;
            let (n, source) = add(&repo, &author, &a.source)?;
            println!("{}", entry_line(n, &source));
        }
        EventsWatchCmd::Remove(a) => {
            let (repo, author) = target(&a.target)?;
            let removed = remove(&repo, &author, a.n)?;
            println!("removed {}", entry_line(a.n, &removed));
        }
    }
    Ok(())
}

fn target(t: &crate::cli::EventsWatchTarget) -> anyhow::Result<(std::path::PathBuf, AgentLabel)> {
    super::resolve_author(t.repo.as_deref(), t.author.as_deref())
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
