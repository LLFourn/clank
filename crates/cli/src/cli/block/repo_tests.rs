use super::*;
use clank_core::{AgentLabel, Tool};

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}
fn set_master(repo: &Path, who: Option<&str>) {
    crate::agent_store::repo_config::transact(repo, |cfg| {
        for agent in cfg.agents.values_mut() {
            if agent.role == super::super::teams_config::RosterRole::Master {
                agent.role = super::super::teams_config::RosterRole::Standby;
            }
        }
        if let Some(who) = who {
            cfg.agents.insert(
                label(who),
                serde_json::from_value(serde_json::json!({"tool":"codex","role":"master"}))
                    .unwrap(),
            );
        }
        Ok(())
    })
    .unwrap();
    if let Some(who) = who {
        test_master(repo, &label(who));
    }
}
fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn cli_addresses_repository_questions_without_an_agent_or_binding() {
    use crate::cli::command::{Cli, Command};
    use clap::Parser;
    let repo = tempfile::tempdir().unwrap();
    for args in [
        vec!["ask", "schema", "-m", "Keep v1?"],
        vec!["answer", "schema", "-m", "Yes"],
    ] {
        let mut argv = vec!["clank", "block"];
        argv.extend(args);
        argv.extend(["--repo", repo.path().to_str().unwrap()]);
        let Command::Block(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!()
        };
        match args.command {
            BlockCmd::Ask(a) => create(a, &crate::agent_env::Stated::default()).unwrap(),
            command => run(BlockArgs { command }).await.unwrap(),
        }
    }
    assert_eq!(
        Pair::read(&root(repo.path()), "schema")
            .unwrap()
            .answer
            .as_deref(),
        Some("Yes")
    );
    assert!(owed_answers(repo.path(), &label("alice")).is_empty());
    assert!(
        Cli::try_parse_from(["clank", "block", "answer", "alice", "schema", "-m", "yes"]).is_err()
    );
    assert!(
        Cli::try_parse_from([
            "clank", "block", "ask", "schema", "--author", "alice", "-m", "q"
        ])
        .is_err()
    );
    set_master(repo.path(), Some("bob"));
    assert_eq!(owed_answers(repo.path(), &label("bob")).len(), 1);
}

/// An answer follows whoever is master until a wake delivers it, and is
/// then deleted: a later master, or the same master rebound to a new
/// session, is shown nothing again.
#[test]
fn an_answer_follows_the_master_and_a_rebind_replays_nothing() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("alice"));
    ask_with(repo, Some(&label("alice")), "q", "Proceed?", &Kind::Plain).unwrap();
    answer_block(repo, "q", "yes", None).unwrap();
    let first = owed_answers(repo, &label("alice")).remove(0).carries;
    set_master(repo, None);
    assert!(owed_answers(repo, &label("alice")).is_empty());
    assert!(Pair::read(&root(repo), "q").unwrap().answer.is_some());
    set_master(repo, Some("bob"));
    let owed = owed_answers(repo, &label("bob")).remove(0);
    assert_eq!(owed.carries, first, "the same answer, to the new master");
    assert!(complete(repo, &owed.carries, &crate::upgrade::process().settling()).unwrap());
    assert!(Pair::read(&root(repo), "q").is_none());
    crate::agent_store::bind_session_to_agent(
        repo,
        &label("bob"),
        Tool::Codex,
        &clank_core::SessionId::parse("replacement").unwrap(),
    )
    .unwrap();
    assert!(owed_answers(repo, &label("bob")).is_empty());
    set_master(repo, Some("carol"));
    assert!(owed_answers(repo, &label("carol")).is_empty());
}

#[test]
fn names_are_repo_unique_and_old_occurrences_cannot_answer_a_reused_name() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("alice"));
    let attempts = std::thread::scope(|scope| {
        let a = scope.spawn(|| ask_with(repo, None, "q", "first", &Kind::Plain));
        let b = scope.spawn(|| ask_with(repo, Some(&label("bob")), "q", "second", &Kind::Plain));
        [a.join().unwrap().is_ok(), b.join().unwrap().is_ok()]
    });
    assert_eq!(attempts.into_iter().filter(|v| *v).count(), 1);
    answer_block(repo, "q", "yes", None).unwrap();
    let old = owed_answers(repo, &label("alice")).remove(0).carries;
    assert!(complete(repo, &old, &crate::upgrade::process().settling()).unwrap());
    ask_with(repo, None, "q", "again", &Kind::Plain).unwrap();
    assert!(
        answer_block(
            repo,
            "q",
            "old answer",
            Some(Shown {
                occurrence: old.occurrence.as_deref(),
                question: "first"
            })
        )
        .is_err()
    );
    assert!(!complete(repo, &old, &crate::upgrade::process().settling()).unwrap());
    assert!(scan_blocks(repo)[0].answer.is_none());
}

