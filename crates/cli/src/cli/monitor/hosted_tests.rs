use super::*;
use crate::cli::monitor::fixtures::*;

/// What a reconcile CANCELS, it hands back.
///
/// Both paths: an author whose spec changed, and one revoked from
/// the roster. The previous body dropped these handles, so a
/// source removed moments before a stand-down was still winding
/// down while the retirement — looking only at the CURRENT map —
/// reported everything settled (codex on dd67677).
#[tokio::test]
async fn a_reconcile_hands_back_the_producers_it_cancels() {
    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = std::collections::BTreeMap::new();

    let first = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert!(first.is_empty(), "nothing was running to cancel yet");
    assert_eq!(hosted.len(), 1, "and codex's source is hosted");

    // An edited spec replaces that author's set.
    with_source(repo.path(), "codex", "builds");
    let replaced = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert!(
        !replaced.is_empty(),
        "the producers it just cancelled are the caller's to see out"
    );

    // A revoked author's set goes the same way.
    std::fs::write(
        repo.path().join(".clank/config.json"),
        r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
    )
    .unwrap();
    let revoked = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert!(hosted.is_empty(), "a revoked agent hosts nothing");
    assert!(
        !revoked.is_empty(),
        "and its producer is still winding down, so it comes back too"
    );

    // Retiring with that inventory observes them end, which is the
    // whole point of carrying it.
    let decision = retire(
        Vec::new(),
        Vec::new(),
        replaced.into_iter().chain(revoked).collect(),
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;
    assert!(
        matches!(decision, Shutdown::ReadyToRelease(_)),
        "they stopped, so the release is orderly: {decision:?}"
    );
}

fn with_timer(repo: &std::path::Path, name: &str, due: u64) {
    let author = AgentLabel::parse("codex").unwrap();
    crate::agent_store::update_agent_config(repo, &author, |config| {
        config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
            clank_core::agent_config::TimerSource {
                interrupt: false,
                interrupt_message: None,
                name: name.into(),
                repeat_every: None,
                due_at: time::OffsetDateTime::from_unix_timestamp(due as i64)
                    .unwrap()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
                content: Some("scheduled work".into()),
            },
        )];
    })
    .unwrap();
}

/// A hosted timer whose occurrence will not write is tried again by
/// no clock and by no repo change but a timer log's: a fold over
/// anything else leaves it unarmed and owed, and the change re-arms
/// it, so the next deadline wake accepts it.
#[tokio::test]
async fn a_hosted_timer_that_will_not_write_waits_for_a_timer_log_to_change() {
    let repo = repo_with_roster();
    let codex = Producer::Agent(AgentLabel::parse("codex").unwrap());
    with_timer(repo.path(), "owed", 1);
    let config =
        crate::agent_store::load_agent_config(repo.path(), &AgentLabel::parse("codex").unwrap())
            .unwrap()
            .unwrap();
    let clank_core::agent_config::WaitEventSource::Timer(source) = &config.wait_events[0] else {
        unreachable!()
    };
    let events_dir = codex.directory(repo.path());
    let wal = events_dir.join(format!("{}.jsonl", source.source_key(None)));
    std::fs::create_dir_all(&wal).unwrap();
    let spawn = stand_in_spawner(Default::default());
    let (into, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let starts = Starts {
        into: &into,
        lifecycle: None,
        spawn: &spawn,
    };
    let mut hosted = std::collections::BTreeMap::new();
    let reconcile = |hosted: &mut std::collections::BTreeMap<Producer, Hosted>, changed| {
        assert!(reconcile_hosting_with(repo.path(), hosted, changed, &starts).is_empty());
    };
    let due = |hosted: &mut std::collections::BTreeMap<Producer, Hosted>| {
        let set = hosted.get_mut(&codex).unwrap().timers.as_mut().unwrap();
        set.due();
        set.next_deadline()
    };
    reconcile(&mut hosted, false);
    assert_eq!(due(&mut hosted), None, "it failed, and has no deadline");
    std::fs::remove_dir(&wal).unwrap();
    for _ in 0..3 {
        reconcile(&mut hosted, false);
        assert_eq!(due(&mut hosted), None, "nothing but its log tries it");
    }
    assert!(rx.try_recv().is_err(), "still owed, not accepted");

    reconcile(&mut hosted, true);
    assert_eq!(due(&mut hosted), None, "accepted at once");
    let (_, emission) = rx.try_recv().expect("its occurrence");
    assert!(format!("{emission:?}").contains("owed"), "{emission:?}");
    assert!(rx.try_recv().is_err(), "once");
}

#[tokio::test]
async fn recurring_timer_host_rearms_on_ack_invalidation_and_cancels_on_removal() {
    use crate::cli::github::log::EventLog;
    let repo = repo_with_roster();
    let author = AgentLabel::parse("codex").unwrap();
    with_timer(repo.path(), "recurring", 100);
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        let clank_core::agent_config::WaitEventSource::Timer(timer) = &mut config.wait_events[0]
        else {
            unreachable!()
        };
        timer.repeat_every = Some("10s".into());
    })
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted: std::collections::BTreeMap<Producer, Hosted> = Default::default();
    assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
    assert!(rx.try_recv().is_err(), "hosting only arms");
    hosted
        .get_mut(&Producer::Agent(author.clone()))
        .unwrap()
        .timers
        .as_mut()
        .unwrap()
        .due();
    let (_, (item, _)) = rx.try_recv().unwrap();
    let selector = item.event_view().unwrap().selector.unwrap();
    let key = selector.split_once('@').unwrap().0;
    let dir = crate::agent_store::agents_root(repo.path()).join("codex/events");
    let mut log = EventLog::open(&dir, key).unwrap();
    let row = log.read_rows().unwrap().unwrap().rows.remove(0);
    assert!(matches!(
        &row.item,
        WaitItem::TimerEvent {
            selector: None,
            recurrence: Some(_),
            ..
        }
    ));
    assert!(
        hosted[&Producer::Agent(author.clone())]
            .timers
            .as_ref()
            .unwrap()
            .next_deadline()
            .is_none()
    );
    // Record an ack while the wall clock is forward, then refresh after
    // retreat; proven coverage must still arm a future anchored deadline.
    log.ack_recurring_at(
        row.seq,
        &row.ident,
        crate::cli::timer_source::wall_now() + 60,
    )
    .unwrap();
    assert!(reconcile_hosting_changed(repo.path(), &mut hosted, &tx, true).is_empty());
    let deadline = hosted[&Producer::Agent(author.clone())]
        .timers
        .as_ref()
        .unwrap()
        .next_deadline()
        .unwrap();
    assert!(deadline > tokio::time::Instant::now());
    assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
    assert_eq!(
        hosted[&Producer::Agent(author.clone())]
            .timers
            .as_ref()
            .unwrap()
            .next_deadline(),
        Some(deadline)
    );
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events.clear();
    })
    .unwrap();
    assert!(reconcile_hosting_changed(repo.path(), &mut hosted, &tx, true).is_empty());
    assert!(hosted.is_empty());
    assert!(rx.try_recv().is_err());
    assert_eq!(log.read_rows().unwrap().unwrap().rows.len(), 1);
}

