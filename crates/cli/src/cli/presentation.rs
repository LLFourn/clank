//! Persist only successful model-delivery boundaries, independently of callbacks.
//! Selection metadata travels with work; viewing the inbox never calls this.
use clank_core::{AgentLabel, wait::EventPresentation};
use std::{collections::BTreeMap, path::Path};

pub(crate) fn record(
    repo: &Path,
    author: &AgentLabel,
    selected: &[EventPresentation],
    current: impl Fn() -> bool,
) {
    let dir = crate::agent_store::agents_root(repo)
        .join(author.as_str())
        .join("events");
    let mut groups: BTreeMap<&str, Vec<EventPresentation>> = BTreeMap::new();
    for key in selected {
        // Keys come from our wait subprocess, but never let transport metadata
        // select a path outside this author's WAL directory.
        let mut components = Path::new(&key.source).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || key.source.contains('\\')
        {
            continue;
        }
        groups.entry(&key.source).or_default().push(key.clone());
    }
    let mut failures = 0;
    for (source, keys) in groups {
        if !current() {
            break;
        }
        // Removed custody is never recreated by a late presentation.
        if !dir.join(format!("{source}.jsonl")).is_file() {
            continue;
        }
        let result = super::github::log::EventLog::open(&dir, source).and_then(|mut log| {
            log.append_presentations(&keys, super::github::log::now_epoch(), &current)
        });
        if result.is_err() {
            failures += 1;
        }
    }
    if failures > 0 {
        eprintln!(
            "clank: wake delivered; could not persist presentation evidence for {failures} source(s)"
        );
    }
}