/// A nudge is typed to the master of its claim, and its submission
/// completes the block even if the master changed while it was typed.
#[test]
fn a_submitted_nudge_completes_across_a_handover() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("alice"));
    ask_with(
        repo,
        None,
        "publish",
        "Push was refused",
        &Kind::Permission { suggestion: None },
    )
    .unwrap();
    let mut handover = |_: &str| {
        set_master(repo, Some("bob"));
        Outcome::Submitted
    };
    assert!(matches!(
        resolve(
            repo,
            "publish",
            None,
            By::Person,
            How::Nudge {
                text: "push it",
                resend: false,
                via: &mut handover
            }
        )
        .unwrap(),
        Resolved::Delivered
    ));
    assert!(Pair::read(&root(repo), "publish").is_none());
    assert_eq!(
        crate::cli::nudge_log::read(&agents_root(repo).join("alice")).len(),
        1,
        "typed to the master that claimed it"
    );
}

/// A permission block's recorded answer, which only an older clank could
/// write, reaches a wake as a prior decision, never as an approval.
#[test]
fn a_permission_blocks_owed_answer_is_no_approval() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("carol"));
    let f = files(&root(repo), "publish");
    let occurrence = "0123456789abcdef0123456789abcdef";
    write(
        &f.question,
        stamp_question(occurrence, &Kind::Permission { suggestion: None }, "Push?").unwrap(),
    );
    write(&f.answer, stamp(occurrence, None, "push it"));
    let owed = owed_answers(repo, &label("carol")).remove(0);
    assert!(
        owed.answer.contains("not permission approval"),
        "{}",
        owed.answer
    );
}

/// The per-agent layout is gone, not migrated: its files, an orphan answer
/// among them, are never read or touched, and the repository's own blocks
/// list and answer normally beside them.
#[test]
fn per_agent_block_files_are_ignored_and_left_alone() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("master"));
    let old = agents_root(repo).join("removed");
    let orphan = old.join("unblocks/gone.md");
    write(&orphan, "an answer with no question");
    write(&old.join("blocks/old.md"), "an old question");
    write(&files(&root(repo), "q").question, "Which?");
    let blocks = scan_blocks(repo);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0].name, "q");
    assert!(blocks[0].unreadable.is_none());
    answer_block(repo, "q", "B", None).unwrap();
    assert_eq!(owed_answers(repo, &label("master")).len(), 1);
    assert_eq!(
        std::fs::read_to_string(&orphan).unwrap(),
        "an answer with no question"
    );
    assert!(old.join("blocks/old.md").exists());
}

const MASTER_SID: &str = "01a16cdd-5f28-7bd2-b895-7122737638ce";
const REVIEWER_SID: &str = "01a16cfb-9a15-7801-a3de-671cc0308ebb";
const STANDBY_SID: &str = "01a16cfc-2e23-7352-875f-d74f16dc1305";
const UNBOUND_SID: &str = "01a16cfd-0000-7000-8000-000000000000";

/// A roster of a master, a commit reviewer and a standby agent, each
/// bound to its own Codex session.
fn team() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    crate::agent_store::repo_config::transact(dir.path(), |cfg| {
        for (who, role) in [("boss", "master"), ("rev", "commit"), ("idle", "standby")] {
            cfg.agents.insert(
                label(who),
                serde_json::from_value(serde_json::json!({"tool":"codex","role":role})).unwrap(),
            );
        }
        Ok(())
    })
    .unwrap();
    for (who, sid) in [
        ("boss", MASTER_SID),
        ("rev", REVIEWER_SID),
        ("idle", STANDBY_SID),
    ] {
        crate::agent_store::bind_session_to_agent(
            dir.path(),
            &label(who),
            Tool::Codex,
            &clank_core::SessionId::parse(sid).unwrap(),
        )
        .unwrap();
    }
    dir
}

fn in_session(sid: &str) -> crate::agent_env::Stated {
    crate::agent_env::Stated::saying(&[("CODEX_THREAD_ID", sid)])
}

