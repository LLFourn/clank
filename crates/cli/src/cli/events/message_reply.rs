//! Explicit message dispatch. Each boundary holds only its own repo's writer
//! lock; one author lease serializes operations without an idle worker.
use super::Sourced;
use crate::cli::github::log::{IngestLease, MessageOperation};
use anyhow::Context;
use clank_core::{
    ids::AgentLabel,
    wait::{MessageReplyRoute, WaitItem},
};
use std::path::Path;

fn dispatch_lease(repo: &Path, author: &AgentLabel) -> anyhow::Result<IngestLease> {
    crate::cli::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    let path = crate::cli::monitor::runtime_path(repo, &format!("message-dispatch-{author}"));
    IngestLease::try_acquire(
        path.parent().unwrap(),
        path.file_name().unwrap().to_str().unwrap(),
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "a message reply for {author} is already in progress; retry after that command ends"
        )
    })
}
fn endpoint(item: &WaitItem) -> anyhow::Result<MessageReplyRoute> {
    match item {
        WaitItem::Message {
            reply_route: Some(route),
            ..
        } => Ok(route.clone()),
        WaitItem::Message {
            from: Some(label),
            repo: Some(repo),
            ..
        } => {
            let repo = std::path::PathBuf::from(repo);
            anyhow::ensure!(
                repo.is_absolute(),
                "this message has no absolute sender repository; no reply was sent"
            );
            let root = crate::git_io::discover_work_dir(&repo)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "message sender repository is unavailable or is not a Git repository"
                )
            })?;
            let repo = crate::cli::resolve_repo(Some(&root))?;
            Ok(MessageReplyRoute {
                repo,
                agent: AgentLabel::parse(label).map_err(anyhow::Error::msg)?,
            })
        }
        _ => anyhow::bail!("this message has no reply endpoint; its sender must supply --reply-to"),
    }
}
pub(super) fn submit(
    repo: &Path,
    dir: &Path,
    selected: &Sourced,
    text: &str,
    retry: Option<&str>,
    acknowledge: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !text.trim().is_empty(),
        "reply text must not be empty or whitespace-only"
    );
    anyhow::ensure!(
        selected.source == crate::inbox::MESSAGES_SOURCE,
        "message replies require a messages selector"
    );
    let WaitItem::Message { handle, .. } = &selected.row.item else {
        anyhow::bail!("events reply supports messages only; command events use events ack --reply");
    };
    let repo = dunce::canonicalize(repo)?;
    let author = AgentLabel::parse(
        dir.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("cannot identify replying author"))?,
    )
    .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        dunce::canonicalize(dir)?
            == crate::agent_store::agents_root(&repo)
                .join(author.as_str())
                .join("events"),
        "message author directory does not match the selected repository"
    );
    let _lease = dispatch_lease(&repo, &author)?;
    let id = if acknowledge {
        blake3::hash(&serde_json::to_vec(&(
            "message-ack-v1",
            &repo,
            &author,
            selected.row.seq,
            &selected.row.ident,
        ))?)
        .to_hex()
        .to_string()
    } else {
        retry
            .map(str::to_string)
            .unwrap_or_else(crate::inbox::mint_id)
    };
    let state = crate::inbox::message_transaction(&repo, &author, |log| {
        let saved = log
            .message_operations()?
            .into_iter()
            .find(|s| s.operation.id == id);
        let operation = if let Some(saved) = saved {
            let op = saved.operation;
            anyhow::ensure!(
                op.acknowledge == acknowledge
                    && op.original_seq == selected.row.seq
                    && op.original_ident == selected.row.ident
                    && op.original_handle == *handle
                    && op.repo == repo
                    && op.author == author
                    && op.text == text,
                "this operation has different immutable input; retry the original text, or omit --operation for a new deliberate reply"
            );
            op
        } else {
            anyhow::ensure!(
                retry.is_none(),
                "operation is unknown or its retained history was pruned; no reply was sent (omit --operation only to send a new reply)"
            );
            MessageOperation {
                id: id.clone(),
                repo: repo.clone(),
                author: author.clone(),
                original_seq: selected.row.seq,
                original_ident: selected.row.ident.clone(),
                original_handle: handle.clone(),
                route: endpoint(&selected.row.item)?,
                text: text.into(),
                at: crate::cli::github::log::now_epoch(),
                acknowledge,
            }
        };
        Ok(log.begin_message_operation(&operation)?)
    })?;
    println!("reply operation: {}", state.operation.id);
    let retry_hint = if acknowledge {
        "retry ack --reply with the same text".into()
    } else {
        format!(
            "retry events reply with --operation {} and the same text",
            state.operation.id
        )
    };
    let receipt = if let Some(receipt) = &state.receipt {
        // Re-sync before a visible acceptance suppresses delivery after a
        // previous uncertain fsync. No endpoint membership check can undo it.
        crate::inbox::message_transaction(&repo, &author, |log| {
            Ok(log.accept_message_operation(&state, receipt)?)
        })?;
        receipt.clone()
    } else {
        let op = &state.operation;
        let delivered = (|| -> anyhow::Result<_> {
            anyhow::ensure!(
                dunce::canonicalize(&op.route.repo)? == op.route.repo,
                "saved reply repository now resolves elsewhere; refusing retargeting"
            );
            crate::inbox::message_transaction(&op.route.repo, &op.route.agent, |log| {
                if log.message_delivery_receipt(op)?.is_none() {
                    let root =
                        crate::git_io::discover_work_dir(&op.route.repo)?.ok_or_else(|| {
                            anyhow::anyhow!("saved reply repository is no longer a Git repository")
                        })?;
                    anyhow::ensure!(
                        dunce::canonicalize(root)? == op.route.repo,
                        "saved reply repository no longer names its Git root"
                    );
                    crate::cli::send::recipient(
                        &op.route.repo,
                        Some(op.route.agent.as_str()),
                        false,
                    )?;
                }
                Ok(log.accept_message_delivery(op)?)
            })
        })()
        .with_context(|| {
            format!(
                "reply destination has not confirmed acceptance; original stays owed; {retry_hint}"
            )
        })?;
        crate::inbox::message_transaction(&repo, &author, |log| {
            Ok(log.accept_message_operation(&state, &delivered)?)
        })
        .with_context(|| {
            format!("destination accepted, but source commit was not confirmed; {retry_hint}")
        })?;
        delivered
    };
    // Cleanup is allowed only after source acceptance above. A removed/offline
    // endpoint cannot turn a completed reply into failure or another delivery.
    if let Err(error) = (|| -> anyhow::Result<()> {
        anyhow::ensure!(
            dunce::canonicalize(&state.operation.route.repo)? == state.operation.route.repo,
            "saved reply repository resolves elsewhere; cleanup deferred"
        );
        crate::inbox::message_transaction(
            &state.operation.route.repo,
            &state.operation.route.agent,
            |log| Ok(log.release_message_receipt(&receipt)?),
        )
    })() {
        eprintln!("clank: reply accepted; receipt-pin cleanup deferred: {error:#}");
    }
    println!(
        "reply accepted{}",
        if acknowledge {
            "; handled"
        } else {
            "; original acknowledgement unchanged"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github::log::EventLog;
    use crate::cli::{EventsAckArgs, EventsArgs, EventsCmd, EventsReplyArgs};
    const TEAM: &str = r#"{"agents":{"worker":{"tool":"codex","role":"master"},"coordinator":{"tool":"claude","role":"commit"}}}"#;
    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }
    fn repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir(repo.path().join(".clank")).unwrap();
        std::fs::write(repo.path().join(".clank/config.json"), TEAM).unwrap();
        repo
    }
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let source = repo();
        let target = repo();
        crate::inbox::deliver_with_options(
            source.path(),
            &label("worker"),
            None,
            "2026-10-04T00:00:00Z",
            "Please answer",
            false,
            Some(&MessageReplyRoute {
                repo: dunce::canonicalize(target.path()).unwrap(),
                agent: label("coordinator"),
            }),
        )
        .unwrap();
        (source, target)
    }
    fn log(repo: &Path, author: &str) -> EventLog {
        EventLog::open(
            &crate::agent_store::agents_root(repo)
                .join(author)
                .join("events"),
            "messages",
        )
        .unwrap()
    }
    async fn ack(repo: &Path, text: &str) -> anyhow::Result<()> {
        crate::cli::events::run(EventsArgs {
            command: EventsCmd::Ack(EventsAckArgs {
                repo: Some(repo.into()),
                author: Some("worker".into()),
                ids: vec!["messages@1".into()],
                reply: Some(text.into()),
            }),
        })
        .await
    }
    async fn reply(repo: &Path, text: &str, operation: Option<&str>) -> anyhow::Result<()> {
        crate::cli::events::run(EventsArgs {
            command: EventsCmd::Reply(EventsReplyArgs {
                repo: Some(repo.into()),
                author: Some("worker".into()),
                id: "messages@1".into(),
                operation: operation.map(str::to_string),
                message: vec![text.into()],
                file: None,
            }),
        })
        .await
    }
    #[tokio::test]
    async fn cli_ack_and_deliberate_replies_keep_verbatim_bodies_and_separate_operations() {
        let (source, target) = fixture();
        let text = "  Answer β.\nKeep whitespace.  ";
        ack(source.path(), text).await.unwrap();
        ack(source.path(), text).await.unwrap();
        assert!(ack(source.path(), "different ack").await.is_err());
        assert_eq!(
            log(target.path(), "coordinator")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            log(source.path(), "worker").message_operations().unwrap()[0]
                .operation
                .route
                .repo,
            dunce::canonicalize(target.path()).unwrap()
        );
        assert!(
            log(source.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows[0]
                .acked
        );
        reply(source.path(), "same follow-up", None).await.unwrap();
        reply(source.path(), "same follow-up", None).await.unwrap();
        let operations = log(source.path(), "worker").message_operations().unwrap();
        assert_eq!(operations.len(), 3);
        let detail = log(source.path(), "worker")
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
            .remove(0);
        let detail = serde_json::to_value(detail).unwrap();
        assert_eq!(detail["message_replies"][0]["operation"]["text"], text);
        assert_eq!(detail["message_replies"].as_array().unwrap().len(), 3);
        reply(
            source.path(),
            "same follow-up",
            Some(&operations[2].operation.id),
        )
        .await
        .unwrap();
        assert!(
            reply(
                source.path(),
                "different",
                Some(&operations[2].operation.id)
            )
            .await
            .is_err()
        );
        assert!(
            reply(source.path(), "same follow-up", Some("unknown-or-pruned"))
                .await
                .is_err()
        );
        let rows = log(target.path(), "coordinator")
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].item.event_view().unwrap().body, Some(text));
        assert!(
            matches!(&rows[0].item, WaitItem::Message { from: Some(from), repo: Some(repo), requires_reply: false, .. } if from == "worker" && Path::new(repo) == dunce::canonicalize(source.path()).unwrap())
        );
    }
    #[tokio::test]
    async fn missing_endpoint_leaves_intent_and_retry_does_not_retarget_to_master() {
        let (source, target) = fixture();
        std::fs::write(
            target.path().join(".clank/config.json"),
            r#"{"agents":{"worker":{"tool":"codex","role":"master"}}}"#,
        )
        .unwrap();
        assert!(reply(source.path(), "progress", None).await.is_err());
        let operations = log(source.path(), "worker").message_operations().unwrap();
        assert_eq!(operations.len(), 1);
        assert!(operations[0].receipt.is_none());
        assert!(
            !log(source.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows[0]
                .acked
        );
        assert!(
            log(target.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .is_empty()
        );
        std::fs::write(target.path().join(".clank/config.json"), TEAM).unwrap();
        reply(source.path(), "progress", Some(&operations[0].operation.id))
            .await
            .unwrap();
        assert_eq!(
            log(target.path(), "coordinator")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .len(),
            1
        );
        assert!(
            !log(source.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows[0]
                .acked
        );
    }
    #[tokio::test]
    async fn recovery_finishes_existing_acceptance_even_after_recipient_removal() {
        let (source, target) = fixture();
        // Simulate interruption after destination acceptance, before source commit.
        std::fs::write(
            target.path().join(".clank/config.json"),
            r#"{"agents":{"worker":{"tool":"codex","role":"master"}}}"#,
        )
        .unwrap();
        assert!(ack(source.path(), "done").await.is_err());
        let state = log(source.path(), "worker")
            .message_operations()
            .unwrap()
            .remove(0);
        crate::inbox::message_transaction(target.path(), &label("coordinator"), |log| {
            Ok(log.accept_message_delivery(&state.operation)?)
        })
        .unwrap();
        ack(source.path(), "done").await.unwrap();
        assert!(
            log(source.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows[0]
                .acked
        );
        assert_eq!(
            log(target.path(), "coordinator")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .len(),
            1
        );
    }
    #[tokio::test]
    async fn ordinary_sender_metadata_allows_reply_without_requiring_or_completing_one() {
        let source = repo();
        let target = repo();
        let nested = target.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        crate::inbox::deliver(
            source.path(),
            &label("worker"),
            Some(&crate::inbox::Sender {
                label: "coordinator".into(),
                repo: nested,
            }),
            "2026-10-04T00:00:00Z",
            "hello",
        )
        .unwrap();
        reply(source.path(), "progress", None).await.unwrap();
        let row = log(source.path(), "worker")
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
            .remove(0);
        assert!(!row.acked && !row.item.event_view().unwrap().requires_reply);
        assert_eq!(
            log(target.path(), "coordinator")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .len(),
            1
        );
        let human = repo();
        crate::inbox::deliver(
            human.path(),
            &label("worker"),
            None,
            "2026-10-04T00:00:00Z",
            "hello",
        )
        .unwrap();
        assert!(reply(human.path(), "progress", None).await.is_err());
        assert!(
            log(human.path(), "worker")
                .message_operations()
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn held_author_lease_refuses_dispatch_without_intent_or_delivery() {
        let (source, target) = fixture();
        let canonical = dunce::canonicalize(source.path()).unwrap();
        let lease = dispatch_lease(&canonical, &label("worker")).unwrap();
        assert!(
            ack(source.path(), "done")
                .await
                .unwrap_err()
                .to_string()
                .contains("already in progress")
        );
        assert!(
            log(source.path(), "worker")
                .message_operations()
                .unwrap()
                .is_empty()
        );
        assert!(
            log(target.path(), "coordinator")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows
                .is_empty()
        );
        drop(lease);
        ack(source.path(), "done").await.unwrap();
    }

    #[tokio::test]
    async fn completed_retry_never_recreates_an_unavailable_destination() {
        let (source, target) = fixture();
        ack(source.path(), "done").await.unwrap();
        std::fs::remove_dir_all(target.path()).unwrap();
        ack(source.path(), "done").await.unwrap();
        assert!(!target.path().exists());
        assert!(
            log(source.path(), "worker")
                .read_rows()
                .unwrap()
                .unwrap()
                .rows[0]
                .acked
        );
    }

    #[test]
    fn reciprocal_replies_share_local_writers_without_a_cross_author_deadlock() {
        let repo = repo();
        let root = dunce::canonicalize(repo.path()).unwrap();
        for (author, destination) in [("worker", "coordinator"), ("coordinator", "worker")] {
            crate::inbox::deliver_with_options(
                &root,
                &label(author),
                None,
                "2026-10-04T00:00:00Z",
                "Please answer",
                false,
                Some(&MessageReplyRoute {
                    repo: root.clone(),
                    agent: label(destination),
                }),
            )
            .unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (sent, received) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for author in ["worker", "coordinator"] {
            let root = root.clone();
            let barrier = barrier.clone();
            let sent = sent.clone();
            threads.push(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                barrier.wait();
                let result = runtime.block_on(crate::cli::events::run(EventsArgs {
                    command: EventsCmd::Ack(EventsAckArgs {
                        repo: Some(root),
                        author: Some(author.into()),
                        ids: vec!["messages@1".into()],
                        reply: Some("done".into()),
                    }),
                }));
                sent.send(result.map_err(|e| e.to_string())).unwrap();
            }));
        }
        for _ in 0..2 {
            received
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
        }
        for thread in threads {
            thread.join().unwrap();
        }
        for author in ["worker", "coordinator"] {
            let rows = log(&root, author).read_rows().unwrap().unwrap().rows;
            assert_eq!(rows.len(), 2);
            assert!(rows[0].acked);
            assert!(!rows[1].acked);
            assert_eq!(rows[0].message_replies.len(), 1);
        }
    }
    #[test]
    fn cli_reply_body_and_retry_id_use_the_actual_argument_parser() {
        use clap::Parser;
        let cli = crate::cli::command::Cli::try_parse_from([
            "clank",
            "events",
            "reply",
            "messages@1",
            "--operation",
            "retained",
            "-m",
            "  Body β.\n  ",
        ])
        .unwrap();
        let crate::cli::command::Command::Events(args) = cli.command else {
            panic!("events")
        };
        let EventsCmd::Reply(args) = args.command else {
            panic!("reply")
        };
        assert_eq!(args.operation.as_deref(), Some("retained"));
        assert_eq!(args.message, ["  Body β.\n  "]);
        assert!(
            crate::cli::command::Cli::try_parse_from(["clank", "events", "reply", "messages@1"])
                .is_err()
        );
    }
}
