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

#[test]
fn handover_fences_old_receipts_and_keeps_decisions_until_repo_cleanup() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("alice"));
    ask_with(repo, Some(&label("alice")), "q", "Proceed?", &Kind::Plain).unwrap();
    answer_block(repo, "q", "yes", None).unwrap();
    let first = owed_answers(repo, &label("alice")).remove(0).carries;
    let old = destination(repo).unwrap();
    set_master(repo, None);
    assert!(owed_answers(repo, &label("alice")).is_empty());
    assert_eq!(clean(repo).unwrap(), 0);
    set_master(repo, Some("bob"));
    assert_eq!(
        receive(
            repo,
            &first,
            &old,
            || true,
            &crate::upgrade::process().settling()
        )
        .unwrap(),
        Receipt::Unauthorized
    );
    let new = destination(repo).unwrap();
    let owed = owed_answers(repo, &label("bob")).remove(0);
    assert_eq!(
        receive(
            repo,
            &owed.carries,
            &new,
            || true,
            &crate::upgrade::process().settling()
        )
        .unwrap(),
        Receipt::Retired
    );
    assert!(owed_answers(repo, &label("bob")).is_empty());
    assert!(Pair::read(&root(repo), "q").unwrap().answer.is_some());
    set_master(repo, Some("carol"));
    assert_eq!(owed_answers(repo, &label("carol")).len(), 1);
    assert_eq!(
        clean(repo).unwrap(),
        0,
        "successor has not seen the decision"
    );
    let owed = owed_answers(repo, &label("carol")).remove(0);
    receive(
        repo,
        &owed.carries,
        &destination(repo).unwrap(),
        || true,
        &crate::upgrade::process().settling(),
    )
    .unwrap();
    assert!(clean(repo).unwrap() > 0);
    assert!(Pair::read(&root(repo), "q").is_none());
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
    receive(
        repo,
        &old,
        &destination(repo).unwrap(),
        || true,
        &crate::upgrade::process().settling(),
    )
    .unwrap();
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
    assert_eq!(
        receive(
            repo,
            &old,
            &destination(repo).unwrap(),
            || true,
            &crate::upgrade::process().settling()
        )
        .unwrap(),
        Receipt::Stale
    );
    assert!(scan_blocks(repo)[0].answer.is_none());
}

#[test]
fn a_permission_attempt_is_evidence_for_one_session_not_block_ownership() {
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
        Resolved::Unknown(_)
    ));
    assert!(resolve(repo, "publish", None, By::Person, How::Delivered).is_err());
    assert_eq!(clean(repo).unwrap(), 0);
    let mut submitted = |_: &str| Outcome::Submitted;
    resolve(
        repo,
        "publish",
        None,
        By::Person,
        How::Nudge {
            text: "push it",
            resend: true,
            via: &mut submitted,
        },
    )
    .unwrap();
    assert!(owed_answers(repo, &label("bob")).is_empty());
    set_master(repo, Some("carol"));
    let owed = owed_answers(repo, &label("carol")).remove(0);
    assert!(owed.answer.contains("not permission approval"));
    // Rebinding also invalidates old delivery evidence even without a role change.
    crate::agent_store::bind_session_to_agent(
        repo,
        &label("carol"),
        Tool::Codex,
        &clank_core::SessionId::parse("replacement").unwrap(),
    )
    .unwrap();
    assert_eq!(owed_answers(repo, &label("carol")).len(), 1);
}

