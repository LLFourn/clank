//! The shared merged-repo-view over every agent's github event inbox
//! (log-timeline-github-events): ONE read layer feeding both `clank
//! log` and the status TUI timeline, so the two surfaces cannot
//! disagree about what happened.
//!
//! Merging is CONNECTED COMPONENTS over shared stable aliases (codex
//! 8354e51), never a precedence key: each row contributes the aliases
//! it has — `(repo, feed_id)` and/or `(repo, action_key)`, both
//! repo-scoped (action keys are source-local and collide across
//! repos) — and rows sharing ANY alias join one component. A poll row
//! carrying both identities is the bridge that joins its relay twin
//! (action key only) into the same entry. KEYLESS rows never merge —
//! mirroring the coordinator's deliberate emit-always treatment of
//! missing identity; no manufactured time buckets.

// Staged layer: the consumers (clank log interleave, TUI timeline)
// land in this plan's next commits — the allow goes with them.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

use crate::cli::github_event_log::EventLog;
use clank_core::wait::WaitItem;

/// A copy's IMMUTABLE identity — the COMPLETE copy key
/// (tui-github-event-page): damaged WALs legally hold distinct
/// idents at one seq, and ident is the discriminator
/// reads/compaction/ack already use. Eq/Ord live HERE and only
/// here, so retained page targets survive mutable-state flips (an
/// ack must never read as the copy disappearing — codex fc7bf6c).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) struct MemberKey {
    pub(crate) agent: String,
    pub(crate) source: String,
    pub(crate) seq: u64,
    pub(crate) ident: String,
}

/// One copy: its key plus MUTABLE state. Deliberately NOT Eq/Ord as
/// an IDENTITY (compare and sort by [`MemberKey`]); the PartialEq
/// here only serves MergedEvent's whole-value comparison (the
/// nothing-changed repaint gate SHOULD see an ack flip as a
/// change). Transport stays typed; rendering goes through `as_str`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct MemberRef {
    #[serde(flatten)]
    pub(crate) key: MemberKey,
    pub(crate) acked: bool,
    pub(crate) transport: crate::cli::github_event_log::Transport,
    /// This copy's content reference (tui-github-event-content).
    /// Absent on rows logged before it existed, which is why the
    /// component-level resolution below cannot simply take member
    /// zero's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<clank_core::wait::ContentRef>,
}

/// Which object the PAGE should read a body from for a merged
/// component (tui-github-event-content, Contract 2).
///
/// Copies of one event should agree, and legacy rows carry nothing at
/// all, so the component takes the first reference in the members'
/// existing provenance order — the same first-some rule every other
/// merged field uses, which is already total and therefore stable
/// across refreshes. (The plan first said "newest"; that was an
/// arbitrary second ordering for no gain, and consistency here is
/// worth more.) A legacy row never clears a reference a sibling has.
///
/// If members genuinely DISAGREE the component does not speak for
/// both — it would be showing one event's body under another's
/// heading — so the caller's target row decides.
pub(crate) fn resolve_content_ref<'a>(
    members: &'a [MemberRef],
    target: &MemberKey,
) -> Option<&'a clank_core::wait::ContentRef> {
    let mut present = members.iter().filter_map(|m| m.content.as_ref());
    let first = present.next()?;
    if present.any(|c| c != first) {
        return members
            .iter()
            .find(|m| &m.key == target)
            .and_then(|m| m.content.as_ref());
    }
    Some(first)
}

/// One merged timeline entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct MergedEvent {
    /// Earliest display time across copies: each copy's github-side
    /// `event_at` when known, else its ingest time.
    pub(crate) at: u64,
    pub(crate) repo: String,
    pub(crate) event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// Agent labels holding a copy, sorted, deduped.
    pub(crate) seen_by: Vec<String>,
    /// Every copy's full address + state, sorted by
    /// (agent, source, seq, ident) — the page's target space and the
    /// ack fanout list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) members: Vec<MemberRef>,
    /// UNHANDLED while ANY copy is unhandled; handled only when every
    /// copy is acked (codex 8354e51) — the TUI's open-work marker is
    /// deterministic under mixed ack state.
    pub(crate) unhandled: bool,
    /// Pre-watch history: true only when EVERY copy is a baseline
    /// record (codex b1b1d91) — one agent's live copy keeps the entry
    /// normal, and a live unhandled copy keeps it open.
    pub(crate) baseline: bool,
}

