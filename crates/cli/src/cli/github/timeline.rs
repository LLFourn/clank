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

use std::collections::HashMap;
use std::path::Path;

use crate::cli::github::log::EventLog;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) woken_at: Option<u64>,
    pub(crate) transport: crate::cli::github::log::Transport,
    /// This copy's content reference (tui-github-event-content).
    /// Absent on rows logged before it existed, which is why the
    /// component-level resolution below cannot simply take member
    /// zero's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<clank_core::wait::ContentRef>,
}

/// Shared lifecycle projection. Sent is source custody, not remote wake evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryState {
    Unwoken,
    Woken,
    Acknowledged,
    Sent,
    History,
}
impl DeliveryState {
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Unwoken => "?",
            Self::Woken => "✓",
            Self::Acknowledged => "✓✓",
            Self::Sent => "→",
            Self::History => "",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unwoken => "? not woken",
            Self::Woken => "✓ woken · awaiting acknowledgement",
            Self::Acknowledged => "✓✓ acknowledged",
            Self::Sent => "→ sent · recipient wake/ack unknown",
            Self::History => "pre-watch history",
        }
    }
}
impl MemberRef {
    pub(crate) fn delivery(&self) -> DeliveryState {
        if self.acked {
            DeliveryState::Acknowledged
        } else if self.woken_at.is_some() {
            DeliveryState::Woken
        } else {
            DeliveryState::Unwoken
        }
    }
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

/// Which backend produced an event. CARRIED, never inferred.
///
/// The first version of this read it off the carve-out — github if a
/// url, number or actor was present — and that was wrong in both
/// directions: a sparse github row with none of the three read as a
/// command event, and a command source named like a repo read as
/// github (codex 80e1fb9). Which backend produced a row is a fact
/// the row knows; guessing it from what the payload happens to
/// contain is a false model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Backend {
    Github,
    Command,
    Timer,
    /// A message one agent sent another. Its own backend, not
    /// `Command`: nobody ran it, and a row labelled "command" would
    /// name a producer that does not exist.
    Message,
}

/// Sending custody is independent of recipient delivery/acknowledgement.
/// Body and timestamp belong to the containing timeline entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct OutgoingReply {
    pub(crate) operation_id: String,
    pub(crate) delivery_id: String,
    pub(crate) from: String,
    pub(crate) from_repo: std::path::PathBuf,
    pub(crate) destination: clank_core::wait::MessageReplyRoute,
    pub(crate) original: MemberKey,
    pub(crate) original_handle: String,
    pub(crate) acknowledge: bool,
}

/// Page navigation identity, never an acknowledgement address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventKey {
    Inbox(MemberKey),
    Outgoing {
        author: String,
        operation_id: String,
    },
}
impl From<MemberKey> for EventKey {
    fn from(key: MemberKey) -> Self {
        Self::Inbox(key)
    }
}
impl EventKey {
    pub(crate) fn member(&self) -> Option<&MemberKey> {
        match self {
            Self::Inbox(key) => Some(key),
            Self::Outgoing { .. } => None,
        }
    }
}

/// One merged timeline entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(remote = "Self")]
pub(crate) struct MergedEvent {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) outgoing: Vec<OutgoingReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recurrence: Option<clank_core::agent_config::TimerRecurrence>,
    /// What KIND of backend produced it — the carve-out's presence is
    /// a payload fact, not provenance.
    pub(crate) backend: Backend,
    /// The source's configured glyph, joined from the agent's CURRENT
    /// config at presentation time — like a watch's prompt, and for
    /// the same reason: it is display config, so editing it retitles
    /// events already logged and the WAL stores no copy.
    ///
    /// Normalized for safe display when read from current config. Its width
    /// is measured by renderers and never affects admission. A producer
    /// never reaches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) glyph: Option<String>,
    /// Earliest display time across copies: each copy's github-side
    /// `event_at` when known, else its ingest time.
    pub(crate) at: u64,
    /// What produced it — a repo for github, the configured name for
    /// a command source. Named `source`, not `repo`, because it is
    /// no longer always a repo
    /// (a-command-is-an-event-source-like-any-other).
    pub(crate) source: String,
    /// The sub-kind within that source.
    pub(crate) kind: String,
    /// The text a reader shows: github's title, a command event's
    /// content, a message's subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    /// The whole text where the row shows only a summary of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
    /// This SOURCE is broken, rather than something it observed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) failure: bool,
    // ── the github carve-out: absent for every other backend ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// Local recipient labels holding an inbox copy, sorted, deduped. An
    /// outgoing sender's custody does not count as having received the reply.
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
impl serde::Serialize for MergedEvent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Projection<'a> {
            delivery: DeliveryState,
            #[serde(flatten, with = "MergedEvent")]
            event: &'a MergedEvent,
        }
        Projection {
            delivery: self.delivery(),
            event: self,
        }
        .serialize(serializer)
    }
}
impl MergedEvent {
    pub(crate) fn delivery(&self) -> DeliveryState {
        if self.baseline {
            return DeliveryState::History;
        }
        if self.outgoing_only() {
            return DeliveryState::Sent;
        }
        if self
            .members
            .iter()
            .any(|m| m.delivery() == DeliveryState::Unwoken)
        {
            return DeliveryState::Unwoken;
        }
        if self
            .members
            .iter()
            .any(|m| m.delivery() == DeliveryState::Woken)
        {
            return DeliveryState::Woken;
        }
        DeliveryState::Acknowledged
    }