#[test]
fn legacy_collisions_migrate_all_custody_without_merging_or_replaying_receipts() {
    let repo = tempfile::tempdir().unwrap();
    let repo = repo.path();
    set_master(repo, Some("master"));
    let old = agents_root(repo).join("removed");
    let other = agents_root(repo).join("second");
    let occurrence = "0123456789abcdef0123456789abcdef";
    write(
        &files(&old, "q").question,
        stamp(occurrence, None, "old question"),
    );
    write(
        &files(&old, "q").answer,
        stamp(occurrence, None, "old answer"),
    );
    write(
        &files(&old, "q").marker,
        serde_json::to_vec(&Marker {
            occurrence: occurrence.into(),
            question_digest: digest("old question"),
            answer_digest: digest("old answer"),
            destination: None,
        })
        .unwrap(),
    );
    write(&files(&other, "q").question, "another question");
    write(&files(&other, "q").attempt, b"unreadable attempt");
    let entries = scan_blocks(repo);
    assert_eq!(entries.len(), 1);
    assert_ne!(entries[0].name, "q");
    assert!(entries[0].unreadable.is_some());
    assert!(owed_answers(repo, &label("master")).is_empty());
    assert_eq!(
        Pair::read(&root(repo), "q").unwrap().answer.as_deref(),
        Some("old answer")
    );
    assert!(!files(&old, "q").question.exists());
    let journal = std::fs::read(root(repo).join("block-migration.json")).unwrap();
    scan_blocks(repo);
    assert_eq!(
        std::fs::read(root(repo).join("block-migration.json")).unwrap(),
        journal
    );
    assert!(clean(repo).unwrap() > 0);
    assert_eq!(
        scan_blocks(repo).len(),
        1,
        "corrupt custody survives cleanup"
    );
}

#[test]
fn orphaned_legacy_delivery_evidence_is_visible_and_never_cleaned() {
    let repo = tempfile::tempdir().unwrap();
    let attempt = agents_root(repo.path()).join("removed/blocks/q.attempt");
    write(&attempt, b"torn unknown delivery");
    let blocks = scan_blocks(repo.path());
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].pending());
    assert!(
        blocks[0]
            .unreadable
            .as_deref()
            .unwrap()
            .contains("no question")
    );
    assert!(clean(repo.path()).is_err());
    assert_eq!(std::fs::read(attempt).unwrap(), b"torn unknown delivery");
}

#[test]
fn damaged_receipts_preserve_custody_in_repository_and_legacy_storage() {
    for legacy in [false, true] {
        for damage in ["malformed", "unreadable", "dangling"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path();
            set_master(repo, Some("master"));
            let dir = if legacy {
                agents_root(repo).join("removed")
            } else {
                root(repo)
            };
            let f = files(&dir, "q");
            let occurrence = "0123456789abcdef0123456789abcdef";
            write(&f.question, stamp(occurrence, None, "Which?"));
            write(&f.answer, stamp(occurrence, None, "B"));
            // A delivery may have captured this answer before its receipt was damaged.
            let carries = Pair::read(&dir, "q").unwrap().carries("q").unwrap();
            match damage {
                "malformed" => write(&f.marker, b"{torn receipt"),
                "unreadable" => std::fs::create_dir(&f.marker).unwrap(),
                "dangling" => std::os::unix::fs::symlink("missing-receipt", &f.marker).unwrap(),
                _ => unreachable!(),
            }
            let blocks = scan_blocks(repo);
            assert_eq!(blocks.len(), 1, "legacy={legacy}, {damage}");
            assert!(blocks[0].pending());
            assert!(blocks[0].unreadable.is_some());
            assert!(owed_answers(repo, &label("master")).is_empty());
            assert!(ended(repo, &label("master")).is_empty());
            let received = receive(
                repo,
                &carries,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling(),
            );
            assert!(
                matches!(received, Ok(Receipt::Stale) | Err(_)),
                "legacy={legacy}, {damage}"
            );
            assert!(ask_with(repo, None, "q", "replacement", &Kind::Plain).is_err());
            assert!(matches!(clean(repo), Ok(0) | Err(_)));
            // Malformed bytes migrate unchanged; an unreadable source stops migration.
            let marker = if damage == "malformed" {
                files(&root(repo), "q").marker
            } else {
                f.marker
            };
            match damage {
                "malformed" => {
                    assert_eq!(std::fs::read(&marker).unwrap(), b"{torn receipt");
                    std::fs::remove_file(&marker).unwrap();
                }
                "unreadable" => std::fs::remove_dir(&marker).unwrap(),
                "dangling" => {
                    assert!(
                        std::fs::symlink_metadata(&marker)
                            .unwrap()
                            .file_type()
                            .is_symlink()
                    );
                    std::fs::remove_file(&marker).unwrap();
                }
                _ => unreachable!(),
            }
            // Only explicit repair makes the absent-receipt case deliverable again.
            assert_eq!(owed_answers(repo, &label("master")).len(), 1);
            assert!(!scan_blocks(repo)[0].pending());
        }
    }
}
