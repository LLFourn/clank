//! Explicit answer submission. No service or retry loop: the ack command owns
//! the callback, and the event WAL owns recovery of its immutable intent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clank_core::wait::WaitItem;

use super::{Sourced, resolve};
use crate::cli::github::log::{EventLog, EventReply, EventRow, IngestLease};

pub(super) const REPLY_BUDGET: Duration = Duration::from_secs(5);

struct Copy {
    dir: PathBuf,
    row: EventRow,
}

pub(super) async fn ack_with_reply(
    repo: &Path,
    dir: &Path,
    rows: Vec<Sourced>,
    ids: &[String],
    text: &str,
) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        anyhow::bail!("--reply must contain an answer, not empty or whitespace-only text");
    }
    if ids.len() != 1 {
        anyhow::bail!(
            "--reply accepts one event selector; acknowledge unrelated events separately"
        );
    }
    let selected = resolve(&rows, &ids[0])?;
    if matches!(selected.row.item, WaitItem::Message { .. }) {
        return super::operation::submit(repo, dir, selected, text, None, true).await;
    }
    let WaitItem::CommandEvent {
        id, reply_route, ..
    } = &selected.row.item
    else {
        anyhow::bail!("this event's backend does not support --reply");
    };
    let repo = dunce::canonicalize(repo)?;
    let dir = dunce::canonicalize(dir)?;
    // Full persisted routing separates same-named sources owned by different
    // agents/backends. Genuine copies retain the same route, lease and key.
    // An invocation source's resolved WAL key already carries its scope.
    let identity =
        serde_json::to_vec(&("clank-reply-v1", &repo, &selected.source, id, reply_route))?;
    let key = blake3::hash(&identity).to_hex().to_string();
    let route = reply_route.clone().ok_or_else(|| anyhow::anyhow!(
        "this event has no saved reply_command; it stays owed (configure reply_command before emitting reply-required events)"
    ))?;
    let (_lease, owner_path) = backend_lease(&repo, &selected.source, &route)?;

    // Re-read after claiming the lease: a competing ack may have completed
    // while the initial selector was being inspected.
    let copies = copies_of(&repo, &selected.source, id, reply_route.as_ref())?;
    if !copies.iter().any(|copy| {
        copy.dir == dir && copy.row.seq == selected.row.seq && copy.row.ident == selected.row.ident
    }) {
        anyhow::bail!("the selected event disappeared; refresh clank events list");
    }
    let existing = copies.iter().find_map(|copy| copy.row.reply.as_ref());
    let reply = match existing {
        Some(reply) => {
            if reply.text != text || reply.id != *id || reply.idempotency_key != key {
                anyhow::bail!(
                    "this event already has a different reply intent; retry its original answer verbatim"
                );
            }
            reply.clone()
        }
        None => {
            let author = dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("cannot identify this event's author"))?;
            EventReply {
                id: id.clone(),
                text: text.into(),
                author: author.into(),
                idempotency_key: key,
                route,
            }
        }
    };
    if reply
        .route
        .command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        anyhow::bail!("the event's saved reply_command is empty; it stays owed");
    }
    for copy in &copies {
        if copy
            .row
            .reply
            .as_ref()
            .is_some_and(|stored| stored != &reply)
        {
            anyhow::bail!("stored copies have conflicting reply intents; no reply was sent");
        }
    }
    // A visible acceptance whose earlier sync failed must be synced now,
    // before it can suppress another callback or complete a fanout.
    for copy in copies.iter().filter(|copy| copy.row.reply_accepted) {
        EventLog::open(&copy.dir, &selected.source)?.accept_reply(
            copy.row.seq,
            &copy.row.ident,
            &reply,
        )?;
    }
    let pending: Vec<_> = copies.iter().filter(|copy| !copy.row.acked).collect();
    if pending.is_empty() {
        if copies.iter().any(|copy| copy.row.reply_accepted) {
            println!("{}: reply already accepted; handled", ids[0]);
            return Ok(());
        }
        anyhow::bail!("the event was already handled without a reply; no answer was sent");
    }
    // All members are validated before the first intent write. A partial local
    // write failure sends nothing; retry adopts the same durable transaction.
    for copy in &pending {
        EventLog::open(&copy.dir, &selected.source)?.append_reply_intent(
            copy.row.seq,
            &copy.row.ident,
            &reply,
        )?;
    }
    if !copies.iter().any(|copy| copy.row.reply_accepted) {
        deliver_reply(&reply, true, REPLY_BUDGET, &owner_path)
            .await
            .context(
                "reply was not confirmed; event stays owed; retry --reply with the same text",
            )?;
    }
    // Acceptance+retirement is one record per WAL. If a fanout is interrupted,
    // an accepted member lets the next explicit retry finish the other copies
    // without invoking the backend again.
    for copy in &pending {
        EventLog::open(&copy.dir, &selected.source)?.accept_reply(
            copy.row.seq,
            &copy.row.ident,
            &reply,
        )?;
    }
    println!("{}: reply accepted; handled", ids[0]);
    Ok(())
}