    pub(crate) fn outgoing_only(&self) -> bool {
        self.members.is_empty() && !self.outgoing.is_empty()
    }
    pub(crate) fn has_page_key(&self, key: &EventKey) -> bool {
        match key {
            EventKey::Inbox(key) => self.members.iter().any(|m| &m.key == key),
            EventKey::Outgoing {
                author,
                operation_id,
            } => self
                .outgoing
                .iter()
                .any(|o| &o.from == author && &o.operation_id == operation_id),
        }
    }
    pub(crate) fn page_keys(&self) -> Vec<EventKey> {
        self.members
            .iter()
            .map(|m| m.key.clone().into())
            .chain(self.outgoing.iter().map(|o| EventKey::Outgoing {
                author: o.from.clone(),
                operation_id: o.operation_id.clone(),
            }))
            .collect()
    }
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
/// plus its discriminator is the copy's stable PROVENANCE — the total tiebreak that keeps
/// member preference and entry order deterministic when timestamps
/// tie (codex 92e9deb: HashMap/read_dir order must never show).
struct Copy {
    outgoing: Option<OutgoingReply>,
    agent: String,
    source: String,
    seq: u64,
    ident: String,
    transport: crate::cli::github::log::Transport,
    display_at: u64,
    backend: Backend,
    glyph: Option<String>,
    aliases: Vec<(String, String)>,
    item: WaitItem,
    acked: bool,
    woken_at: Option<u64>,
    baseline: bool,
}

impl Copy {
    fn provenance(&self) -> (u64, &str, &str, u64, &str) {
        (
            self.display_at,
            &self.agent,
            &self.source,
            self.seq,
            &self.ident,
        )
    }
}

/// Read every agent's event logs under `.clank/agents/*/events/` and
/// merge. Corrupt logs are skipped with one stderr notice each —
/// a viewer degrades, never quarantines (the ingest side owns that).
pub(crate) fn timeline_snapshot(repo: &Path) -> TimelineSnapshot {
    // Anything still in a file inbox joins the log before it is read,
    // so a legacy message is in the timeline the first time somebody
    // LOOKS rather than the next time its recipient waits.
    crate::inbox::migrate_all(repo);
    let local_repo = dunce::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut copies: Vec<Copy> = Vec::new();
    let mut notices: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let agents_root = crate::agent_store::agents_root(repo);
    let Ok(agents) = std::fs::read_dir(&agents_root) else {
        return TimelineSnapshot::default();
    };
    for agent in agents.flatten() {
        let label = agent.file_name().to_string_lossy().into_owned();
        // Source key -> glyph, from this agent's CURRENT config. The
        // glyph is presentation config, never stored in the WAL, so
        // it is joined here exactly as a watch's prompt is
        // (github-watch-prompts).
        let glyphs = command_glyphs(repo, &label);
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
                    // Anything that projects as an EVENT belongs here;
                    // non-events can't merge and would render as blank
                    // entries, so they are filtered rather than blanked
                    // (codex 92e9deb). This used to test for
                    // `GithubEvent` and said so — "the github wait path
                    // only logs github events today, so this is belt" —
                    // which stopped being true the moment command
                    // sources shared this log, turning the belt into a
                    // silent drop (codex 4e47a2a).
                    copies.extend(read.rows.into_iter().filter_map(|r| {
                        let v = r.item.event_view()?;
                        // NAMESPACED by backend and by the identity
                        // that backend actually owns. Scoping on the
                        // display source alone collided a command
                        // source named `o/r` emitting id `123` with
                        // github repo `o/r` at feed id `123` — one
                        // merged entry, and the page's ack fanout then
                        // acknowledged both (codex 80e1fb9). Github
                        // keeps its repo, because its aliases must
                        // cross agents' watches of the SAME repo;
                        // a command source keys on its WAL stem, which
                        // is its durable identity.
                        // The TRANSPORT tells a message from a command
                        // source's line: both lack github's carve-out,
                        // and calling a message a command would label
                        // the row with something nobody ran.
                        let (ns, backend) = match (&v.github, r.transport) {
                            (Some(_), _) => (format!("gh:{}", v.source), Backend::Github),
                            (None, crate::cli::github::log::Transport::Message) => {
                                (format!("msg:{stem}"), Backend::Message)
                            }
                            (None, crate::cli::github::log::Transport::Timer) => {
                                (format!("timer:{stem}"), Backend::Timer)
                            }
                            (None, _) => (format!("cmd:{stem}"), Backend::Command),
                        };
                        // The PRODUCER's time when it gave one: a
                        // command source stores it on the item, and
                        // rows written before the envelope carried it
                        // would otherwise display as ingested-now and
                        // sort as new (codex 80e1fb9).
                        let display_at = r.event_at.or(v.at).unwrap_or(r.at);
                        let event_repo = ns;
                        let mut aliases = Vec::new();
                        if let Some(id) = &r.feed_id {
                            aliases.push((event_repo.clone(), format!("f:{id}")));
                        }
                        if let Some(k) = &r.action_key {
                            aliases.push((event_repo.clone(), format!("k:{k}")));
                        }
                        if let WaitItem::Message { handle, .. } = &r.item
                            && r.action_key
                                .as_deref()
                                .is_some_and(|k| k == format!("message-reply/{handle}"))
                        {
                            aliases.push(delivery_alias(&local_repo, &label, handle));
                        }
                        Some(Copy {
                            outgoing: None,
                            agent: label.clone(),
                            source: stem.to_string(),
                            seq: r.seq,
                            ident: r.ident,
                            transport: r.transport,
                            display_at,
                            backend,
                            glyph: glyphs.get(stem).cloned(),
                            aliases,
                            item: r.item,
                            woken_at: r.woken_at,
                            acked: r.acked,
                            baseline: r.baseline,
                        })
                    }));
                    // Acceptance records also survive after the parent body is
                    // pruned. Read them from the same parsed WAL, with no remote
                    // scan and without turning them into sender inbox work.
                    for state in read.outgoing_replies {
                        let op = state.operation;
                        if op.repo != local_repo
                            || op.author.as_str() != label
                            || stem != crate::inbox::MESSAGES_SOURCE
                        {
                            notices.insert(format!(
                                "{label}/{name}: outgoing reply provenance disagrees — skipped"
                            ));
                            continue;
                        }
                        let id = op.delivery_id();
                        let item = op.item();
                        let alias = delivery_alias(&op.route.repo, op.route.agent.as_str(), &id);
                        copies.push(Copy {
                            outgoing: Some(OutgoingReply {
                                operation_id: op.id,
                                delivery_id: id.clone(),
                                from: label.clone(),
                                from_repo: op.repo,
                                destination: op.route,
                                original: MemberKey {
                                    agent: label.clone(),
                                    source: stem.into(),
                                    seq: op.original_seq,
                                    ident: op.original_ident,
                                },
                                original_handle: op.original_handle,
                                acknowledge: op.acknowledge,
                            }),
                            agent: label.clone(),
                            source: stem.into(),
                            seq: state.seq,
                            ident: id,
                            transport: crate::cli::github::log::Transport::Message,
                            display_at: op.at,
                            backend: Backend::Message,
                            glyph: None,
                            aliases: vec![alias],
                            item,
                            woken_at: None,
                            acked: false,
                            baseline: false,
                        });
                    }
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

fn delivery_alias(repo: &Path, recipient: &str, id: &str) -> (String, String) {
    (
        format!(
            "msg-delivery:{}",
            serde_json::to_string(&(repo, recipient)).unwrap()
        ),
        id.into(),
    )
}

/// This agent's configured command-source glyphs, keyed by the WAL
/// stem its events are filed under.
///
/// Read current presentation config independently of any wait. Unsupported
/// icons quietly use the default badge; they cannot remove an event or
/// generate repeated warnings as the timeline refreshes.
fn command_glyphs(repo: &Path, label: &str) -> HashMap<String, String> {
    let Ok(author) = clank_core::ids::AgentLabel::parse(label) else {
        return HashMap::new();
    };
    let Ok(Some(cfg)) = crate::agent_store::load_agent_config(repo, &author) else {
        return HashMap::new();
    };
    cfg.wait_events
        .iter()
        .filter_map(|s| match s {
            clank_core::agent_config::WaitEventSource::Command(c) => {
                let g = crate::cli::command_source::display_glyph(c.glyph.as_deref()?)?;
                Some((c.source_key(), g.to_string()))
            }
            _ => None,
        })
        .collect()
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
    // (display_at, agent, source, seq, ident) — a TOTAL key — so field-wise
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
            // ONE extraction for every backend. `first` then fills
            // each optional from the earliest member that has it, so a
            // copy logged before a field existed does not blank it.
            let (source, kind, detail, number, failure) = copies[members[0]]
                .item
                .event_view()
                .map(|v| {
                    (
                        v.source.to_string(),
                        v.kind.to_string(),
                        v.github.as_ref().and_then(|g| g.detail).map(str::to_owned),
                        v.github.as_ref().and_then(|g| g.number),
                        v.failure,
                    )
                })
                .unwrap_or_default();
            let inbound: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&i| copies[i].outgoing.is_none())
                .collect();
            let mut seen_by: Vec<String> =
                inbound.iter().map(|&i| copies[i].agent.clone()).collect();
            seen_by.sort();
            seen_by.dedup();
            let mut member_refs: Vec<MemberRef> = inbound
                .iter()
                .map(|&i| MemberRef {
                    key: MemberKey {
                        agent: copies[i].agent.clone(),
                        source: copies[i].source.clone(),
                        seq: copies[i].seq,
                        ident: copies[i].ident.clone(),
                    },
                    woken_at: copies[i].woken_at,
                    acked: copies[i].acked,
                    transport: copies[i].transport,
                    content: copies[i]
                        .item
                        .event_view()
                        .and_then(|v| v.github.and_then(|g| g.content.cloned())),
                })
                .collect();
            member_refs.sort_by(|a, b| a.key.cmp(&b.key));
            let merged = MergedEvent {
                outgoing: members
                    .iter()
                    .filter_map(|&i| copies[i].outgoing.clone())
                    .collect(),
                recurrence: members.iter().find_map(|&i| {
                    copies[i]
                        .item
                        .event_view()
                        .and_then(|v| v.recurrence)
                        .cloned()
                }),
                at: copies[members[0]].display_at,
                // Provenance, from the first member in the same total
                // order every other field uses.
                backend: copies[members[0]].backend,
                // First-some in the same total member order every
                // other field uses: one agent configuring a glyph is
                // enough, and a copy from an agent that did not
                // never blanks it.
                glyph: members.iter().find_map(|&i| copies[i].glyph.clone()),
                source,
                kind,
                detail,
                number,
                failure,
                content: first(&|i| i.event_view().and_then(|v| v.content).map(str::to_owned)),
                // The whole text, when there is more of it than the
                // row shows. A message's row is its subject; this is
                // the message.
                body: first(&|i| i.event_view().and_then(|v| v.body).map(str::to_owned)),
                actor: first(&|i| {
                    i.event_view()
                        .and_then(|v| v.github?.actor)
                        .map(str::to_owned)
                }),
                url: first(&|i| {
                    i.event_view()
                        .and_then(|v| v.github?.url)
                        .map(str::to_owned)
                }),
                seen_by,
                members: member_refs,
                unhandled: inbound.iter().any(|&i| !copies[i].acked),
                baseline: !inbound.is_empty() && inbound.iter().all(|&i| copies[i].baseline),
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
pub(crate) mod tests {

    use clank_core::wait::ContentRef;

    fn member_with(agent: &str, content: Option<ContentRef>) -> MemberRef {
        MemberRef {
            woken_at: None,
            key: MemberKey {
                agent: agent.into(),
                source: "github-o-r-aaaa".into(),
                seq: 1,
                ident: String::new(),
            },
            acked: false,
            transport: crate::cli::github::log::Transport::Poll,
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
    use crate::cli::github::log::Transport;
    use std::path::PathBuf;

    /// A COMMAND event reaches the timeline, keeps its own source,
    /// and does not steal github's carve-out.
    ///
    /// The filter here tested for `GithubEvent` and said why: "the
    /// github wait path only logs github events today, so this is
    /// belt." That stopped being true the moment command sources
    /// shared this WAL, and the belt became a silent drop — both
    /// rows pending, both projecting, and only one reaching the TUI
    /// (codex 4e47a2a).
    #[test]
    fn merged_delivery_never_hides_an_unwoken_pending_recipient() {
        let mut log = member_with("alpha", None);
        let mut other = member_with("beta", None);
        let (repo, _, _, _, _) = reply_fixture(false, true);
        let mut ev = timeline_snapshot(repo.path())
            .events
            .into_iter()
            .find(|e| !e.members.is_empty())
            .unwrap();
        for (a, b, expected) in [
            ((false, None), (false, None), DeliveryState::Unwoken),
            ((false, Some(100)), (false, None), DeliveryState::Unwoken),
            ((true, None), (false, None), DeliveryState::Unwoken),
            ((false, Some(100)), (false, Some(200)), DeliveryState::Woken),
            ((true, None), (false, Some(200)), DeliveryState::Woken),
            ((true, None), (true, None), DeliveryState::Acknowledged),
        ] {
            (log.acked, log.woken_at) = a;
            (other.acked, other.woken_at) = b;
            ev.members = vec![log.clone(), other.clone()];
            ev.unhandled = ev.members.iter().any(|m| !m.acked);
            assert_eq!(ev.delivery(), expected);
            let encoded = serde_json::to_value(&ev).unwrap();
            assert_eq!(encoded["delivery"], serde_json::to_value(expected).unwrap());
            assert_eq!(encoded["members"][1]["acked"], b.0);
        }
        ev.baseline = true;
        assert_eq!(ev.delivery(), DeliveryState::History);
        ev.baseline = false;
        ev.members.clear();
        ev.outgoing = timeline_snapshot(repo.path())
            .events
            .into_iter()
            .find(|e| e.outgoing_only())
            .unwrap()
            .outgoing;
        assert_eq!(ev.delivery(), DeliveryState::Sent);
    }

    #[test]
    fn a_command_event_reaches_the_timeline_beside_a_github_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let events = crate::agent_store::agents_root(repo)
            .join("claude")
            .join("events");
        std::fs::create_dir_all(&events).unwrap();

        let mut gh = EventLog::open(&events, "github-o-r-aaaa").unwrap();
        gh.append_inbox(
            crate::cli::github::log::Transport::Poll,
            Some("f-1"),
            None,
            Some(100),
            &item("o/r", 12, Some("Add the widget")),
        )
        .unwrap();

        let mut cmd = EventLog::open(&events, "command-deploys-0000abcd").unwrap();
        cmd.append_inbox(
            crate::cli::github::log::Transport::Command,
            Some("sha-abc"),
            None,
            Some(200),
            &WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "deploys".into(),
                id: "sha-abc".into(),
                event: "deployed".into(),
                content: "prod is live".into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
        )
        .unwrap();

        let snap = timeline_snapshot(repo);
        assert!(snap.notices.is_empty(), "{:?}", snap.notices);
        assert_eq!(
            snap.events.len(),
            2,
            "both events belong on the timeline: {:?}",
            snap.events,
        );

        let c = snap
            .events
            .iter()
            .find(|e| e.source == "deploys")
            .expect("the command event is not dropped");
        assert_eq!(c.kind, "deployed");
        assert_eq!(c.content.as_deref(), Some("prod is live"));
        assert!(c.unhandled, "still owed");
        // The carve-out is github's alone — a command event must not
        // sprout a PR number or an actor.
        assert_eq!(
            (c.number, c.actor.as_deref(), c.url.as_deref()),
            (None, None, None)
        );
        // Its ack target is source-scoped and addresses ITS row.
        assert_eq!(c.members.len(), 1);
        assert_eq!(c.members[0].key.source, "command-deploys-0000abcd");

        let g = snap
            .events
            .iter()
            .find(|e| e.source == "o/r")
            .expect("the github event still arrives");
        assert_eq!(g.number, Some(12));
        assert_eq!(g.content.as_deref(), Some("Add the widget"));
        assert_eq!(g.actor.as_deref(), Some("alice"));
        assert_eq!(g.members[0].key.source, "github-o-r-aaaa");
    }

    /// Two backends, the same NAME and the same ID, and they stay
    /// two events.
    ///
    /// Aliases were scoped on the display source, which a command
    /// source can set to anything: a source named `o/r` emitting id
    /// `123` merged with github repo `o/r` at feed id `123`, and the
    /// page's ack fanout then acknowledged the unrelated command
    /// event along with the github one (codex 80e1fb9).
    #[test]
    fn a_command_source_named_like_a_repo_never_merges_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let events = crate::agent_store::agents_root(repo)
            .join("claude")
            .join("events");
        std::fs::create_dir_all(&events).unwrap();

        EventLog::open(&events, "github-o-r-aaaa")
            .unwrap()
            .append_inbox(
                crate::cli::github::log::Transport::Poll,
                Some("123"),
                None,
                Some(100),
                &item("o/r", 12, Some("the github one")),
            )
            .unwrap();
        EventLog::open(&events, "command-o-r-0000abcd")
            .unwrap()
            .append_inbox(
                crate::cli::github::log::Transport::Command,
                Some("123"),
                None,
                Some(200),
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "o/r".into(),
                    id: "123".into(),
                    event: "deployed".into(),
                    content: "the command one".into(),
                    at: Some(200),
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();

        let snap = timeline_snapshot(repo);
        assert_eq!(
            snap.events.len(),
            2,
            "same name, same id, different backends: still two events: {:?}",
            snap.events,
        );
        // And each ack target addresses only its own WAL.
        let mut sources: Vec<&str> = snap
            .events
            .iter()
            .flat_map(|e| e.members.iter().map(|m| m.key.source.as_str()))
            .collect();
        sources.sort();
        assert_eq!(sources, ["command-o-r-0000abcd", "github-o-r-aaaa"]);
        for e in &snap.events {
            assert_eq!(
                e.members.len(),
                1,
                "acking one must not fan out to the other: {e:?}",
            );
        }
    }

    /// The configured GLYPH reaches the row.
    ///
    /// `CommandSource.glyph` is documented as the character the log
    /// and the TUI draw this source's events with, and this plan
    /// added load-time validation for it — while nothing drew it. A
    /// validator for a contract no code implements is a guard with
    /// no feature behind it (codex db5821e).
    #[test]
    fn a_configured_glyph_reaches_the_row_and_absence_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agents = crate::agent_store::agents_root(repo);
        let events = agents.join("claude").join("events");
        std::fs::create_dir_all(&events).unwrap();

        let glyphed = clank_core::agent_config::CommandSource {
            interrupt: false,
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: Some("*".into()),
        };
        let plain = clank_core::agent_config::CommandSource {
            interrupt: false,
            name: "builds".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let cfg = clank_core::agent_config::AgentConfig {
            wait_events: vec![
                clank_core::agent_config::WaitEventSource::Command(glyphed.clone()),
                clank_core::agent_config::WaitEventSource::Command(plain.clone()),
            ],
            ..Default::default()
        };
        std::fs::write(
            agents.join("claude").join("config.json"),
            serde_json::to_string(&cfg).unwrap(),
        )
        .unwrap();

        for (src, id) in [(&glyphed, "a"), (&plain, "b")] {
            EventLog::open(&events, &src.source_key())
                .unwrap()
                .append_inbox(
                    crate::cli::github::log::Transport::Command,
                    Some(id),
                    None,
                    Some(100),
                    &WaitItem::CommandEvent {
                        presentation: None,
                        source_cwd: None,
                        name: src.name.clone(),
                        id: id.into(),
                        event: "ran".into(),
                        content: "x".into(),
                        at: Some(100),
                        failure: false,
                        requires_reply: false,
                        reply_route: None,
                        selector: None,
                    },
                )
                .unwrap();
        }

        let snap = timeline_snapshot(repo);
        let glyph_of = |source: &str| {
            snap.events
                .iter()
                .find(|e| e.source == source)
                .unwrap_or_else(|| panic!("{source} is on the timeline"))
                .glyph
                .clone()
        };
        assert_eq!(glyph_of("deploys").as_deref(), Some("*"));
        assert!(
            snap.notices.is_empty(),
            "a valid glyph is quiet: {:?}",
            snap.notices
        );
        assert_eq!(
            glyph_of("builds"),
            None,
            "no glyph configured means the backend's default badge",
        );

        // And it reaches the actual printed row.
        let items: Vec<crate::cli::log::TimelineItem> = snap
            .events
            .iter()
            .enumerate()
            .map(|(i, e)| crate::cli::log::TimelineItem::Github(i, e))
            .collect();
        let rows =
            crate::cli::log::oneline_items_rows(&items, &Default::default(), &Default::default());
        let lines = crate::cli::log::oneline_plain_lines(&rows);
        assert!(
            lines.iter().any(|l| l.starts_with("? * ")),
            "the configured glyph is the gutter badge: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("? cmd ")),
            "and an unconfigured source keeps the default: {lines:?}"
        );
    }

    /// Display normalization reads current config, independently of any wait.
    /// Editing an icon retitles existing events without changing delivery.
    #[test]
    fn an_edited_icon_falls_back_safely_without_hiding_events_or_repeating_notices() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agents = crate::agent_store::agents_root(repo);
        let events = agents.join("claude").join("events");
        std::fs::create_dir_all(&events).unwrap();

        let src = clank_core::agent_config::CommandSource {
            interrupt: false,
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: Some("*".into()),
        };
        EventLog::open(&events, &src.source_key())
            .unwrap()
            .append_inbox(
                crate::cli::github::log::Transport::Command,
                Some("a"),
                None,
                Some(100),
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "deploys".into(),
                    id: "a".into(),
                    event: "ran".into(),
                    content: "x".into(),
                    at: Some(100),
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();

        let write_glyph = |g: &str| {
            let cfg = clank_core::agent_config::AgentConfig {
                wait_events: vec![clank_core::agent_config::WaitEventSource::Command(
                    clank_core::agent_config::CommandSource {
                        reply_command: None,
                        receipt_command: None,
                        glyph: Some(g.to_string()),
                        ..src.clone()
                    },
                )],
                ..Default::default()
            };
            std::fs::write(
                agents.join("claude").join("config.json"),
                serde_json::to_string(&cfg).unwrap(),
            )
            .unwrap();
        };

        for bad in ["\n", "\u{1b}", "", "ab", "\u{0301}"] {
            write_glyph(bad);
            for _ in 0..2 {
                let snap = timeline_snapshot(repo);
                assert_eq!(snap.events.len(), 1, "the EVENT survives a bad icon");
                assert_eq!(snap.events[0].glyph, None, "{bad:?} must not reach display");
                assert!(
                    snap.notices.is_empty(),
                    "fallback does not flood the timeline"
                );
            }
        }
        for icon in ["💬", "e\u{0301}", "👩‍💻", "☁️"] {
            write_glyph(icon);
            let snap = timeline_snapshot(repo);
            assert_eq!(snap.events[0].glyph.as_deref(), Some(icon));
            assert!(snap.notices.is_empty());
        }

        // Fixing it fixes it, with no arm in between.
        write_glyph("*");
        let snap = timeline_snapshot(repo);
        assert_eq!(snap.events[0].glyph.as_deref(), Some("*"));
        assert!(snap.notices.is_empty(), "{:?}", snap.notices);
    }

    /// A SPARSE github event is still a github event.
    ///
    /// The backend used to be read off the carve-out, so a row with
    /// no url, number or actor — which every `branch_push` is —
    /// reported itself as a command source (codex 80e1fb9).
    #[test]
    fn a_github_event_with_no_extras_is_still_github() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let events = crate::agent_store::agents_root(repo)
            .join("claude")
            .join("events");
        std::fs::create_dir_all(&events).unwrap();
        EventLog::open(&events, "github-o-r-aaaa")
            .unwrap()
            .append_inbox(
                crate::cli::github::log::Transport::Poll,
                Some("f-1"),
                None,
                Some(100),
                &WaitItem::GithubEvent {
                    presentation: None,
                    repo: "o/r".into(),
                    event: "branch_push".into(),
                    detail: None,
                    number: None,
                    title: None,
                    actor: None,
                    url: None,
                    instructions: None,
                    content: None,
                },
            )
            .unwrap();

        let snap = timeline_snapshot(repo);
        assert_eq!(snap.events.len(), 1);
        assert_eq!(snap.events[0].backend, Backend::Github);
    }

    /// A command event's own timestamp reaches the timeline.
    ///
    /// The envelope carries it now, and rows written before it did
    /// fall back to the item — otherwise a replayed or backfilled
    /// event displays as ingested-now and sorts as new (codex
    /// 80e1fb9).
    #[test]
    fn a_command_events_producer_time_survives_an_empty_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let events = crate::agent_store::agents_root(repo)
            .join("claude")
            .join("events");
        std::fs::create_dir_all(&events).unwrap();
        EventLog::open(&events, "command-deploys-0000abcd")
            .unwrap()
            .append_inbox(
                crate::cli::github::log::Transport::Command,
                Some("sha-abc"),
                None,
                // EMPTY envelope, as every row written before the
                // supervisor populated it.
                None,
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "deploys".into(),
                    id: "sha-abc".into(),
                    event: "deployed".into(),
                    content: "prod is live".into(),
                    at: Some(123),
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();

        let snap = timeline_snapshot(repo);
        assert_eq!(snap.events.len(), 1);
        assert_eq!(
            snap.events[0].at, 123,
            "the producer's time, not the ingest epoch",
        );
    }

    fn item(repo: &str, n: u64, title: Option<&str>) -> WaitItem {
        WaitItem::GithubEvent {
            presentation: None,
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
        assert_eq!(merged[0].content.as_deref(), Some("fix the thing"));
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

    pub(crate) fn reply_fixture(
        local: bool,
        accepted: bool,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        EventLog,
        EventLog,
        crate::cli::github::log::MessageOperation,
    ) {
        use crate::cli::github::log::{MessageOperation, Transport};
        use clank_core::{ids::AgentLabel, wait::MessageReplyRoute};
        let repo = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(repo.path()).unwrap();
        let target = if local {
            root.clone()
        } else {
            dunce::canonicalize(external.path()).unwrap()
        };
        let mut source = EventLog::open(
            &crate::agent_store::agents_root(&root).join("worker/events"),
            "messages",
        )
        .unwrap();
        let mut destination = EventLog::open(
            &crate::agent_store::agents_root(&target).join("coordinator/events"),
            "messages",
        )
        .unwrap();
        source
            .append_inbox(
                Transport::Message,
                None,
                Some("original"),
                Some(10),
                &WaitItem::Message {
                    presentation: None,
                    handle: "parent-handle".into(),
                    from: Some("coordinator".into()),
                    repo: Some(target.display().to_string()),
                    subject: "Original question".into(),
                    body: "Original question".into(),
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();
        let parent = source.read_rows().unwrap().unwrap().rows.remove(0);
        let op = MessageOperation {
            id: "first-reply".into(),
            repo: root,
            author: AgentLabel::parse("worker").unwrap(),
            original_seq: parent.seq,
            original_ident: parent.ident,
            original_handle: "parent-handle".into(),
            route: MessageReplyRoute {
                repo: target,
                agent: AgentLabel::parse("coordinator").unwrap(),
            },
            text: "  Answer β.\nKeep this whitespace.  ".into(),
            at: 42,
            acknowledge: false,
        };
        let state = source.begin_message_operation(&op).unwrap();
        if accepted {
            let receipt = destination.accept_message_delivery(&op).unwrap();
            source.accept_message_operation(&state, &receipt).unwrap();
            destination.release_message_receipt(&receipt).unwrap();
        }
        (repo, external, source, destination, op)
    }

    #[test]
    fn external_reply_projects_from_local_custody_without_recipient_access() {
        let (repo, external, mut source, _destination, op) = reply_fixture(false, true);
        std::fs::remove_dir_all(external.path()).unwrap();
        let snapshot = timeline_snapshot(repo.path());
        assert!(snapshot.notices.is_empty(), "{:?}", snapshot.notices);
        assert_eq!(snapshot.events.len(), 2);
        let reply = snapshot
            .events
            .iter()
            .find(|e| !e.outgoing.is_empty())
            .unwrap();
        assert!(reply.outgoing_only());
        assert!(!reply.unhandled && !reply.baseline);
        assert!(reply.members.is_empty() && reply.seen_by.is_empty());
        assert_eq!(reply.body.as_deref(), Some(op.text.as_str()));
        assert_eq!(reply.at, op.at);
        assert_eq!(reply.outgoing[0].destination, op.route);
        assert_eq!(reply.outgoing[0].original_handle, "parent-handle");
        assert_eq!(reply.outgoing[0].original.ident, op.original_ident);
        assert_eq!(
            reply.page_keys(),
            vec![EventKey::Outgoing {
                author: "worker".into(),
                operation_id: op.id
            }]
        );
        assert_eq!(
            source.load().unhandled.len(),
            1,
            "outgoing records are not work"
        );
        assert!(
            !external.path().exists(),
            "rendering does not recreate the remote repo"
        );
    }

    #[test]
    fn same_repo_reply_merges_custody_with_only_real_recipient_state() {
        let (repo, _external, _source, mut target, op) = reply_fixture(true, true);
        let snapshot = timeline_snapshot(repo.path());
        assert_eq!(snapshot.events.len(), 2, "original plus one merged reply");
        let reply = snapshot
            .events
            .iter()
            .find(|e| !e.outgoing.is_empty())
            .unwrap();
        assert!(!reply.outgoing_only());
        assert!(reply.unhandled);
        assert_eq!(reply.seen_by, vec!["coordinator"]);
        assert_eq!(reply.members.len(), 1);
        assert_eq!(reply.members[0].key.agent, "coordinator");
        assert_eq!(reply.outgoing[0].delivery_id, op.delivery_id());
        assert_eq!(reply.page_keys().len(), 2);
        let key = &reply.members[0].key;
        target.append_checked_ack(key.seq, &key.ident).unwrap();
        let refreshed = timeline_snapshot(repo.path());
        let reply = refreshed
            .events
            .iter()
            .find(|e| !e.outgoing.is_empty())
            .unwrap();
        assert!(!reply.unhandled && !reply.outgoing_only());
        assert!(reply.members[0].acked);
    }

    #[test]
    fn pending_intents_are_not_sent_and_equal_deliberate_bodies_do_not_merge() {
        let (repo, _external, mut source, mut target, mut op) = reply_fixture(true, false);
        let snapshot = timeline_snapshot(repo.path());
        assert_eq!(snapshot.events.len(), 1);
        assert!(snapshot.events[0].outgoing.is_empty());
        for id in ["first-reply", "second-reply"] {
            op.id = id.into();
            let state = source.begin_message_operation(&op).unwrap();
            let receipt = target.accept_message_delivery(&op).unwrap();
            source.accept_message_operation(&state, &receipt).unwrap();
            target.release_message_receipt(&receipt).unwrap();
        }
        let snapshot = timeline_snapshot(repo.path());
        let replies: Vec<_> = snapshot
            .events
            .iter()
            .filter(|e| !e.outgoing.is_empty())
            .collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].body, replies[1].body);
        assert_ne!(
            replies[0].outgoing[0].delivery_id,
            replies[1].outgoing[0].delivery_id
        );
        assert!(
            replies
                .iter()
                .all(|e| e.members.len() == 1 && e.outgoing.len() == 1)
        );
    }

    #[test]
    fn accepted_reply_outlives_parent_display_and_pin_cleanup() {
        let (repo, _external, _source, _target, op) = reply_fixture(false, true);
        // Model the compacted form: source acceptance is retained independently
        // of an original body, not discovered only through row.message_replies.
        let path =
            crate::agent_store::agents_root(repo.path()).join("worker/events/messages.jsonl");
        let accepted: Vec<_> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["t"] == "message_operation" && !value["receipt"].is_null()
            })
            .map(str::to_owned)
            .collect();
        std::fs::write(&path, accepted.join("\n") + "\n").unwrap();
        let snapshot = timeline_snapshot(repo.path());
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(snapshot.events[0].body.as_deref(), Some(op.text.as_str()));
        assert!(snapshot.events[0].outgoing_only());
    }

    #[test]
    fn missing_dirs_are_empty_not_errors() {
        let repo = tempdir();
        assert!(timeline_snapshot(&repo).events.is_empty());
    }
}
