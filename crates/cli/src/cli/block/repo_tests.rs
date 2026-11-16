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
        run(args).await.unwrap();
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