/// The typed result of one timeline read: entries plus deduplicated
/// read notices (corrupt logs, read failures, skipped-foreign counts).
/// The layer performs NO terminal I/O — `clank log` and the TUI
/// decide how to render notices (codex 92e9deb: an eprintln here
/// would write through the TUI's raw screen).
#[derive(Debug, Default)]
pub(crate) struct TimelineSnapshot {
    pub(crate) events: Vec<MergedEvent>,
    /// Sorted + deduplicated.
    pub(crate) notices: Vec<String>,
}

/// One agent's copy of one event, pre-merge. `(agent, source, seq)`
/// is the copy's stable PROVENANCE — the total tiebreak that keeps
/// member preference and entry order deterministic when timestamps
/// tie (codex 92e9deb: HashMap/read_dir order must never show).
struct Copy {
    agent: String,
    source: String,
    seq: u64,
    ident: String,
    transport: crate::cli::github_event_log::Transport,
    display_at: u64,
    aliases: Vec<(String, String)>,
    item: WaitItem,
    acked: bool,
    baseline: bool,
}

impl Copy {
    fn provenance(&self) -> (u64, &str, &str, u64) {
        (self.display_at, &self.agent, &self.source, self.seq)
    }
}

/// Read every agent's event logs under `.clank/agents/*/events/` and
/// merge. Corrupt logs are skipped with one stderr notice each —
/// a viewer degrades, never quarantines (the ingest side owns that).
pub(crate) fn timeline_snapshot(repo: &Path) -> TimelineSnapshot {
    let mut copies: Vec<Copy> = Vec::new();
    let mut notices: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let agents_root = crate::agent_store::agents_root(repo);
    let Ok(agents) = std::fs::read_dir(&agents_root) else {
        return TimelineSnapshot::default();
    };
    for agent in agents.flatten() {
        let label = agent.file_name().to_string_lossy().into_owned();
        let events_dir = agent.path().join("events");
        let Ok(files) = std::fs::read_dir(&events_dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let Ok(log) = EventLog::open(&events_dir, stem) else {
                continue;
            };
            match log.read_rows() {
                Ok(Some(read)) => {
                    if read.foreign > 0 {
                        notices.insert(format!(
                            "{label}/{name}: {} record(s) from another clank version skipped",
                            read.foreign
                        ));
                    }
                    // Non-github items can't merge and would render as
                    // blank entries — filter, don't blank (codex
                    // 92e9deb). (The github wait path only logs github
                    // events today, so this is belt.)
                    copies.extend(read.rows.into_iter().filter_map(|r| {
                        let WaitItem::GithubEvent {
                            repo: event_repo, ..
                        } = &r.item
                        else {
                            return None;
                        };
                        let event_repo = event_repo.clone();
                        let mut aliases = Vec::new();
                        if let Some(id) = &r.feed_id {
                            aliases.push((event_repo.clone(), format!("f:{id}")));
                        }
                        if let Some(k) = &r.action_key {
                            aliases.push((event_repo.clone(), format!("k:{k}")));
                        }
                        Some(Copy {
                            agent: label.clone(),
                            source: stem.to_string(),
                            seq: r.seq,
                            ident: r.ident,
                            transport: r.transport,
                            display_at: r.event_at.unwrap_or(r.at),
                            aliases,
                            item: r.item,
                            acked: r.acked,
                            baseline: r.baseline,
                        })
                    }));
                }
                Ok(None) => {
                    notices.insert(format!("{label}/{name}: corrupt — skipped"));
                }
                Err(e) => {
                    notices.insert(format!("{label}/{name}: read failed ({e}) — skipped"));
                }
            }
        }
    }
    TimelineSnapshot {
        events: merge(copies),
        notices: notices.into_iter().collect(),
    }
}