#[tokio::test]
async fn rejected_timer_edits_preserve_drivers_and_their_accepted_delivery_spec() {
    use crate::cli::event_source::{EmissionSink, SourceOrigin};
    use crate::cli::github::log::{EventLog, Transport};
    use clank_core::agent_config::WaitEventSource;
    let repo = repo_with_roster();
    let author = AgentLabel::parse("codex").unwrap();
    with_source(repo.path(), "codex", "accepted-command");
    let mut spec = hostable(repo.path(), &author).unwrap();
    let github: WaitEventSource = serde_json::from_value(serde_json::json!({
        "kind":"github", "repo":"o/r", "events":["pr_opened"], "prompt":"accepted instructions"
    }))
    .unwrap();
    spec.push((github, SourceOrigin::Config));
    let timer: WaitEventSource = serde_json::from_value(serde_json::json!({
        "kind":"timer", "name":"future", "due_at":"2040-01-01T00:00:00Z"
    }))
    .unwrap();
    spec.push((timer, SourceOrigin::Config));
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events = spec.iter().map(|(source, _)| source.clone()).collect();
    })
    .unwrap();
    let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let timers = crate::cli::timer_source::TimerSet::new(
        &spec,
        &events,
        None,
        EmissionSink::for_owner(tx.clone(), author.clone().into()),
    )
    .unwrap();
    let deadline = timers.next_deadline();
    // Lifecycle stand-ins for the two ordinary drivers: no real GitHub
    // request is appropriate in this regression test. Their guards prove
    // that even cancellation after an await would be observed.
    struct Ended(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Ended {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let ended = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let (started, ready) = tokio::sync::oneshot::channel();
        let held = ended.clone();
        tasks.push(tokio::spawn(async move {
            let _guard = Ended(held);
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready.await.unwrap();
    }
    let ids: Vec<_> = tasks.iter().map(|task| task.id()).collect();
    let tasks: std::collections::BTreeMap<String, _> = tasks
        .into_iter()
        .enumerate()
        .map(|(i, handle)| {
            (
                format!("source-{i}"),
                HostedTask {
                    instance: 0,
                    handle,
                    stop: None,
                },
            )
        })
        .collect();
    let command_key = match &spec[0].0 {
        WaitEventSource::Command(source) => source.source_key(),
        _ => unreachable!(),
    };
    let github_key = match &spec[1].0 {
        WaitEventSource::Github(source) => crate::cli::github::supervisor::source_key(source),
        _ => unreachable!(),
    };
    for (key, transport, item) in [
        (
            &command_key,
            Transport::Command,
            serde_json::json!({"kind":"command_event", "name":"accepted-command", "id":"one", "event":"work", "content":"still owed"}),
        ),
        (
            &github_key,
            Transport::Poll,
            serde_json::json!({"kind":"github_event", "repo":"o/r", "event":"pr_opened", "title":"new PR"}),
        ),
    ] {
        let item: WaitItem = serde_json::from_value(item).unwrap();
        EventLog::open(&events, key)
            .unwrap()
            .append_inbox(transport, Some("one"), None, None, &item)
            .unwrap();
    }
    let mut hosts = std::collections::BTreeMap::from([(
        author.clone().into(),
        Hosted {
            spec: spec.clone(),
            tasks,
            timers: Some(timers),
        },
    )]);
    for bad in [
        serde_json::json!([{"kind":"timer", "name":"broken", "due_at":"invalid"}]),
        serde_json::json!([
            {"kind":"timer", "name":"duplicate", "due_at":"2040-01-01T00:00:00Z"},
            {"kind":"timer", "name":"duplicate", "due_at":"2040-01-02T00:00:00Z"}
        ]),
    ] {
        let mut attempted = vec![
            serde_json::json!({"kind":"command", "name":"unaccepted-command", "command":["true"]}),
            serde_json::json!({"kind":"github", "repo":"different/repo", "events":["pr_opened"], "prompt":"unaccepted instructions"}),
        ];
        attempted.extend(bad.as_array().unwrap().iter().cloned());
        std::fs::write(
            crate::agent_store::agents_root(repo.path()).join("codex/config.json"),
            serde_json::to_vec(&serde_json::json!({"wait_events":attempted})).unwrap(),
        )
        .unwrap();
        assert!(
            hostable(repo.path(), &author).is_err(),
            "invalid edit is distinct from clearing sources"
        );
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        assert_eq!(hosts[&Producer::Agent(author.clone())].spec, spec);
        assert_eq!(
            hosts[&Producer::Agent(author.clone())]
                .tasks
                .values()
                .map(|task| task.handle.id())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            hosts[&Producer::Agent(author.clone())]
                .timers
                .as_ref()
                .unwrap()
                .next_deadline(),
            deadline
        );
        assert_eq!(ended.load(std::sync::atomic::Ordering::SeqCst), 0);

        let (out, mut received) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let client = Client {
            writer: None,
            who: Served::Agent {
                author: author.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        };
        let mut clients = Vec::new();
        let state = crate::repo_state::RepoState::empty(repo.path().to_owned());
        let mut sources = Sources::hosting(std::mem::take(&mut hosts));
        assert!(
            admit_into(
                repo.path(),
                &mut clients,
                client,
                Some(&state),
                &mut sources,
            )
            .is_empty()
        );
        hosts = sources.into_hosted();
        let note: Note = serde_json::from_str(&received.recv().await.unwrap()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("retained drivers' rows must stay deliverable")
        };
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(
            |item| matches!(item, WaitItem::CommandEvent { name, .. } if name == "accepted-command")
        ));
        assert!(items.iter().any(|item| matches!(item, WaitItem::GithubEvent { instructions: Some(prompt), .. } if prompt == "accepted instructions")));
    }
    // A genuinely empty, valid replacement still revokes every driver.
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events.clear()
    })
    .unwrap();
    let departing = reconcile_hosting(repo.path(), &mut hosts, &tx);
    assert!(hosts.is_empty());
    assert_eq!(departing.len(), 2);
    for task in departing {
        let _ = task.await;
    }
    assert_eq!(ended.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timer_host_is_passive_unchanged_folds_preserve_deadline_and_removal_cancels() {
    let repo = repo_with_roster();
    with_timer(
        repo.path(),
        "future",
        crate::cli::timer_source::wall_now() + 60,
    );
    let author = AgentLabel::parse("codex").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosts = Default::default();
    assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
    let deadline = hosts[&Producer::Agent(author.clone())]
        .timers
        .as_ref()
        .unwrap()
        .next_deadline();
    assert!(deadline.is_some());
    assert!(
        hosts[&Producer::Agent(author.clone())].tasks.is_empty(),
        "native timer starts no tasks or producers"
    );
    for _ in 0..10 {
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        assert_eq!(
            hosts[&Producer::Agent(author.clone())]
                .timers
                .as_ref()
                .unwrap()
                .next_deadline(),
            deadline
        );
    }
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events.clear()
    })
    .unwrap();
    assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
    assert!(hosts.is_empty());
    assert!(rx.try_recv().is_err());
    assert!(
        !crate::agent_store::agents_root(repo.path())
            .join("codex/events")
            .exists(),
        "future hosting acquires no idle ingest lease"
    );
}

#[tokio::test]
async fn timer_removed_definition_is_owed_to_two_admissions_and_ack_retires_it() {
    let repo = repo_with_roster();
    let author = AgentLabel::parse("codex").unwrap();
    with_timer(repo.path(), "past", 1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosts: std::collections::BTreeMap<Producer, Hosted> = Default::default();
    assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
    hosts
        .get_mut(&Producer::Agent(author.clone()))
        .unwrap()
        .timers
        .as_mut()
        .unwrap()
        .due();
    assert!(matches!(
        rx.recv().await.unwrap().1,
        (
            WaitItem::TimerEvent { .. },
            crate::cli::event_source::Durability::Logged
        )
    ));
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events.clear()
    })
    .unwrap();
    assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
    assert!(hosts.is_empty());
    let fresh = |out| Client {
        writer: None,
        who: Served::Agent {
            author: author.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    };
    let (one, mut first) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (two, mut second) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![fresh(one), fresh(two)];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let mut sources = Sources::hosting(hosts);
    assert!(
        present_to(
            repo.path(),
            &mut clients,
            &state,
            None,
            &mut sources,
            &mut Reading::cached(),
        )
        .is_empty()
    );
    let mut selectors = Vec::new();
    for receiver in [&mut first, &mut second] {
        let note: Note = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("accepted timer is owed")
        };
        assert_eq!(items.len(), 1);
        selectors.push(items[0].event_view().unwrap().selector.unwrap().to_owned());
    }
    assert_eq!(
        selectors[0], selectors[1],
        "one WAL occurrence, independent presentations"
    );
    assert!(clients.is_empty());
    crate::cli::events::run(crate::cli::EventsArgs {
        command: crate::cli::EventsCmd::Ack(crate::cli::EventsAckArgs {
            repo: Some(repo.path().to_owned()),
            author: Some("codex".into()),
            ids: vec![selectors.remove(0)],
            reply: None,
        }),
    })
    .await
    .unwrap();
    let (out, mut later) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    clients.push(fresh(out));
    assert!(
        present_to(
            repo.path(),
            &mut clients,
            &state,
            None,
            &mut sources,
            &mut Reading::cached(),
        )
        .is_empty()
    );
    let note: Note = serde_json::from_str(&later.recv().await.unwrap()).unwrap();
    assert!(
        matches!(note, Note::Beat),
        "cached inventory still validates durable ack state"
    );
}