fn raise_as(
    repo: &Path,
    env: &crate::agent_env::Stated,
    kind: &Kind,
) -> anyhow::Result<(Option<AgentLabel>, PathBuf)> {
    raise(
        repo,
        crate::agent_env::identify_in(repo, env),
        "q",
        "Which API shape?",
        kind,
    )
}

fn permission() -> Kind {
    Kind::Permission {
        suggestion: Some("yes: push it".into()),
    }
}

/// Nothing a block is made of exists: no question, answer, attempt or
/// creator record.
fn nothing_raised(repo: &Path) -> bool {
    let f = files(&root(repo), "q");
    [&f.question, &f.answer, &f.attempt, &f.creator]
        .iter()
        .all(|p| absent(p))
        && question_names(&root(repo)).is_empty()
}

#[test]
fn a_reviewer_is_refused_a_block_and_told_to_request_changes() {
    let repo = team();
    for kind in [Kind::Plain, permission()] {
        let err = raise_as(repo.path(), &in_session(REVIEWER_SID), &kind).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("REQUEST_CHANGES"), "{err}");
        assert!(err.contains("only the master raises blocks"), "{err}");
        assert!(nothing_raised(repo.path()), "{}", kind.name());
    }
}

#[test]
fn a_standby_agent_is_refused_a_block_and_told_to_send_the_master() {
    let repo = team();
    for kind in [Kind::Plain, permission()] {
        let err = raise_as(repo.path(), &in_session(STANDBY_SID), &kind).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("`clank send --master"), "{err}");
        assert!(!err.contains("REQUEST_CHANGES"), "{err}");
        assert!(nothing_raised(repo.path()), "{}", kind.name());
    }
}

#[test]
fn the_master_raises_a_block() {
    let repo = team();
    let (author, path) = raise_as(repo.path(), &in_session(MASTER_SID), &permission()).unwrap();
    assert_eq!(author, Some(label("boss")));
    assert_eq!(path, files(&root(repo.path()), "q").question);
    let pair = Pair::read(&root(repo.path()), "q").unwrap();
    assert_eq!(pair.question, "Which API shape?");
}

#[test]
fn a_caller_with_no_identity_raises_a_block() {
    let repo = team();
    let (author, _) = raise_as(
        repo.path(),
        &crate::agent_env::Stated::default(),
        &Kind::Plain,
    )
    .unwrap();
    assert_eq!(author, None);
    assert!(Pair::read(&root(repo.path()), "q").is_some());
    assert!(absent(&files(&root(repo.path()), "q").creator));
}

/// A session variable that is malformed, ambiguous or unbound, a Grok
/// marker with no session behind it, and a binding or roster that cannot
/// be read all leave the caller unresolved: an agent, never a person.
#[test]
fn an_unresolved_identity_is_refused_and_creates_nothing() {
    use crate::agent_env::Stated;
    let refused = [
        (
            Stated::saying(&[("CODEX_THREAD_ID", "not/a/session")]),
            "is not a valid session id",
        ),
        (
            Stated::saying(&[
                ("CODEX_THREAD_ID", MASTER_SID),
                ("CLAUDE_CODE_SESSION_ID", REVIEWER_SID),
            ]),
            "multiple session env vars",
        ),
        (in_session(UNBOUND_SID), "no agent is bound"),
        (
            Stated::saying(&[("GROK_AGENT", "1"), ("CLANK_AGENT", "boss")]),
            "its session can't be identified",
        ),
        (
            Stated::saying(&[("CLANK_AGENT", "a/b")]),
            "is not a valid agent label",
        ),
    ];
    for (env, why) in refused {
        let repo = team();
        let err = format!(
            "{:#}",
            raise_as(repo.path(), &env, &Kind::Plain).unwrap_err()
        );
        assert!(err.contains("did not resolve"), "{err}");
        assert!(err.contains(why), "{err}");
        assert!(nothing_raised(repo.path()), "{why}");
    }

    let repo = team();
    write(
        &crate::agent_store::agent_config_path(repo.path(), &label("boss")),
        "{ not json",
    );
    let err = format!(
        "{:#}",
        raise_as(repo.path(), &in_session(MASTER_SID), &Kind::Plain).unwrap_err()
    );
    assert!(err.contains("no agent is bound"), "{err}");
    assert!(nothing_raised(repo.path()));

    let repo = team();
    write(
        &crate::agent_store::repo_config::path(repo.path()),
        "{ not json",
    );
    assert!(raise_as(repo.path(), &in_session(MASTER_SID), &Kind::Plain).is_err());
    assert!(nothing_raised(repo.path()));
}