/// Union-find over copies sharing any alias; keyless copies stay
/// singletons.
fn merge(copies: Vec<Copy>) -> Vec<MergedEvent> {
    let n = copies.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut i = i;
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let mut by_alias: HashMap<(String, String), usize> = HashMap::new();
    for (i, c) in copies.iter().enumerate() {
        for a in &c.aliases {
            match by_alias.entry(a.clone()) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(i);
                }
                std::collections::hash_map::Entry::Occupied(o) => {
                    let (ra, rb) = (find(&mut parent, *o.get()), find(&mut parent, i));
                    if ra != rb {
                        parent[rb] = ra;
                    }
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    // Provenance-first determinism (codex 92e9deb): members order by
    // (display_at, agent, source, seq) — a TOTAL key — so field-wise
    // first-some never depends on read_dir order; entries then sort by
    // (at, minimum member provenance), total again, so tied timestamps
    // can't shuffle across invocations or TUI refreshes.
    let mut out: Vec<(Vec<usize>, MergedEvent)> = groups
        .into_values()
        .map(|mut members| {
            members.sort_by(|&a, &b| copies[a].provenance().cmp(&copies[b].provenance()));
            let first = |get: &dyn Fn(&WaitItem) -> Option<String>| {
                members.iter().find_map(|&i| get(&copies[i].item))
            };
            let gh = |item: &WaitItem| match item {
                WaitItem::GithubEvent {
                    repo,
                    event,
                    detail,
                    number,
                    title,
                    actor,
                    url,
                    // Presentation-time stamp; WAL rows never carry it.
                    instructions: _,
                    content: _,
                } => Some((
                    repo.clone(),
                    event.clone(),
                    detail.clone(),
                    *number,
                    title.clone(),
                    actor.clone(),
                    url.clone(),
                )),
                _ => None,
            };
            let (repo, event, detail, number, ..) =
                gh(&copies[members[0]].item).unwrap_or_default();
            let mut seen_by: Vec<String> =
                members.iter().map(|&i| copies[i].agent.clone()).collect();
            seen_by.sort();
            seen_by.dedup();
            let mut member_refs: Vec<MemberRef> = members
                .iter()
                .map(|&i| MemberRef {
                    key: MemberKey {
                        agent: copies[i].agent.clone(),
                        source: copies[i].source.clone(),
                        seq: copies[i].seq,
                        ident: copies[i].ident.clone(),
                    },
                    acked: copies[i].acked,
                    transport: copies[i].transport,
                    content: match &copies[i].item {
                        WaitItem::GithubEvent { content, .. } => content.clone(),
                        _ => None,
                    },
                })
                .collect();
            member_refs.sort_by(|a, b| a.key.cmp(&b.key));
            let merged = MergedEvent {
                at: copies[members[0]].display_at,
                repo,
                event,
                detail,
                number,
                title: first(&|i| gh(i).and_then(|g| g.4)),
                actor: first(&|i| gh(i).and_then(|g| g.5)),
                url: first(&|i| gh(i).and_then(|g| g.6)),
                seen_by,
                members: member_refs,
                unhandled: members.iter().any(|&i| !copies[i].acked),
                baseline: members.iter().all(|&i| copies[i].baseline),
            };
            (members, merged)
        })
        .collect();
    out.sort_by(|(ma, a), (mb, b)| {
        (a.at, copies[ma[0]].provenance()).cmp(&(b.at, copies[mb[0]].provenance()))
    });
    out.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {

    use clank_core::wait::ContentRef;

    fn member_with(agent: &str, content: Option<ContentRef>) -> MemberRef {
        MemberRef {
            key: MemberKey {
                agent: agent.into(),
                source: "github-o-r-aaaa".into(),
                seq: 1,
                ident: String::new(),
            },
            acked: false,
            transport: crate::cli::github_event_log::Transport::Poll,
            content,
        }
    }

    #[test]
    fn a_legacy_row_never_clears_a_sibling_reference() {
        // The mix EVERY existing repo hits on the first run after the
        // reference ships: copies logged before it sit beside copies
        // logged after.
        let want = ContentRef::ReviewComment { id: 900 };
        let members = vec![
            member_with("codex", None),
            member_with("claude", Some(want.clone())),
        ];
        let target = members[0].key.clone();
        assert_eq!(
            resolve_content_ref(&members, &target),
            Some(&want),
            "a row with no reference must not out-vote one that has it"
        );
        // Order must not matter: the reference is a property of the
        // event, not of which agent happened to log first.
        let flipped = vec![members[1].clone(), members[0].clone()];
        assert_eq!(resolve_content_ref(&flipped, &target), Some(&want));
    }

    #[test]
    fn no_reference_anywhere_resolves_to_none() {
        let members = vec![member_with("codex", None), member_with("claude", None)];
        let target = members[0].key.clone();
        assert_eq!(resolve_content_ref(&members, &target), None);
    }

    #[test]
    fn conflicting_references_defer_to_the_targeted_row() {
        // A component whose copies disagree does not speak for both:
        // guessing would render one event's body under another's
        // heading, which is precisely the confusion this page exists
        // to remove.
        let mine = ContentRef::ReviewComment { id: 900 };
        let theirs = ContentRef::ReviewComment { id: 901 };
        let members = vec![
            member_with("claude", Some(mine.clone())),
            member_with("codex", Some(theirs.clone())),
        ];
        assert_eq!(
            resolve_content_ref(&members, &members[1].key),
            Some(&theirs),
            "the targeted row decides, not the first one"
        );
        assert_eq!(resolve_content_ref(&members, &members[0].key), Some(&mine));
        // A target that is no longer present (retargeting mid-flight)
        // yields nothing rather than an arbitrary pick.
        let gone = member_with("ruthless", None).key;
        assert_eq!(resolve_content_ref(&members, &gone), None);
    }
    use super::*;
    use crate::cli::github_event_log::Transport;
    use std::path::PathBuf;

    fn item(repo: &str, n: u64, title: Option<&str>) -> WaitItem {
        WaitItem::GithubEvent {
            repo: repo.into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(n),
            title: title.map(str::to_string),
            actor: Some("alice".into()),
            url: None,
            instructions: None,
            content: None,
        }
    }

    fn tempdir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clank-gh-timeline-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn agent_log(repo: &Path, agent: &str) -> EventLog {
        let dir = repo.join(".clank/agents").join(agent).join("events");
        std::fs::create_dir_all(&dir).unwrap();
        EventLog::open(&dir, "github-o-r-aaaa").unwrap()
    }

    #[test]
    fn members_carry_full_copy_keys_across_agents_and_damaged_seqs() {
        // tui-github-event-page identity pins: two agents may reuse
        // a source name and seq — members stay distinct by agent —
        // and one damaged WAL may hold two DISTINCT idents at one
        // seq — members stay distinct by ident. Sorted, full keys.
        let repo = tempdir();
        // Agent A: feed id f-1 at seq 1.
        let mut a = agent_log(&repo, "alpha");
        a.append_inbox(
            Transport::Poll,
            Some("f-1"),
            None,
            Some(100),
            &item("o/r", 1, None),
        )
        .unwrap();
        // Agent B: SAME source stem, SAME seq, same feed id (merges
        // into one component) — distinct agent in the member set.
        let mut b = agent_log(&repo, "beta");
        b.append_inbox(
            Transport::Relay,
            Some("f-1"),
            None,
            Some(100),
            &item("o/r", 1, None),
        )
        .unwrap();

        let snap = timeline_snapshot(&repo);
        let ev = snap
            .events
            .iter()
            .find(|e| e.members.len() == 2)
            .expect("merged component");
        let keys: Vec<(&str, u64)> = ev
            .members
            .iter()
            .map(|m| (m.key.agent.as_str(), m.key.seq))
            .collect();
        assert_eq!(
            keys,
            vec![("alpha", 1), ("beta", 1)],
            "sorted, agent-scoped"
        );
        assert!(ev.members.iter().all(|m| !m.key.ident.is_empty()));
        assert_eq!(ev.members[0].transport.as_str(), "poll");
        assert_eq!(ev.members[1].transport.as_str(), "relay");

        // The DAMAGED-WAL case for real (codex fc7bf6c): duplicate
        // gamma's row bytes at the SAME seq with a different feed id
        // — one agent, one source, one seq, two DISTINCT idents; the
        // full four-field keys stay separately targetable.
        let mut c = agent_log(&repo, "gamma");
        c.append_inbox(
            Transport::Poll,
            Some("g-1"),
            None,
            Some(200),
            &item("o/r", 9, None),
        )
        .unwrap();
        let gpath = repo.join(".clank/agents/gamma/events/github-o-r-aaaa.jsonl");
        let text = std::fs::read_to_string(&gpath).unwrap();
        let dup = text
            .lines()
            .last()
            .unwrap()
            .replace("\"g-1\"", "\"g-2\"")
            .replace("\"number\":9", "\"number\":10");
        std::fs::write(&gpath, format!("{text}{dup}\n")).unwrap();

        let snap = timeline_snapshot(&repo);
        let gamma_keys: Vec<&MemberKey> = snap
            .events
            .iter()
            .flat_map(|e| &e.members)
            .filter(|m| m.key.agent == "gamma")
            .map(|m| &m.key)
            .collect();
        assert_eq!(gamma_keys.len(), 2, "both damaged-seq copies surface");
        assert_eq!(gamma_keys[0].seq, gamma_keys[1].seq, "same seq");
        assert_eq!(gamma_keys[0].source, gamma_keys[1].source);
        assert_ne!(
            gamma_keys[0].ident, gamma_keys[1].ident,
            "distinct idents keep distinct full keys"
        );
    }

    #[test]
    fn poll_row_bridges_its_relay_twin_into_one_entry() {
        let repo = tempdir();
        let mut log = agent_log(&repo, "claude");
        // Relay copy first: action key only, later ingest time.
        log.append_inbox(
            Transport::Relay,
            None,
            Some("review#9"),
            Some(2_000),
            &item("o/r", 5, None),
        )
        .unwrap();
        // Poll copy: BOTH identities — the bridge — and the earlier
        // (github-side) time plus a title.
        log.append_inbox(
            Transport::Poll,
            Some("feed-1"),
            Some("review#9"),
            Some(1_000),
            &item("o/r", 5, Some("fix the thing")),
        )
        .unwrap();
        let merged = timeline_snapshot(&repo).events;
        assert_eq!(merged.len(), 1, "bridged into one entry: {merged:?}");
        assert_eq!(merged[0].at, 1_000, "earliest time wins");
        assert_eq!(merged[0].title.as_deref(), Some("fix the thing"));
        assert!(merged[0].unhandled);
    }

    #[test]
    fn same_event_across_agents_merges_with_all_acked_lifecycle() {
        let repo = tempdir();
        let mut a = agent_log(&repo, "alice");
        let sa = a
            .append_inbox(
                Transport::Poll,
                Some("feed-1"),
                None,
                Some(1_000),
                &item("o/r", 5, None),
            )
            .unwrap();
        let mut b = agent_log(&repo, "bob");
        let _sb = b
            .append_inbox(
                Transport::Poll,
                Some("feed-1"),
                None,
                Some(1_500),
                &item("o/r", 5, None),
            )
            .unwrap();
        let merged = timeline_snapshot(&repo).events;
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].seen_by, vec!["alice".to_string(), "bob".into()]);
        assert!(merged[0].unhandled);
        // One agent acks — still unhandled (mixed state).
        a.append_ack(sa).unwrap();
        let merged = timeline_snapshot(&repo).events;
        assert!(merged[0].unhandled, "unhandled while ANY copy is");
        // Both ack — handled.
        let sb = 1; // bob's only seq
        b.append_ack(sb).unwrap();
        let merged = timeline_snapshot(&repo).events;
        assert!(!merged[0].unhandled, "handled only when ALL copies acked");
    }

    #[test]
    fn cross_repo_action_keys_stay_separate_and_keyless_never_merge() {
        let repo = tempdir();
        let mut log = agent_log(&repo, "claude");
        // Same action key, DIFFERENT repos → two entries.
        log.append_inbox(
            Transport::Relay,
            None,
            Some("pr_opened#12"),
            Some(1_000),
            &item("o/r", 12, None),
        )
        .unwrap();
        log.append_inbox(
            Transport::Relay,
            None,
            Some("pr_opened#12"),
            Some(1_000),
            &item("o/x", 12, None),
        )
        .unwrap();
        // Two keyless rows with identical (repo, kind, number, time)
        // → still two entries.
        log.append_inbox(
            Transport::Poll,
            None,
            None,
            Some(3_000),
            &item("o/r", 9, None),
        )
        .unwrap();
        log.append_inbox(
            Transport::Poll,
            None,
            None,
            Some(3_000),
            &item("o/r", 9, None),
        )
        .unwrap();
        let merged = timeline_snapshot(&repo).events;
        assert_eq!(merged.len(), 4, "{merged:?}");
    }

    #[test]
    fn tied_timestamps_order_deterministically_by_provenance() {
        // codex 92e9deb: distinct events commonly tie on
        // (at, repo, number); the order must be total and stable
        // across invocations, never HashMap/read_dir noise.
        let repo = tempdir();
        let mut a = agent_log(&repo, "alice");
        for key in ["k#b", "k#a", "k#c"] {
            a.append_inbox(
                Transport::Relay,
                None,
                Some(key),
                Some(5_000),
                &item("o/r", 1, None),
            )
            .unwrap();
        }
        let first = timeline_snapshot(&repo).events;
        assert_eq!(first.len(), 3);
        for _ in 0..10 {
            assert_eq!(timeline_snapshot(&repo).events, first, "order shuffled");
        }
        // The expected total order: same at → provenance (agent,
        // source, seq) — i.e. append order here.
        let seqs_stable = first.iter().map(|e| e.at).all(|t| t == 5_000);
        assert!(seqs_stable);
    }

    #[test]
    fn notices_are_data_not_terminal_io() {
        let repo = tempdir();
        let mut a = agent_log(&repo, "alice");
        a.append_inbox(
            Transport::Poll,
            Some("1"),
            None,
            Some(1_000),
            &item("o/r", 1, None),
        )
        .unwrap();
        let dir = repo.join(".clank/agents/alice/events");
        // A corrupt sibling log and a foreign record in a third.
        std::fs::write(dir.join("github-x-y-bbbb.jsonl"), "garbage\n{}\n").unwrap();
        std::fs::write(
            dir.join("github-z-w-cccc.jsonl"),
            "{\"t\":\"future\",\"v\":9,\"seq\":1}\n",
        )
        .unwrap();
        let snap = timeline_snapshot(&repo);
        assert_eq!(snap.events.len(), 1, "healthy log still contributes");
        assert!(
            snap.notices
                .iter()
                .any(|n| n.contains("bbbb") && n.contains("corrupt")),
            "{:?}",
            snap.notices
        );
        assert!(
            snap.notices
                .iter()
                .any(|n| n.contains("cccc") && n.contains("another clank version")),
            "{:?}",
            snap.notices
        );
    }

    #[test]
    fn baseline_aggregation_requires_every_copy() {
        // codex b1b1d91: one agent's cold arm baselines an event
        // another agent received LIVE — the entry must render normal
        // (and stay OPEN while the live copy is unhandled), never
        // dimmed as pre-watch history. All-baseline components dim.
        let repo = tempdir();
        let mut a = agent_log(&repo, "alice");
        a.append_baseline_inbox(Some("feed-1"), None, Some(1_000), &item("o/r", 5, None))
            .unwrap();
        let mut b = agent_log(&repo, "bob");
        let live = b
            .append_inbox(
                Transport::Poll,
                Some("feed-1"),
                None,
                Some(1_000),
                &item("o/r", 5, None),
            )
            .unwrap();
        let e = &timeline_snapshot(&repo).events[0];
        assert!(!e.baseline, "a live copy keeps the entry normal");
        assert!(e.unhandled, "…and unhandled live keeps it open");
        b.append_ack(live).unwrap();
        let e = &timeline_snapshot(&repo).events[0];
        assert!(!e.baseline && !e.unhandled, "acked live: normal, closed");

        // A second, all-baseline event dims.
        a.append_baseline_inbox(Some("feed-2"), None, Some(2_000), &item("o/r", 6, None))
            .unwrap();
        let events = timeline_snapshot(&repo).events;
        let dimmed = events.iter().find(|e| e.number == Some(6)).unwrap();
        assert!(dimmed.baseline && !dimmed.unhandled);
    }

    #[test]
    fn missing_dirs_are_empty_not_errors() {
        let repo = tempdir();
        assert!(timeline_snapshot(&repo).events.is_empty());
    }
}