/// The account says, for every hosted source, the configuration the
/// monitor accepted and the lifecycle its producer last reported, and
/// it is published outside the repo: a producer starting or ending is
/// no repo write, so it wakes no wait and no gate.
#[tokio::test]
async fn the_sources_account_is_what_runs_and_lives_outside_the_repo() {
    use crate::cli::event_source::{Lifecycle, SourceOrigin};
    let repo = repo_with_roster();
    let codex = AgentLabel::parse("codex").unwrap();
    let config = |name: &str| {
        clank_core::agent_config::WaitEventSource::Command(
            clank_core::agent_config::CommandSource {
                interrupt: false,
                interrupt_message: None,
                name: name.into(),
                command: vec!["true".into()],
                reply_command: None,
                receipt_command: None,
                glyph: None,
                stop_grace: None,
            },
        )
    };
    let key =
        |name: &str| crate::cli::source_status::ConfiguredSource::of(&codex, &config(name)).key;
    let one = key("one");
    let task = |instance| HostedTask {
        instance,
        handle: tokio::spawn(std::future::pending()),
        stop: None,
    };
    let hosted = std::collections::BTreeMap::from([(
        codex.clone().into(),
        Hosted {
            spec: vec![
                (config("one"), SourceOrigin::Config),
                (config("two"), SourceOrigin::Config),
            ],
            tasks: [(one.clone(), task(5)), (key("two"), task(6))].into(),
            timers: None,
        },
    )]);
    // `two`'s report is from the task its current one replaced.
    let lifecycles = std::collections::BTreeMap::from([
        (
            (codex.clone().into(), one.clone()),
            (5, Lifecycle::Running { pid: 9, since: 0 }),
        ),
        (
            (codex.clone().into(), key("two")),
            (4, Lifecycle::Running { pid: 8, since: 0 }),
        ),
    ]);
    let birth = crate::proc_identity::token_for(std::process::id() as i32).unwrap();
    let account = sources_account(&hosted, &lifecycles, birth);
    assert_eq!(account.monitor, std::process::id() as i32);
    assert_eq!(account.sources.len(), 2);
    assert_eq!(account.sources[0].key, one);
    assert_eq!(
        account.sources[0].accepted,
        crate::cli::source_status::fingerprint(&config("one"))
    );
    assert_eq!(
        account.sources[0].lifecycle,
        Some(Lifecycle::Running { pid: 9, since: 0 })
    );
    assert_eq!(
        account.sources[1].lifecycle, None,
        "a replaced task's report"
    );
    for host in hosted.into_values() {
        for handle in host.stop() {
            let _ = handle.await;
        }
    }
    let published = crate::cli::source_status::path(repo.path());
    assert!(
        !published.starts_with(repo.path()),
        "{}",
        published.display()
    );
}

