use super::*;
use crate::cli::monitor::fixtures::*;

/// Agent `watch:1` and watch 1 display alike and host identical
/// definitions; a restart naming the watch must reach the watch alone.
#[tokio::test]
async fn a_restart_names_the_watch_not_the_agent_that_displays_like_it() {
    let repo = tempfile::tempdir().unwrap();
    let agent = Producer::Agent(AgentLabel::parse("watch:1").unwrap());
    let watch = Producer::repo_watch(1);
    assert_eq!(agent.to_string(), watch.to_string());
    let source: clank_core::agent_config::WaitEventSource = serde_json::from_value(
        serde_json::json!({"kind":"command", "name":"build", "command":["producer"]}),
    )
    .unwrap();
    let key = crate::cli::watch_store::source_key(&source);
    let spec = vec![(
        source.clone(),
        crate::cli::event_source::SourceOrigin::Config,
    )];
    let agents_task = tokio::spawn(std::future::pending::<()>());
    let agents_task_id = agents_task.id();
    let mut hosted = std::collections::BTreeMap::from([
        (
            agent.clone(),
            Hosted {
                spec: spec.clone(),
                timers: None,
                tasks: std::collections::BTreeMap::from([(
                    key.clone(),
                    HostedTask {
                        instance: 1,
                        handle: agents_task,
                        stop: None,
                    },
                )]),
            },
        ),
        (
            watch.clone(),
            Hosted {
                spec,
                timers: None,
                tasks: Default::default(),
            },
        ),
    ]);
    let request = Control::Restart {
        author: watch,
        key: key.clone(),
        accepted: crate::cli::source_status::fingerprint(&source),
    };
    let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (lifecycle, _lrx) = tokio::sync::mpsc::unbounded_channel();
    let (departing, err) =
        match restart_source(repo.path(), &mut hosted, &into, &lifecycle, &request).await {
            Ok((_, stopped)) => (vec![stopped], String::new()),
            Err(err) => (Vec::new(), err),
        };
    assert!(err.contains("has no running task"), "{err}");
    assert!(departing.is_empty(), "nothing of the agent's was stopped");
    assert_eq!(hosted[&agent].tasks[&key].handle.id(), agents_task_id);
    hosted[&agent].tasks[&key].handle.abort();
}