/// The lease that serializes every reply to one saved backend, and the
/// record of the callback it owns: one per backend, not one per question,
/// which also bounds simultaneous callbacks to it.
pub(super) fn backend_lease(
    repo: &Path,
    source: &str,
    route: &clank_core::wait::CommandReplyRoute,
) -> anyhow::Result<(IngestLease, PathBuf)> {
    let owner_identity = serde_json::to_vec(&("clank-reply-owner-v1", repo, source, Some(route)))?;
    let owner_key = blake3::hash(&owner_identity).to_hex().to_string();
    let lease = IngestLease::try_acquire(
        &crate::agent_store::agents_root(repo),
        &format!("reply-{owner_key}"),
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "a reply to this source is already in progress; retry after that command ends"
        )
    })?;
    Ok((
        lease,
        crate::agent_store::agents_root(repo).join(format!("reply-{owner_key}.owner.json")),
    ))
}

fn copies_of(
    repo: &Path,
    source: &str,
    id: &str,
    route: Option<&clank_core::wait::CommandReplyRoute>,
) -> anyhow::Result<Vec<Copy>> {
    let mut copies = Vec::new();
    for entry in std::fs::read_dir(crate::agent_store::agents_root(repo))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path().join("events");
        if !dir.is_dir() {
            continue;
        }
        let rows = EventLog::open(&dir, source)?.read_rows()?.ok_or_else(|| {
            anyhow::anyhow!("{} is corrupt; refusing reply fanout", dir.display())
        })?;
        for row in rows.rows {
            if matches!(&row.item, WaitItem::CommandEvent { id: other, reply_route, .. } if other == id && reply_route.as_ref() == route)
            {
                copies.push(Copy {
                    dir: dir.clone(),
                    row,
                });
            }
        }
    }
    copies
        .sort_by(|a, b| (&a.dir, a.row.seq, &a.row.ident).cmp(&(&b.dir, b.row.seq, &b.row.ident)));
    Ok(copies)
}

/// A reply keeps its existing immutable route and five-second budget; the
/// common executor knows nothing about replies or event acknowledgement.
/// `acknowledge` tells the backend whether this is the event's one answer or
/// a further reply; `idempotency_key` tells a retry from another reply.
pub(super) async fn deliver_reply(
    reply: &EventReply,
    acknowledge: bool,
    budget: Duration,
    owner_path: &Path,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    let call_deadline = deadline - budget.min(Duration::from_secs(1)) / 5;
    let encoded = serde_json::to_vec(&serde_json::json!({
        "version": 1, "id": reply.id, "reply": reply.text,
        "author": reply.author, "idempotency_key": reply.idempotency_key,
        "acknowledge": acknowledge,
    }))?;
    crate::cli::owned_callback::deliver(
        crate::cli::owned_callback::Request {
            argv: &reply.route.command,
            cwd: &reply.route.cwd,
            state_dir: &reply.route.state_dir,
            stdin: &encoded,
        },
        call_deadline,
        deadline,
        owner_path,
    )
    .await
    .map_err(|error| anyhow::anyhow!("reply_command delivery failed: {error:#}"))
}