/// Two sources resolving to one storage key are refused, command or
/// github, and an edit that introduces them leaves the last accepted
/// host running as it was rather than losing hold of a task.
#[tokio::test]
async fn sources_sharing_a_storage_key_are_refused_and_the_host_kept() {
    let repo = repo_with_roster();
    let codex = AgentLabel::parse("codex").unwrap();
    let dir = crate::agent_store::agents_root(repo.path()).join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    let write = |sources: serde_json::Value| {
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({ "wait_events": sources })).unwrap(),
        )
        .unwrap()
    };
    let command =
        serde_json::json!({"kind": "command", "name": "watch", "command": ["sleep", "300"]});
    let github = serde_json::json!({"kind": "github", "repo": "o/r", "events": ["pr_comment"]});
    for twice in [command.clone(), github] {
        write(serde_json::json!([twice.clone(), twice]));
        let refused = hostable(repo.path(), &codex).unwrap_err().to_string();
        assert!(refused.contains("share the storage key"), "{refused}");
    }
    write(serde_json::json!([command.clone()]));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = std::collections::BTreeMap::new();
    let mut departing: Vec<_> = reconcile_hosting(repo.path(), &mut hosted, &tx);
    let before: Vec<_> = hosted[&Producer::Agent(codex.clone())]
        .tasks
        .values()
        .map(|t| t.handle.id())
        .collect();
    write(serde_json::json!([command.clone(), command]));
    departing.extend(reconcile_hosting(repo.path(), &mut hosted, &tx));
    let after: Vec<_> = hosted[&Producer::Agent(codex.clone())]
        .tasks
        .values()
        .map(|t| t.handle.id())
        .collect();
    assert_eq!(before, after, "the accepted host runs on, holding its task");
    for host in hosted.into_values() {
        departing.extend(host.stop());
    }
    for handle in departing {
        let _ = handle.await;
    }
}

/// A source's task replaced under the same key starts with nothing
/// said: the old task's state is forgotten and its late reports are
/// refused, and what the new one reports, even that it stopped before
/// running anything, is what is published.
#[tokio::test]
async fn a_replaced_task_is_never_spoken_for_by_the_old_one() {
    use crate::cli::event_source::{Lifecycle, SourceOrigin};
    let codex = AgentLabel::parse("codex").unwrap();
    let config = clank_core::agent_config::WaitEventSource::Command(
        clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: "watch".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
            stop_grace: None,
        },
    );
    let key = crate::cli::source_status::ConfiguredSource::of(&codex, &config).key;
    let host = |instance| Hosted {
        spec: vec![(config.clone(), SourceOrigin::Config)],
        tasks: [(
            key.clone(),
            HostedTask {
                instance,
                handle: tokio::spawn(std::future::pending()),
                stop: None,
            },
        )]
        .into(),
        timers: None,
    };
    let mut hosted = std::collections::BTreeMap::from([(codex.clone().into(), host(1))]);
    let mut lifecycles = Lifecycles::default();
    let running = Lifecycle::Running { pid: 9, since: 0 };
    assert!(record_lifecycle(
        &hosted,
        &mut lifecycles,
        (codex.clone().into(), key.clone(), 1, running.clone())
    ));
    let old = hosted.insert(codex.clone().into(), host(2)).unwrap();
    forget_replaced(&hosted, &mut lifecycles);
    assert!(lifecycles.is_empty(), "the new task has said nothing");
    assert!(!record_lifecycle(
        &hosted,
        &mut lifecycles,
        (codex.clone().into(), key.clone(), 1, running)
    ));
    let stopped = Lifecycle::Stopped {
        why: "cannot take its ingest lease".into(),
    };
    assert!(record_lifecycle(
        &hosted,
        &mut lifecycles,
        (codex.clone().into(), key.clone(), 2, stopped.clone())
    ));
    let birth = crate::proc_identity::token_for(std::process::id() as i32).unwrap();
    assert_eq!(
        sources_account(&hosted, &lifecycles, birth).sources[0].lifecycle,
        Some(stopped)
    );
    for host in hosted.into_values().chain([old]) {
        for handle in host.stop() {
            let _ = handle.await;
        }
    }
}

/// A restart stops one source's whole process group, a grandchild
/// included, and starts it again, leaving the author's other source
/// running as it was; a request for a configuration the monitor no
/// longer runs, or a source it does not have, restarts nothing.
#[tokio::test]
async fn a_restart_is_one_source_whole_and_only_it() {
    let repo = repo_with_roster();
    let marks = repo.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let mark = |name: &str| marks.join(name).display().to_string();
    let dir = crate::agent_store::agents_root(repo.path()).join("codex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({"wait_events": [
                {"kind": "command", "name": "restartable", "command": [
                    "sh", "-c", "sleep 300 & child=$!; echo $$ >> \"$0.runs\"; echo $child > \"$0.gc\"; wait", mark("a")]},
                {"kind": "command", "name": "sibling", "command": [
                    "sh", "-c", "echo $$ > \"$0.pid\"; exec sleep 300", mark("b")]},
            ]}))
            .unwrap(),
        )
        .unwrap();
    let codex = AgentLabel::parse("codex").unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (life, _heard) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = std::collections::BTreeMap::new();
    let mut departing = futures_util::stream::FuturesUnordered::new();
    departing.extend(reconcile_hosting_reporting(
        repo.path(),
        &mut hosted,
        &tx,
        false,
        Some(&life),
    ));
    let read = |name: &str| std::fs::read_to_string(mark(name)).unwrap_or_default();
    let pid = |name: &str| {
        read(name)
            .lines()
            .last()
            .and_then(|l| l.trim().parse::<i32>().ok())
    };
    async fn until(what: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !what() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        what()
    }
    assert!(
        until(|| pid("a.gc").is_some() && pid("b.pid").is_some()).await,
        "both started"
    );
    let grandchild = pid("a.gc").unwrap();
    let sibling = pid("b.pid").unwrap();
    let alive = |pid: i32| crate::signal::probe_process(pid) != crate::signal::Probe::Gone;
    let config = hosted[&Producer::Agent(codex.clone())]
            .spec
            .iter()
            .map(|(c, _)| c.clone())
            .find(|c| matches!(c, clank_core::agent_config::WaitEventSource::Command(s) if s.name == "restartable"))
            .unwrap();
    let clank_core::agent_config::WaitEventSource::Command(command) = &config else {
        unreachable!()
    };
    let key = command.source_key();
    let ask = |key: &str, accepted: &str| Control::Restart {
        author: "codex".into(),
        key: key.into(),
        accepted: accepted.into(),
    };
    for refused in [
        ask(&key, "an earlier config"),
        ask("command-nothing-0", "x"),
    ] {
        assert!(
            restart_source(repo.path(), &mut hosted, &tx, &life, &refused)
                .await
                .is_err()
        );
    }
    assert!(alive(grandchild), "a refused restart touches nothing");
    let accepted = crate::cli::source_status::fingerprint(&config);
    let (_, stopped) = restart_source(repo.path(), &mut hosted, &tx, &life, &ask(&key, &accepted))
        .await
        .unwrap();
    departing.push(stopped);
    assert!(
        until(|| !alive(grandchild)).await,
        "the grandchild went with its group"
    );
    assert!(
        until(|| read("a.runs").lines().count() == 2).await,
        "and it started again"
    );
    assert!(
        hosted[&Producer::Agent(codex.clone())]
            .tasks
            .contains_key(&key)
            && hosted[&Producer::Agent(codex.clone())].tasks.len() == 2,
        "and the monitor holds its new task, to stop or restart again"
    );
    assert!(
        alive(sibling) && pid("b.pid") == Some(sibling),
        "the sibling ran on"
    );
    for host in std::mem::take(&mut hosted).into_values() {
        departing.extend(host.stop());
    }
    while futures_util::StreamExt::next(&mut departing)
        .await
        .is_some()
    {}
    assert!(until(|| !alive(sibling)).await);
}