#[cfg(test)]
use crate::cli::owned_callback::{Owner as ReplyOwner, save_owner};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::command_source as cs;
    use crate::cli::github::log::Transport;
    use clank_core::agent_config::CommandSource;
    use clank_core::wait::CommandReplyRoute;

    const KEY: &str = "command-questions-0000-inv-one";
    const ID: &str = "utt/a/雪\n\"$()`";
    const ANSWER: &str = "  Use option β.\nKeep this newline.  ";

    fn directory(repo: &Path, agent: &str) -> PathBuf {
        crate::agent_store::agents_root(repo)
            .join(agent)
            .join("events")
    }

    fn route(repo: &Path, script: &str) -> CommandReplyRoute {
        let state = repo.join("original-state");
        std::fs::create_dir_all(&state).unwrap();
        CommandReplyRoute {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "reply-fixture".into(),
                repo.join("request").to_str().unwrap().into(),
                repo.join("fail").to_str().unwrap().into(),
            ],
            cwd: repo.into(),
            state_dir: Some(state),
        }
    }

    // Backend records every attempt, but commits the logical answer once.
    const BACKEND: &str = r#"
        cat > "$1"
        printf '%s\n' "$CLANK_SOURCE_STATE_DIR" > "$1.state"
        pwd > "$1.cwd"
        printf 'attempt\n' >> "$1.attempts"
        if [ -f "$2" ]; then printf 'backend unavailable\n' >&2; exit 7; fi
        if [ -f "$1.answer" ]; then cmp -s "$1" "$1.answer" || exit 9
        else cp "$1" "$1.answer"; fi
    "#;

    fn seed(repo: &Path, agent: &str, route: Option<&CommandReplyRoute>, required: bool) -> String {
        let dir = directory(repo, agent);
        let src = CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: "questions".into(),
            command: vec!["true".into()],
            reply_command: route.map(|r| r.command.clone()),
            receipt_command: None,
            glyph: None,
            stop_grace: None,
        };
        let mut log = EventLog::open(&dir, KEY).unwrap();
        log.load();
        let mut seen = cs::Seen::at(&dir, KEY);
        let raw = serde_json::json!({"id":ID,"content":"Which option?","requires_reply":required});
        let parsed = cs::ingest_line_with_route(&src, &seen, &raw.to_string(), route).unwrap();
        let mut selector = None;
        assert!(cs::apply(parsed, KEY, &mut log, &mut seen, |item| {
            selector = item.event_view().unwrap().selector.map(str::to_owned);
            true
        }));
        selector.unwrap()
    }

    async fn submit(repo: &Path, agent: &str, selector: &str, answer: &str) -> anyhow::Result<()> {
        let dir = directory(repo, agent);
        ack_with_reply(
            repo,
            &dir,
            super::super::read_all(&dir)?,
            &[selector.into()],
            answer,
        )
        .await
    }

    fn rows(repo: &Path, agent: &str) -> Vec<EventRow> {
        EventLog::open(&directory(repo, agent), KEY)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
    }

    fn attempts(repo: &Path) -> usize {
        std::fs::read_to_string(repo.join("request.attempts"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[tokio::test]
    async fn required_event_reaches_backend_verbatim_and_retires_genuine_copies_once() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        seed(repo.path(), "beta", Some(&route), true);
        // This source has the same name and ID but belongs to another backend.
        let mut unrelated = route.clone();
        unrelated.state_dir = Some(repo.path().join("other-state"));
        seed(repo.path(), "gamma", Some(&unrelated), true);
        let dir = directory(repo.path(), "alpha");
        let error = super::super::ack(
            &dir,
            super::super::read_all(&dir).unwrap(),
            &[selector.clone()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("--reply"));
        submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap();
        let request: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repo.path().join("request")).unwrap()).unwrap();
        assert_eq!(request["version"], 1);
        assert_eq!(request["id"], ID);
        assert_eq!(request["reply"], ANSWER);
        assert_eq!(request["author"], "alpha");
        assert_eq!(request["idempotency_key"].as_str().unwrap().len(), 64);
        assert_eq!(
            std::fs::read_to_string(repo.path().join("request.state"))
                .unwrap()
                .trim(),
            route.state_dir.as_ref().unwrap().to_str().unwrap()
        );
        assert_eq!(
            dunce::canonicalize(
                std::fs::read_to_string(repo.path().join("request.cwd"))
                    .unwrap()
                    .trim()
            )
            .unwrap(),
            dunce::canonicalize(&route.cwd).unwrap()
        );
        for agent in ["alpha", "beta"] {
            let row = &rows(repo.path(), agent)[0];
            assert!(row.acked && row.reply_accepted);
            assert_eq!(row.reply.as_ref().unwrap().text, ANSWER);
        }
        assert!(!rows(repo.path(), "gamma")[0].acked);
        submit(repo.path(), "beta", &selector, ANSWER)
            .await
            .unwrap();
        assert_eq!(attempts(repo.path()), 1);
        assert!(
            submit(repo.path(), "beta", &selector, "different")
                .await
                .is_err()
        );
        assert_eq!(attempts(repo.path()), 1);
    }

    // Backend that takes every reply it is given, recording each request.
    const MULTI: &str = r#"
        cat > "$1"
        cat "$1" >> "$1.all"
        printf '\n' >> "$1.all"
        if [ -f "$2" ]; then printf 'backend unavailable\n' >&2; exit 7; fi
    "#;

    fn requests(repo: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(repo.join("request.all"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    async fn deliberate(
        repo: &Path,
        agent: &str,
        selector: &str,
        text: &str,
        operation: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = directory(repo, agent);
        let rows = super::super::read_all(&dir)?;
        let selected = resolve(&rows, selector)?;
        super::super::operation::submit(repo, &dir, selected, text, operation, false).await
    }

    fn operations(repo: &Path, agent: &str) -> Vec<crate::cli::github::log::ReplyOperationState> {
        rows(repo, agent).remove(0).replies
    }

    /// Each deliberate reply is its own operation, sent once with its own
    /// key; a retry names one and sends nothing new once it was accepted.
    /// None of them answers the event, and none touches another copy.
    #[tokio::test]
    async fn a_command_event_takes_many_replies_each_its_own_operation() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), MULTI);
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        seed(repo.path(), "beta", Some(&route), true);
        deliberate(repo.path(), "alpha", &selector, "progress", None)
            .await
            .unwrap();
        deliberate(repo.path(), "alpha", &selector, "progress", None)
            .await
            .unwrap();
        let sent = requests(repo.path());
        assert_eq!(sent.len(), 2, "the same text sent twice is two replies");
        for (request, op) in sent.iter().zip(operations(repo.path(), "alpha")) {
            assert_eq!(request["id"], ID);
            assert_eq!(request["reply"], "progress");
            assert_eq!(request["acknowledge"], false);
            assert_eq!(request["idempotency_key"], op.operation.id);
            assert!(op.receipt.is_some());
            assert_eq!(op.operation.source_name.as_deref(), Some("questions"));
        }
        assert_ne!(sent[0]["idempotency_key"], sent[1]["idempotency_key"]);
        let first = operations(repo.path(), "alpha")[0].operation.id.clone();
        deliberate(repo.path(), "alpha", &selector, "progress", Some(&first))
            .await
            .unwrap();
        assert_eq!(
            requests(repo.path()).len(),
            2,
            "an accepted retry sends nothing"
        );
        for (text, operation) in [("changed", first.as_str()), ("progress", "unknown")] {
            assert!(
                deliberate(repo.path(), "alpha", &selector, text, Some(operation))
                    .await
                    .is_err()
            );
        }
        assert_eq!(requests(repo.path()).len(), 2);
        let alpha = rows(repo.path(), "alpha").remove(0);
        assert!(!alpha.acked && alpha.item.event_view().unwrap().requires_reply);
        assert!(
            operations(repo.path(), "beta").is_empty(),
            "another copy's history"
        );

        // The acknowledging answer keeps its own key and retires every copy;
        // a further reply after it is delivered and leaves it handled.
        submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap();
        let answer = requests(repo.path()).remove(2);
        assert_eq!(answer["acknowledge"], true);
        assert_eq!(
            answer["idempotency_key"],
            rows(repo.path(), "alpha")[0]
                .reply
                .as_ref()
                .unwrap()
                .idempotency_key
        );
        deliberate(repo.path(), "beta", &selector, "after", None)
            .await
            .unwrap();
        assert_eq!(requests(repo.path()).len(), 4);
        for agent in ["alpha", "beta"] {
            assert!(rows(repo.path(), agent)[0].acked, "{agent}");
        }
    }

    /// Discovery (list and show) says a command reply exists, its ID and
    /// state, and that it went to the event's handler: never its text nor
    /// the handler's argv, cwd or state, pending or accepted, and it records
    /// no presentation.
    #[tokio::test]
    async fn discovery_names_a_command_reply_without_its_body_or_callback() {
        let repo = tempfile::tempdir().unwrap();
        let mut route = route(repo.path(), MULTI);
        route.command.push("CALLBACK-ONLY-SECRET".into());
        let selector = seed(repo.path(), "alpha", Some(&route), false);
        deliberate(repo.path(), "alpha", &selector, "BODY-ONE", None)
            .await
            .unwrap();
        std::fs::write(repo.path().join("fail"), b"fail").unwrap();
        assert!(
            deliberate(repo.path(), "alpha", &selector, "BODY-TWO", None)
                .await
                .is_err()
        );
        let dir = directory(repo.path(), "alpha");
        let sourced = super::super::read_all(&dir).unwrap();
        let shown = super::super::metadata_row(&sourced, resolve(&sourced, &selector).unwrap());
        let text = shown.to_string();
        for secret in [
            "CALLBACK-ONLY-SECRET",
            "BODY-ONE",
            "BODY-TWO",
            "original-state",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let replies = shown["replies"].as_array().unwrap();
        assert_eq!(
            replies
                .iter()
                .map(|r| r["accepted"].clone())
                .collect::<Vec<_>>(),
            [true, false]
        );
        for (reply, op) in replies.iter().zip(operations(repo.path(), "alpha")) {
            assert_eq!(reply["id"], op.operation.id.as_str());
            assert_eq!(
                reply["route"],
                serde_json::json!({"backend": "reply_command"})
            );
        }
        super::super::show_metadata(sourced, &selector, true).unwrap();
        assert_eq!(rows(repo.path(), "alpha")[0].woken_at, None);
    }

    /// A deliberate reply never stands in for the answer a required event
    /// is owed.
    #[tokio::test]
    async fn a_deliberate_reply_does_not_answer_a_required_event() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), MULTI);
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        deliberate(repo.path(), "alpha", &selector, "progress", None)
            .await
            .unwrap();
        let dir = directory(repo.path(), "alpha");
        assert!(
            super::super::ack(&dir, super::super::read_all(&dir).unwrap(), &[selector]).is_err()
        );
        assert!(!rows(repo.path(), "alpha")[0].acked);
    }

    /// A failed delivery leaves the operation pending under an ID the
    /// caller is told; its retry resends exactly it, with the same key.
    #[tokio::test]
    async fn a_failed_reply_stays_pending_and_its_retry_resends_it() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), MULTI);
        let selector = seed(repo.path(), "alpha", Some(&route), false);
        std::fs::write(repo.path().join("fail"), b"fail").unwrap();
        let error = deliberate(repo.path(), "alpha", &selector, "progress", None)
            .await
            .unwrap_err();
        let pending = operations(repo.path(), "alpha").remove(0);
        assert!(pending.receipt.is_none());
        assert!(format!("{error:#}").contains(&pending.operation.id));
        let read = || {
            let dir = directory(repo.path(), "alpha");
            let mut out = Vec::new();
            super::super::render_full(
                &super::super::read_all(&dir).unwrap(),
                &Default::default(),
                &selector,
                true,
                &mut out,
            )
            .unwrap();
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()["replies"][0].clone()
        };
        assert_eq!(read()["operation"]["id"], pending.operation.id.as_str());
        assert_eq!(read()["receipt"], serde_json::Value::Null, "pending");
        std::fs::remove_file(repo.path().join("fail")).unwrap();
        deliberate(
            repo.path(),
            "alpha",
            &selector,
            "progress",
            Some(&pending.operation.id),
        )
        .await
        .unwrap();
        let sent = requests(repo.path());
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1], "the same reply, not a new one");
        assert!(operations(repo.path(), "alpha")[0].receipt.is_some());
        assert_eq!(operations(repo.path(), "alpha").len(), 1);
        assert!(read()["receipt"]["accepted_at"].is_u64(), "accepted");
    }

    /// A backend that takes no replies refuses, naming why, and records
    /// nothing.
    #[tokio::test]
    async fn a_backend_without_replies_refuses_a_deliberate_reply() {
        let repo = tempfile::tempdir().unwrap();
        let selector = seed(repo.path(), "alpha", None, false);
        let error = deliberate(repo.path(), "alpha", &selector, "progress", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reply_command"), "{error}");
        assert!(operations(repo.path(), "alpha").is_empty());
        let dir = directory(repo.path(), "alpha");
        EventLog::open(&dir, "github")
            .unwrap()
            .append_inbox(
                Transport::Poll,
                Some("gh"),
                None,
                None,
                &WaitItem::GithubEvent {
                    presentation: None,
                    repo: "o/r".into(),
                    event: "pr_comment".into(),
                    detail: None,
                    number: Some(1),
                    title: None,
                    actor: None,
                    url: None,
                    instructions: None,
                    content: None,
                },
            )
            .unwrap();
        let error = deliberate(repo.path(), "alpha", "github@1", "progress", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("takes no replies"), "{error}");
    }

    #[tokio::test]
    async fn nonzero_leaves_immutable_intent_and_retry_uses_original_route_and_key() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let selector = seed(repo.path(), "alpha", Some(&route), false);
        std::fs::write(repo.path().join("fail"), b"fail").unwrap();
        let error = submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("backend unavailable"));
        let pending = rows(repo.path(), "alpha").remove(0);
        assert!(!pending.acked && !pending.reply_accepted);
        assert!(pending.item.event_view().unwrap().requires_reply);
        let mut restarted = EventLog::open(&directory(repo.path(), "alpha"), KEY).unwrap();
        let replay = restarted.load().unhandled.remove(0).item;
        assert!(
            replay.event_view().unwrap().requires_reply,
            "a retry wake must ask for the pending answer"
        );
        let intent = pending.reply.unwrap();
        assert_eq!(intent.text, ANSWER);
        // A stale plain-ack resolution must recheck the new pending intent.
        let mut log = EventLog::open(&directory(repo.path(), "alpha"), KEY).unwrap();
        assert!(log.append_checked_ack(pending.seq, &pending.ident).is_err());
        assert!(
            submit(repo.path(), "alpha", &selector, "changed")
                .await
                .is_err()
        );
        assert_eq!(attempts(repo.path()), 1);
        std::fs::remove_file(repo.path().join("fail")).unwrap();
        submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap();
        let accepted = rows(repo.path(), "alpha").remove(0);
        assert!(accepted.acked && accepted.reply_accepted);
        assert_eq!(accepted.reply.unwrap(), intent);
        assert_eq!(attempts(repo.path()), 2);
    }

    #[tokio::test]
    async fn backend_acceptance_before_local_commit_replays_same_idempotent_answer() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        // First delivery deliberately fails locally before an acceptance record.
        // The immutable intent is written by the real submission path.
        std::fs::write(repo.path().join("fail"), b"fail").unwrap();
        assert!(
            submit(repo.path(), "alpha", &selector, ANSWER)
                .await
                .is_err()
        );
        std::fs::remove_file(repo.path().join("fail")).unwrap();
        let pending = rows(repo.path(), "alpha").remove(0);
        let intent = pending.reply.unwrap();
        deliver_reply(
            &intent,
            true,
            REPLY_BUDGET,
            &repo.path().join("test.owner.json"),
        )
        .await
        .unwrap();
        let first_answer = std::fs::read(repo.path().join("request.answer")).unwrap();
        assert!(
            !rows(repo.path(), "alpha")[0].acked,
            "crash window is still owed"
        );
        submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(repo.path().join("request")).unwrap(),
            first_answer
        );
        assert_eq!(
            std::fs::read(repo.path().join("request.answer")).unwrap(),
            first_answer
        );
        assert_eq!(attempts(repo.path()), 3);
        assert!(rows(repo.path(), "alpha")[0].reply_accepted);
    }

    #[tokio::test]
    async fn an_accepted_copy_completes_interrupted_fanout_without_another_callback() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        submit(repo.path(), "alpha", &selector, ANSWER)
            .await
            .unwrap();
        let accepted = rows(repo.path(), "alpha").remove(0);
        let intent = accepted.reply.unwrap();
        seed(repo.path(), "beta", Some(&route), true);
        submit(repo.path(), "beta", &selector, ANSWER)
            .await
            .unwrap();
        assert_eq!(attempts(repo.path()), 1);
        let beta = rows(repo.path(), "beta").remove(0);
        assert!(beta.acked && beta.reply_accepted);
        assert_eq!(beta.reply.unwrap(), intent);
    }

    #[tokio::test]
    async fn absent_unsupported_empty_and_bulk_replies_do_not_retire_any_event() {
        let repo = tempfile::tempdir().unwrap();
        let selector = seed(repo.path(), "alpha", None, true);
        let dir = directory(repo.path(), "alpha");
        let before = std::fs::read(dir.join(format!("{KEY}.jsonl"))).unwrap();
        for text in ["", " \n\t", ANSWER] {
            assert!(submit(repo.path(), "alpha", &selector, text).await.is_err());
        }
        assert!(
            ack_with_reply(
                repo.path(),
                &dir,
                super::super::read_all(&dir).unwrap(),
                &[selector.clone(), selector.clone()],
                ANSWER
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read(dir.join(format!("{KEY}.jsonl"))).unwrap(),
            before
        );
        let mut log = EventLog::open(&dir, "github").unwrap();
        log.append_inbox(
            Transport::Poll,
            Some("gh"),
            None,
            None,
            &WaitItem::GithubEvent {
                presentation: None,
                repo: "o/r".into(),
                event: "pr_comment".into(),
                detail: None,
                number: Some(1),
                title: None,
                actor: None,
                url: None,
                instructions: None,
                content: None,
            },
        )
        .unwrap();
        assert!(
            submit(repo.path(), "alpha", "github@1", ANSWER)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not support")
        );
        assert!(!log.read_rows().unwrap().unwrap().rows[0].acked);
    }

    #[tokio::test]
    async fn missing_handler_and_hanging_process_leave_intent_owed_and_cleanup_is_bounded() {
        let repo = tempfile::tempdir().unwrap();
        let mut route = route(repo.path(), BACKEND);
        route.command = vec![
            repo.path()
                .join("missing-executable")
                .to_str()
                .unwrap()
                .into(),
        ];
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        assert!(
            submit(repo.path(), "alpha", &selector, ANSWER)
                .await
                .is_err()
        );
        let row = rows(repo.path(), "alpha").remove(0);
        assert!(!row.acked && row.reply.is_some());
        let mut intent = row.reply.unwrap();
        intent.route.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo $$ > \"$1.pid\"; sleep 300 & wait".into(),
            "fixture".into(),
            repo.path().join("request").to_str().unwrap().into(),
        ];
        // Bigger than a pipe buffer: stdin writing itself must time out,
        // alongside the child that deliberately never reads it.
        intent.text = "x".repeat(256 * 1024);
        let start = std::time::Instant::now();
        let error = deliver_reply(
            &intent,
            true,
            Duration::from_millis(400),
            &repo.path().join("test.owner.json"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invocation budget"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(repo.path().join("request.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            crate::signal::probe_process(pid),
            crate::signal::Probe::Gone
        );
        assert!(!rows(repo.path(), "alpha")[0].acked);
    }

    #[tokio::test]
    async fn concurrent_reply_is_refused_and_cancelled_callback_leaves_other_events_responsive() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(
            repo.path(),
            "cat > \"$1\"; echo $$ > \"$1.pid\"; sleep 300 & wait",
        );
        let selector = seed(repo.path(), "alpha", Some(&route), true);
        let repo_path = repo.path().to_path_buf();
        let sel = selector.clone();
        let task = tokio::spawn(async move { submit(&repo_path, "alpha", &sel, ANSWER).await });
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(repo.path().join("request.pid"))
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let error = submit(repo.path(), "alpha", &selector, "different")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already in progress"));
        // The callback holds its dispatch lease, never a WAL append lock.
        let dir = directory(repo.path(), "alpha");
        let mut other = EventLog::open(&dir, "other").unwrap();
        let mut item = rows(repo.path(), "alpha")[0].item.clone();
        if let WaitItem::CommandEvent { requires_reply, .. } = &mut item {
            *requires_reply = false;
        }
        other
            .append_inbox(Transport::Command, Some("ordinary"), None, None, &item)
            .unwrap();
        super::super::ack(
            &dir,
            super::super::read_all(&dir).unwrap(),
            &["other@1".into()],
        )
        .unwrap();
        assert!(other.read_rows().unwrap().unwrap().rows[0].acked);
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while crate::signal::probe_process(pid) != crate::signal::Probe::Gone {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!rows(repo.path(), "alpha")[0].acked);
    }

    #[tokio::test]
    async fn an_explicit_retry_retires_a_recorded_survivor_before_starting_a_new_callback() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let intent = EventReply {
            id: ID.into(),
            text: ANSWER.into(),
            author: "alpha".into(),
            idempotency_key: "stable".into(),
            route,
        };
        let path = repo.path().join("test.owner.json");
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let _group = crate::cli::wait::GroupKill::over(Some(pid));
        save_owner(
            &path,
            &ReplyOwner {
                pgid: pid as i32,
                token: crate::proc_identity::token_for(pid as i32).unwrap(),
            },
        )
        .unwrap();
        // The test reaps the simulated previous caller's child concurrently.
        let reaper = tokio::spawn(async move {
            let mut child = child;
            child.wait().await.unwrap()
        });
        deliver_reply(&intent, true, REPLY_BUDGET, &path)
            .await
            .unwrap();
        assert!(!reaper.await.unwrap().success());
        assert_eq!(attempts(repo.path()), 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn corrupt_owner_evidence_refuses_a_new_callback_and_is_preserved() {
        let repo = tempfile::tempdir().unwrap();
        let route = route(repo.path(), BACKEND);
        let intent = EventReply {
            id: ID.into(),
            text: ANSWER.into(),
            author: "alpha".into(),
            idempotency_key: "stable".into(),
            route,
        };
        let path = repo.path().join("test.owner.json");
        std::fs::write(&path, b"unreadable recovery evidence").unwrap();
        assert!(
            deliver_reply(&intent, true, REPLY_BUDGET, &path)
                .await
                .is_err()
        );
        assert_eq!(attempts(repo.path()), 0);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"unreadable recovery evidence"
        );
    }
}