#[tokio::test]
async fn hosted_producers_deliver_with_their_author_and_release_their_leases() {
    use crate::cli::event_source::Durability;
    use crate::cli::github::log::IngestLease;

    let repo = repo_with_roster();
    for who in ["claude", "codex"] {
        with_source(repo.path(), who, "builds");
        let config = crate::agent_store::agents_root(repo.path())
            .join(who)
            .join("config.json");
        std::fs::write(config, serde_json::to_vec(&serde_json::json!({
                "wait_events": [{"kind": "command", "name": "builds", "command": [
                    "sh", "-c", format!(r#"printf '{{"id":"{who}","content":"{who}"}}\n'; sleep 300"#)
                ]}]
            })).unwrap()).unwrap();
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = std::collections::BTreeMap::new();
    assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
    assert_eq!(hosted.len(), 2);
    assert!(hosted.values().all(|host| host.tasks.len() == 1));

    let mut heard = tokio::time::timeout(Duration::from_secs(10), async {
        let mut heard = Vec::new();
        for _ in 0..2 {
            let (author, (item, durability)) = rx.recv().await.unwrap();
            assert_eq!(durability, Durability::Logged);
            let WaitItem::CommandEvent {
                content, failure, ..
            } = item
            else {
                panic!("expected the hosted command's event");
            };
            assert!(!failure);
            assert_eq!(content, author.producer.to_string());
            heard.push(author);
        }
        heard
    })
    .await
    .expect("both hosted producers must deliver into the monitor queue");
    heard.sort();
    assert_eq!(
        heard
            .iter()
            .map(|stream| stream.producer.to_string())
            .collect::<Vec<_>>(),
        vec!["claude", "codex"]
    );
    for author in &heard {
        let label = author.recipient(repo.path()).unwrap();
        let spec = &hosted[&Producer::Agent(label)].spec;
        let clank_core::agent_config::WaitEventSource::Command(command) = &spec[0].0 else {
            unreachable!()
        };
        let events = author.producer.directory(repo.path());
        assert!(
            IngestLease::try_acquire(&events, &command.source_key())
                .unwrap()
                .is_none()
        );
    }
    let mut stopped = Vec::new();
    for (_, host) in hosted {
        stopped.extend(host.stop());
    }
    for task in stopped {
        let _ = task.await;
    }
    for author in &heard {
        let sources = owned_sources(repo.path(), &author.producer).unwrap();
        let clank_core::agent_config::WaitEventSource::Command(command) = &sources[0].0 else {
            unreachable!()
        };
        let events = author.producer.directory(repo.path());
        assert!(
            IngestLease::try_acquire(&events, &command.source_key())
                .unwrap()
                .is_some()
        );
    }
}

/// Everything the monitor hosts is an author's CONFIG source.
///
/// The assumption that makes passing no invocation scope safe: a
/// pooled host has none to give, and `EventSources::spawn` refuses
/// an invocation's source without one. This is the other half —
/// that such a source can never arrive here, because `hostable`
/// resolves with no CLI items at all.
#[test]
fn nothing_the_monitor_hosts_belongs_to_an_invocation() {
    use crate::cli::event_source::SourceOrigin;
    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let codex = AgentLabel::parse("codex").unwrap();

    let hosted = hostable(repo.path(), &codex).unwrap();
    assert!(!hosted.is_empty(), "the fixture declares one");
    assert!(
        hosted
            .iter()
            .all(|(_, origin)| matches!(origin, SourceOrigin::Config)),
        "an invocation's source cannot reach a pooled host: {:?}",
        hosted.iter().map(|(_, o)| *o).collect::<Vec<_>>()
    );
}

/// Hosting follows the ROSTER, not whoever happens to be connected.
///
/// Tying a producer to an attached client is tying it to a delivery
/// client again, and the churn that causes — the user's command
/// respawned on every wake, because the lease holder was a parked
/// wait that returned — is the whole reason for this plan.
#[tokio::test]
async fn hosting_follows_the_roster_and_reconciles_on_a_config_change() {
    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = std::collections::BTreeMap::new();

    // No client has connected, and the producer runs anyway.
    let _ = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert_eq!(
        hosted.keys().map(|a| a.to_string()).collect::<Vec<_>>(),
        vec!["codex".to_string()],
        "an author with no configured source hosts nothing"
    );
    let codex = AgentLabel::parse("codex").unwrap();
    let first: Vec<_> = hosted[&Producer::Agent(codex.clone())]
        .tasks
        .values()
        .map(|t| t.handle.id())
        .collect();
    assert_eq!(first.len(), 1, "one source driver, no author forwarder");

    // An unrelated fold leaves a healthy producer alone.
    let _ = reconcile_hosting(repo.path(), &mut hosted, &tx);
    let again: Vec<_> = hosted[&Producer::Agent(codex.clone())]
        .tasks
        .values()
        .map(|t| t.handle.id())
        .collect();
    assert_eq!(first, again, "an unchanged spec must not restart anything");

    // An edited `wait_events` takes effect without a restart: the
    // renamed source is a different one, so the old stops and the new
    // starts.
    with_source(repo.path(), "codex", "builds");
    let departing = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert_eq!(departing.len(), 1, "the source the edit removed is stopped");
    let after: Vec<_> = hosted[&Producer::Agent(codex.clone())]
        .tasks
        .values()
        .map(|t| t.handle.id())
        .collect();
    assert_eq!(after.len(), 1);
    assert_ne!(first, after, "the source the edit added is started");

    // And an author off the roster keeps nothing running.
    std::fs::write(
        repo.path().join(".clank/config.json"),
        r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
    )
    .unwrap();
    let _ = reconcile_hosting(repo.path(), &mut hosted, &tx);
    assert!(
        hosted.is_empty(),
        "a revoked agent's producer can serve nobody"
    );
}

/// Stand-in source tasks for driving the reconcile itself: no
/// process, only a task per source that runs until it is asked to
/// stop, and notes by its key that it was.
fn stand_in_spawner(
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> impl Fn(
    &clank_core::agent_config::WaitEventSource,
    crate::cli::event_source::SourceOrigin,
    &std::path::Path,
    crate::cli::event_source::EmissionSink,
) -> Option<crate::cli::wait::SourceTask> {
    move |source, origin, _, _| {
        let key = crate::cli::wait::task_key(source, origin, "")?;
        let (stop, mut requested) = tokio::sync::watch::channel(false);
        let asked = asked.clone();
        let noted = key.clone();
        let handle = tokio::spawn(async move {
            if requested.wait_for(|stop| *stop).await.is_ok() {
                asked.lock().unwrap().push(noted);
            }
        });
        Some(crate::cli::wait::SourceTask {
            key,
            handle,
            stop: Some(stop),
        })
    }
}

fn write_sources(repo: &std::path::Path, author: &str, sources: serde_json::Value) {
    let dir = crate::agent_store::agents_root(repo).join(author);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_vec(&serde_json::json!({ "wait_events": sources })).unwrap(),
    )
    .unwrap();
}

fn instances(
    hosted: &std::collections::BTreeMap<Producer, Hosted>,
    author: &AgentLabel,
) -> std::collections::BTreeMap<String, u64> {
    hosted[&Producer::Agent(author.clone())]
        .tasks
        .iter()
        .map(|(key, task)| (key.clone(), task.instance))
        .collect()
}

#[tokio::test]
async fn repository_producers_and_timer_deadlines_survive_master_handover() {
    use crate::cli::teams_config::RosterRole;
    use crate::cli::watch_store::{self, Target};
    let repo = repo_with_roster();
    let command = serde_json::from_value(serde_json::json!({
        "kind":"command", "name":"build", "command":["producer"]
    }))
    .unwrap();
    let github = serde_json::from_value(serde_json::json!({
        "kind":"github", "repo":"owner/repo", "events":["pr_opened"]
    }))
    .unwrap();
    let timer = serde_json::from_value(serde_json::json!({
        "kind":"timer", "name":"deadline", "due_at":"2099-01-01T00:00:00Z"
    }))
    .unwrap();
    let command_id = watch_store::add(repo.path(), Target::Master, command).unwrap();
    let github_id = watch_store::add(repo.path(), Target::Master, github).unwrap();
    let timer_id = watch_store::add(repo.path(), Target::Master, timer).unwrap();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let spawn = stand_in_spawner(asked.clone());
    let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
    let starts = Starts {
        into: &into,
        lifecycle: None,
        spawn: &spawn,
    };
    let mut hosts = std::collections::BTreeMap::new();
    assert!(reconcile_hosting_with(repo.path(), &mut hosts, false, &starts).is_empty());
    let tasks = |hosts: &std::collections::BTreeMap<Producer, Hosted>| {
        [command_id, github_id].map(|id| {
            hosts[&Producer::repo_watch(id)]
                .tasks
                .values()
                .next()
                .unwrap()
                .handle
                .id()
        })
    };
    let original = tasks(&hosts);
    let deadline = hosts[&Producer::repo_watch(timer_id)]
        .timers
        .as_ref()
        .unwrap()
        .next_deadline();
    assert!(deadline.is_some());
    for master in [None, Some("codex"), Some("claude")] {
        crate::agent_store::repo_config::transact(repo.path(), |config| {
            for (name, agent) in &mut config.agents {
                agent.role = if Some(name.as_str()) == master {
                    RosterRole::Master
                } else {
                    RosterRole::Standby
                };
            }
            Ok(())
        })
        .unwrap();
        assert!(reconcile_hosting_with(repo.path(), &mut hosts, false, &starts).is_empty());
        assert_eq!(tasks(&hosts), original);
        assert_eq!(
            hosts[&Producer::repo_watch(timer_id)]
                .timers
                .as_ref()
                .unwrap()
                .next_deadline(),
            deadline
        );
        assert_eq!(
            Producer::repo_watch(command_id)
                .recipient(&crate::cli::watch_store::read(repo.path()).unwrap())
                .as_ref()
                .map(AgentLabel::as_str),
            master
        );
    }
    assert!(asked.lock().unwrap().is_empty());
    for (_, host) in hosts {
        for task in host.stop() {
            task.await.unwrap();
        }
    }
    assert_eq!(asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn repository_native_producer_keeps_its_process_and_pending_emission() {
    use crate::cli::teams_config::RosterRole;
    use crate::cli::watch_store::{self, Target};
    let repo = repo_with_roster();
    let source = serde_json::from_value(serde_json::json!({
            "kind":"command", "name":"handover", "command":["sh","-c",
                "printf '%s\\n' '{\"id\":\"ready\",\"content\":\"pending through handover\"}'; exec sleep 300"]
        })).unwrap();
    let id = watch_store::add(repo.path(), Target::Master, source).unwrap();
    let (into, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (lifecycle, mut reported) = tokio::sync::mpsc::unbounded_channel();
    let mut hosted = Default::default();
    assert!(
        reconcile_hosting_reporting(repo.path(), &mut hosted, &into, false, Some(&lifecycle))
            .is_empty()
    );
    let (owner, (_, durability)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(owner.producer, Producer::repo_watch(id));
    let owner = owner.producer;
    assert_eq!(durability, crate::cli::event_source::Durability::Logged);
    let pid = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some((_, _, _, crate::cli::event_source::Lifecycle::Running { pid, .. })) =
                reported.recv().await
            {
                break pid;
            }
        }
    })
    .await
    .unwrap();
    let process = crate::proc_identity::token_for(pid as i32).unwrap();
    let task = hosted[&owner].tasks.values().next().unwrap().handle.id();
    crate::agent_store::repo_config::transact(repo.path(), |config| {
        config
            .agents
            .get_mut(&AgentLabel::parse("claude").unwrap())
            .unwrap()
            .role = RosterRole::Standby;
        config
            .agents
            .get_mut(&AgentLabel::parse("codex").unwrap())
            .unwrap()
            .role = RosterRole::Master;
        Ok(())
    })
    .unwrap();
    assert!(
        reconcile_hosting_reporting(repo.path(), &mut hosted, &into, false, Some(&lifecycle))
            .is_empty()
    );
    assert_eq!(
        hosted[&owner].tasks.values().next().unwrap().handle.id(),
        task
    );
    assert_eq!(crate::proc_identity::token_for(pid as i32), Some(process));
    assert!(
        watch_store::owed(
            repo.path(),
            &AgentLabel::parse("claude").unwrap(),
            &mut Default::default(),
            &crate::cli::github::log::Sidecars::waiting(),
        )
        .is_empty()
    );
    assert_eq!(
        watch_store::owed(
            repo.path(),
            &AgentLabel::parse("codex").unwrap(),
            &mut Default::default(),
            &crate::cli::github::log::Sidecars::waiting(),
        )
        .len(),
        1
    );
    for (_, host) in hosted {
        for task in host.stop() {
            task.await.unwrap();
        }
    }
}

/// What is hosted is a source, not an author's set of sources: a
/// reconcile leaves every source whose definition is unchanged
/// running as the same instance — through a timer added or removed,
/// a reorder, an edit to a sibling, or to another author — and stops
/// and restarts only the source an edit changed, under the same
/// storage key, so its log, checkpoint and lease carry over. A timer
/// edit leaves an unchanged timer's deadline where it was, and a
/// replaced task's late report is not taken for its successor's.
#[tokio::test]
async fn a_source_restarts_only_when_it_changes() {
    let repo = repo_with_roster();
    let (codex, claude) = (
        AgentLabel::parse("codex").unwrap(),
        AgentLabel::parse("claude").unwrap(),
    );
    let due = |secs: u64| {
        time::OffsetDateTime::from_unix_timestamp(
            (crate::cli::timer_source::wall_now() + secs) as i64,
        )
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
    };
    let (soon, later) = (due(600), due(1200));
    let one = serde_json::json!({"kind": "command", "name": "one", "command": ["true"]});
    let two = serde_json::json!({"kind": "command", "name": "two", "command": ["true"]});
    let timer = serde_json::json!({"kind": "timer", "name": "soon", "due_at": soon});
    let added = serde_json::json!({"kind": "timer", "name": "later", "due_at": later});
    write_sources(repo.path(), "codex", serde_json::json!([one, two, timer]));
    write_sources(
        repo.path(),
        "claude",
        serde_json::json!([{"kind": "command", "name": "theirs", "command": ["true"]}]),
    );
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let spawn = stand_in_spawner(asked.clone());
    let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
    let starts = Starts {
        into: &into,
        lifecycle: None,
        spawn: &spawn,
    };
    let mut hosted = std::collections::BTreeMap::new();
    assert!(reconcile_hosting_with(repo.path(), &mut hosted, false, &starts).is_empty());
    let first = instances(&hosted, &codex);
    let theirs = instances(&hosted, &claude);
    assert_eq!(first.len(), 2);
    let deadline = hosted[&Producer::Agent(codex.clone())]
        .timers
        .as_ref()
        .unwrap()
        .next_deadline();
    assert!(deadline.is_some());
    let key = |name: &str| {
        first
            .keys()
            .find(|key| key.starts_with(&format!("command-{name}-")))
            .unwrap()
            .clone()
    };
    let reconcile = |hosted: &mut std::collections::BTreeMap<Producer, Hosted>| {
        reconcile_hosting_with(repo.path(), hosted, false, &starts)
    };

    for unrelated in [
        serde_json::json!([one, two, timer, added]),
        serde_json::json!([added, timer, two, one]),
        serde_json::json!([one, two, timer]),
    ] {
        write_sources(repo.path(), "codex", unrelated.clone());
        assert!(reconcile(&mut hosted).is_empty(), "{unrelated}");
        assert_eq!(instances(&hosted, &codex), first, "{unrelated}");
        assert_eq!(
            hosted[&Producer::Agent(codex.clone())]
                .timers
                .as_ref()
                .unwrap()
                .next_deadline(),
            deadline,
            "an unchanged timer keeps its deadline: {unrelated}"
        );
    }

    let mut edited = two.clone();
    edited["command"] = serde_json::json!(["false"]);
    write_sources(
        repo.path(),
        "codex",
        serde_json::json!([one, edited, timer]),
    );
    let departing = reconcile(&mut hosted);
    assert_eq!(departing.len(), 1);
    for task in departing {
        task.await.unwrap();
    }
    let now = instances(&hosted, &codex);
    assert_eq!(now[&key("one")], first[&key("one")], "the sibling ran on");
    assert_ne!(
        now[&key("two")],
        first[&key("two")],
        "the edited source restarted, under the key it had"
    );
    assert_eq!(*asked.lock().unwrap(), vec![key("two")]);

    let mut graced = one.clone();
    graced["stop_grace"] = serde_json::json!("30s");
    write_sources(repo.path(), "codex", serde_json::json!([graced, edited]));
    let departing = reconcile(&mut hosted);
    assert_eq!(departing.len(), 1);
    for task in departing {
        task.await.unwrap();
    }
    let after = instances(&hosted, &codex);
    assert_ne!(after[&key("one")], now[&key("one")]);
    assert_eq!(after[&key("two")], now[&key("two")], "a timer removed");
    assert!(hosted[&Producer::Agent(codex.clone())].timers.is_none());
    assert_eq!(
        instances(&hosted, &claude),
        theirs,
        "another author's edits"
    );

    let mut lifecycles = Lifecycles::default();
    let report = |instance| {
        (
            codex.clone().into(),
            key("one"),
            instance,
            crate::cli::event_source::Lifecycle::Running { pid: 9, since: 0 },
        )
    };
    assert!(!record_lifecycle(
        &hosted,
        &mut lifecycles,
        report(now[&key("one")])
    ));
    assert!(record_lifecycle(
        &hosted,
        &mut lifecycles,
        report(after[&key("one")])
    ));

    for host in std::mem::take(&mut hosted).into_values() {
        for task in host.stop() {
            task.await.unwrap();
        }
    }
}

/// An author off the roster has every source it hosted asked to
/// stop, and handed back to be observed ending, while another
/// author's run on.
#[tokio::test]
async fn a_removed_authors_sources_are_stopped() {
    let repo = repo_with_roster();
    write_sources(
        repo.path(),
        "codex",
        serde_json::json!([
            {"kind": "command", "name": "one", "command": ["true"]},
            {"kind": "command", "name": "two", "command": ["true"]},
        ]),
    );
    write_sources(
        repo.path(),
        "claude",
        serde_json::json!([{"kind": "command", "name": "theirs", "command": ["true"]}]),
    );
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let spawn = stand_in_spawner(asked.clone());
    let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
    let starts = Starts {
        into: &into,
        lifecycle: None,
        spawn: &spawn,
    };
    let mut hosted = std::collections::BTreeMap::new();
    assert!(reconcile_hosting_with(repo.path(), &mut hosted, false, &starts).is_empty());
    let codex = AgentLabel::parse("codex").unwrap();
    let claude = AgentLabel::parse("claude").unwrap();
    let mut keys: Vec<String> = instances(&hosted, &codex).into_keys().collect();
    let theirs = instances(&hosted, &claude);

    std::fs::write(
        repo.path().join(".clank/config.json"),
        r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
    )
    .unwrap();
    let departing = reconcile_hosting_with(repo.path(), &mut hosted, false, &starts);
    assert_eq!(departing.len(), 2);
    for task in departing {
        task.await.unwrap();
    }
    assert!(!hosted.contains_key(&Producer::Agent(codex.clone())));
    let mut stopped = asked.lock().unwrap().clone();
    stopped.sort();
    keys.sort();
    assert_eq!(stopped, keys);
    assert_eq!(instances(&hosted, &claude), theirs);
    for host in std::mem::take(&mut hosted).into_values() {
        for task in host.stop() {
            task.await.unwrap();
        }
    }
}
