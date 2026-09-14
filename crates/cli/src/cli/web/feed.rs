//! The feed: what the page is told, and how a browser that arrives
//! late is told all of it.
//!
//! A subscription child emits each pane's viewport when it starts
//! and then only when the pane changes, so a stream alone shows a
//! late browser blank panes until the agent next prints. This keeps
//! the LATEST of everything and hands each connection that state
//! before the live stream — in the one order that cannot lose an
//! update between the two (see [`Feed::connect`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use super::transcript::{Body, Turn};
use crate::cli::open_zellij::PaneMeta;

/// Turns retained per agent — the last few screens, not the history.
pub(crate) const TURNS_KEPT: usize = 200;

/// One agent's transcript window, scoped to the session it came from
/// and the generation of the read: a turn from another session or an
/// earlier read of the file is not this window's.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct AgentTurns {
    pub(crate) session: String,
    pub(crate) generation: u32,
    pub(crate) turns: Vec<Turn>,
    /// Which tail may write this window. A stop flag cannot stop a
    /// thread already inside a read from publishing afterwards, and a
    /// stale RESET would replace the window a newer tail just built —
    /// after which every real turn is refused as stale. So each tail
    /// is issued a token when it is started, the window records the
    /// current one, and every publication, resets included, must
    /// carry it (codex on 056a342). Never sent to the page.
    #[serde(skip)]
    pub(crate) owner: u64,
}

impl AgentTurns {
    /// Replace the turn with this id, or append it; the window stays
    /// bounded from the front.
    fn upsert(&mut self, turn: Turn) {
        match self.turns.iter_mut().find(|t| t.id == turn.id) {
            Some(slot) => *slot = turn,
            None => self.turns.push(turn),
        }
        if self.turns.len() > TURNS_KEPT {
            let drop = self.turns.len() - TURNS_KEPT;
            self.turns.drain(..drop);
        }
    }
}

/// One line of `zellij subscribe --format json`. Five fields on the
/// wire; every one optional here except the kind, so a zellij that
/// adds a field or an event does not kill the feed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SubscribeEvent {
    pub(crate) event: String,
    #[serde(default)]
    pub(crate) pane_id: Option<String>,
    #[serde(default)]
    pub(crate) viewport: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) scrollback: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) is_initial: bool,
}

/// What a terminal is told so that it SHOWS `viewport` and nothing
/// else. Home, then every line followed by erase-to-end-of-line — a
/// row that got shorter, or empty, must not keep the tail of what
/// was there — then erase-below for rows the viewport no longer has.
/// Built here rather than in the page so the property is a test:
/// `["long content"]` repainted as `["short"]` reads `short`, not
/// `shortcontent` (codex on 56d1e6a, reproduced in a real xterm).
pub(crate) fn repaint(viewport: &[String]) -> String {
    let mut out = String::from("\x1b[H");
    for (i, line) in viewport.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        out.push_str(line);
        out.push_str("\x1b[K");
    }
    out.push_str("\x1b[J");
    out
}

/// One Server-Sent Events frame. The data is compact JSON, which
/// never contains a raw newline — that is what makes one event one
/// frame, since the protocol ends a frame at a blank line.
pub(crate) fn sse_frame(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Everything a browser needs to draw the page from nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Retained {
    /// The status event's data as last published.
    pub(crate) status: Option<serde_json::Value>,
    pub(crate) panes: Vec<PaneMeta>,
    /// The latest full viewport per pane id.
    pub(crate) viewports: BTreeMap<String, Vec<String>>,
    /// Panes the child reported closed; a listing that drops them
    /// clears them.
    pub(crate) closed: BTreeSet<String>,
    /// Each agent's transcript window, by label.
    pub(crate) turns: BTreeMap<String, AgentTurns>,
}

impl Retained {
    /// The frames that reconstruct this state, in the order the page
    /// wants them: the table first (so viewports have terms to land
    /// in), then status, then every viewport.
    pub(crate) fn frames(&self) -> Vec<String> {
        let mut out = vec![sse_frame("panes", &panes_data(&self.panes, &self.closed))];
        if let Some(status) = &self.status {
            out.push(sse_frame("status", status));
        }
        // A `turns` frame REPLACES the agent's window on the page, so a
        // replay is never a duplicate — unlike a turn, which is why
        // viewports may be replayed as-is and turns may not.
        for (agent, window) in &self.turns {
            out.push(turns_frame(agent, window));
        }
        for (id, viewport) in &self.viewports {
            out.push(pane_frame(id, viewport));
        }
        out
    }
}

/// A pane frame is CONTENT only. Whether the pane is open is the
/// table's to say (`panes` frames), never a viewport's: a retained
/// viewport replayed to a late browser must not reopen a pane the
/// table just said was closed (codex on 56d1e6a).
fn pane_frame(id: &str, viewport: &[String]) -> String {
    sse_frame(
        "pane",
        &serde_json::json!({ "pane_id": id, "screen": repaint(viewport) }),
    )
}

fn turns_frame(agent: &str, window: &AgentTurns) -> String {
    sse_frame(
        "turns",
        &serde_json::json!({
            "agent": agent,
            "session": window.session,
            "generation": window.generation,
            "turns": window.turns,
        }),
    )
}

fn turn_frame(agent: &str, session: &str, generation: u32, turn: &Turn) -> String {
    sse_frame(
        "turn",
        &serde_json::json!({
            "agent": agent,
            "session": session,
            "generation": generation,
            "turn": turn,
        }),
    )
}

fn panes_data(panes: &[PaneMeta], closed: &BTreeSet<String>) -> serde_json::Value {
    serde_json::json!({
        "panes": panes,
        "closed": closed.iter().collect::<Vec<_>>(),
    })
}

/// What a fresh listing changed, decided against the retained table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableChange {
    Unchanged,
    /// Same panes, different facts — a resize, an exit. The page is
    /// told; the subscription stands.
    Meta,
    /// A different SET of panes: the subscription names panes, so it
    /// must be restarted with the new set.
    Membership,
}

/// Compare two tables. Membership is the set of ids; everything
/// else is meta.
pub(crate) fn diff_table(old: &[PaneMeta], new: &[PaneMeta]) -> TableChange {
    let ids = |t: &[PaneMeta]| t.iter().map(|p| p.id.clone()).collect::<BTreeSet<_>>();
    if ids(old) != ids(new) {
        return TableChange::Membership;
    }
    if old != new {
        return TableChange::Meta;
    }
    TableChange::Unchanged
}

/// The retained state and the live stream, together.
#[derive(Clone)]
pub(crate) struct Feed {
    state: Arc<Mutex<Retained>>,
    tx: broadcast::Sender<String>,
}

impl Feed {
    /// `capacity` frames of lag before a slow browser is resynced from
    /// state instead of catching up.
    pub(crate) fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            state: Arc::new(Mutex::new(Retained::default())),
            tx,
        }
    }

    /// Change the state and broadcast what changed, under one lock.
    ///
    /// The lock is held across the send on purpose: it is what makes
    /// [`Self::connect`]'s order sufficient. A connection subscribes,
    /// then reads the state. Any publish that completes before its
    /// read is in the state it reads; any publish that completes
    /// after is in the receiver it already holds. Nothing falls
    /// between.
    fn publish(&self, apply: impl FnOnce(&mut Retained) -> Vec<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for frame in apply(&mut state) {
            let _ = self.tx.send(frame);
        }
    }

    /// A browser arrives: everything so far, then a receiver for
    /// everything after. Subscribe FIRST, then read — see `publish`.
    pub(crate) fn connect(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        let rx = self.tx.subscribe();
        let frames = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frames();
        (frames, rx)
    }

    /// A browser that fell behind gets the state again rather than a
    /// hole in its stream. Every frame is a full repaint, so a resync
    /// is only ever redundant, never wrong.
    pub(crate) fn resync(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frames()
    }

    pub(crate) fn status(&self, data: serde_json::Value) {
        self.publish(|s| {
            let frame = sse_frame("status", &data);
            s.status = Some(data);
            vec![frame]
        });
    }

    /// A subscription line. Unknown kinds are kept out of the way,
    /// not treated as errors.
    pub(crate) fn event(&self, ev: SubscribeEvent) {
        match ev.event.as_str() {
            "pane_update" => {
                let (Some(id), Some(viewport)) = (ev.pane_id, ev.viewport) else {
                    return;
                };
                self.publish(|s| {
                    let mut frames = Vec::new();
                    // Output means open. The TABLE says so, before the
                    // content arrives, because content never carries
                    // lifecycle.
                    if s.closed.remove(&id) {
                        frames.push(sse_frame("panes", &panes_data(&s.panes, &s.closed)));
                    }
                    frames.push(pane_frame(&id, &viewport));
                    s.viewports.insert(id, viewport);
                    frames
                });
            }
            "pane_closed" => {
                let Some(id) = ev.pane_id else {
                    return;
                };
                self.publish(|s| {
                    s.closed.insert(id);
                    vec![sse_frame("panes", &panes_data(&s.panes, &s.closed))]
                });
            }
            _ => {}
        }
    }

    /// A fresh listing. Says what changed so the caller can restart
    /// the subscription when the set of panes did.
    pub(crate) fn panes(&self, table: Vec<PaneMeta>) -> TableChange {
        let mut change = TableChange::Unchanged;
        self.publish(|s| {
            change = diff_table(&s.panes, &table);
            if change == TableChange::Unchanged {
                return Vec::new();
            }
            let ids: BTreeSet<String> = table.iter().map(|p| p.id.clone()).collect();
            s.viewports.retain(|id, _| ids.contains(id));
            s.closed.retain(|id| ids.contains(id));
            s.panes = table;
            vec![sse_frame("panes", &panes_data(&s.panes, &s.closed))]
        });
        change
    }

    /// Scope `agent`'s window to `session` — nothing of it read yet —
    /// for the tail holding `owner`, and show that scope NOW. Called by
    /// whoever starts a tail, BEFORE it can publish: any tail with an
    /// earlier token is refused from here on, however far into a read
    /// it already was, and the page stops showing the old session the
    /// moment the binding moves, not when the new file finally opens
    /// (codex on eb143e7). No session is a scope too — a binding
    /// cleared, or moved to a harness that keeps no transcript.
    pub(crate) fn claim_turns(&self, agent: &str, owner: u64, session: Option<&str>) {
        self.publish(|s| {
            let window = AgentTurns {
                session: session.unwrap_or_default().to_string(),
                generation: 0,
                turns: Vec::new(),
                owner,
            };
            let frame = turns_frame(agent, &window);
            s.turns.insert(agent.to_string(), window);
            vec![frame]
        });
    }

    /// A transcript (re)read from its file: the agent's window becomes
    /// exactly these turns, under this session and generation — if
    /// the tail still owns the window.
    pub(crate) fn turns_reset(
        &self,
        agent: &str,
        owner: u64,
        session: &str,
        generation: u32,
        turns: Vec<Turn>,
    ) {
        self.publish(|s| {
            if s.turns.get(agent).is_none_or(|w| w.owner != owner) {
                return Vec::new();
            }
            let mut window = AgentTurns {
                session: session.to_string(),
                generation,
                turns: Vec::new(),
                owner,
            };
            for t in turns {
                window.upsert(t);
            }
            let frame = turns_frame(agent, &window);
            s.turns.insert(agent.to_string(), window);
            vec![frame]
        });
    }

    /// One turn appended to a transcript. Dropped unless it is from
    /// the tail that owns the window, for the session and generation
    /// the window holds.
    pub(crate) fn turn(&self, agent: &str, owner: u64, session: &str, generation: u32, turn: Turn) {
        self.publish(|s| {
            let Some(window) = s.turns.get_mut(agent) else {
                return Vec::new();
            };
            if window.owner != owner || window.session != session || window.generation != generation
            {
                return Vec::new();
            }
            let frame = turn_frame(agent, session, generation, &turn);
            window.upsert(turn);
            vec![frame]
        });
    }

    /// A tool's output, for the tool turn with this id: the turn is
    /// re-sent whole with its output attached — one id, one turn, so
    /// there is no separate attach message to arrive out of order.
    /// An id nobody has (a call older than the window) is nothing.
    pub(crate) fn tool_output(
        &self,
        agent: &str,
        owner: u64,
        session: &str,
        generation: u32,
        id: &str,
        output: String,
    ) {
        self.publish(|s| {
            let Some(window) = s.turns.get_mut(agent) else {
                return Vec::new();
            };
            if window.owner != owner || window.session != session || window.generation != generation
            {
                return Vec::new();
            }
            let Some(turn) = window.turns.iter_mut().find(|t| t.id == id) else {
                return Vec::new();
            };
            let Body::Tool { output: slot, .. } = &mut turn.body else {
                return Vec::new();
            };
            *slot = Some(output);
            vec![turn_frame(agent, session, generation, turn)]
        });
    }

    /// Drop the windows of agents no longer on the roster.
    pub(crate) fn forget_turns_except(&self, keep: &BTreeSet<String>) {
        self.publish(|s| {
            s.turns.retain(|agent, _| keep.contains(agent));
            Vec::new()
        });
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Retained {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What the page holds of transcripts, and how a frame changes it —
/// the reducer the page's JavaScript mirrors line for line. Kept in
/// Rust so the duplicate-and-stale contract codex asked for is a
/// test here rather than a hope in a browser.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PageModel {
    pub(crate) agents: BTreeMap<String, AgentTurns>,
}

impl PageModel {
    /// One SSE frame as the browser receives it. Frames that are not
    /// about turns leave the model alone.
    pub(crate) fn apply(&mut self, frame: &str) {
        let mut event = "";
        let mut data = "";
        for line in frame.lines() {
            if let Some(e) = line.strip_prefix("event: ") {
                event = e;
            } else if let Some(d) = line.strip_prefix("data: ") {
                data = d;
            }
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let generation = v.get("generation").and_then(|g| g.as_u64()).unwrap_or(0) as u32;
        match event {
            "turns" => {
                let Ok(turns) = serde_json::from_value::<Vec<Turn>>(
                    v.get("turns").cloned().unwrap_or_default(),
                ) else {
                    return;
                };
                self.agents.insert(
                    field("agent"),
                    AgentTurns {
                        session: field("session"),
                        generation,
                        turns,
                        // The token is the server's; a page has none.
                        owner: 0,
                    },
                );
            }
            "turn" => {
                let Some(window) = self.agents.get_mut(&field("agent")) else {
                    return;
                };
                if window.session != field("session") || window.generation != generation {
                    return;
                }
                let Ok(turn) =
                    serde_json::from_value::<Turn>(v.get("turn").cloned().unwrap_or_default())
                else {
                    return;
                };
                window.upsert(turn);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/subscribe.jsonl");

    fn fixture_events() -> Vec<SubscribeEvent> {
        FIXTURE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("a real line decodes"))
            .collect()
    }

    fn meta(id: &str, label: &str, columns: u16) -> PaneMeta {
        PaneMeta {
            id: id.into(),
            label: label.into(),
            columns,
            rows: 40,
            exited: false,
        }
    }

    /// The captured lines of zellij 0.45.0, decoded: an initial
    /// update, a later one, a close. The escapes survive intact —
    /// they are what the page renders.
    #[test]
    fn real_subscribe_lines_decode_with_their_escapes() {
        let evs = fixture_events();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].event, "pane_update");
        assert!(evs[0].is_initial);
        assert_eq!(evs[0].pane_id.as_deref(), Some("terminal_5"));
        let vp = evs[0].viewport.as_ref().unwrap();
        assert_eq!(vp.len(), 3);
        assert!(vp[1].contains("\x1b[38;2;"), "24-bit colour escapes kept");
        assert!(!evs[1].is_initial);
        assert_eq!(evs[2].event, "pane_closed");
        assert_eq!(evs[2].pane_id.as_deref(), Some("terminal_5"));
        assert!(evs[2].viewport.is_none());
    }

    /// A zellij that grows the protocol must not kill the feed: an
    /// unknown event decodes and is ignored, an extra field is
    /// tolerated.
    #[test]
    fn a_future_event_or_field_is_not_an_error() {
        let ev: SubscribeEvent =
            serde_json::from_str(r#"{"event":"pane_resized","pane_id":"terminal_5","cols":80}"#)
                .unwrap();
        assert_eq!(ev.event, "pane_resized");
        let feed = Feed::new(8);
        feed.event(ev);
        assert_eq!(feed.snapshot(), Retained::default(), "ignored, not stored");
    }

    /// One event is one frame, whatever the data holds: the JSON is
    /// compact, so a newline inside a string is escaped, never raw.
    #[test]
    fn an_event_is_one_frame() {
        let frame = sse_frame(
            "pane",
            &serde_json::json!({"viewport": ["a\nb", "\x1b[1mbold"]}),
        );
        assert!(frame.starts_with("event: pane\ndata: "));
        assert!(frame.ends_with("\n\n"));
        assert_eq!(frame.matches("\n\n").count(), 1, "exactly one frame end");
        assert!(frame.contains(r"a\nb"), "the newline is escaped: {frame:?}");
    }

    /// A browser that connects after the child's initial viewports
    /// went by receives every pane's current viewport and the
    /// status, before any live frame.
    #[test]
    fn a_late_browser_receives_the_state_first() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        for ev in fixture_events().into_iter().take(1) {
            feed.event(ev);
        }
        feed.status(serde_json::json!({"lamp": "🔨 CLAUDE working"}));

        let (frames, _rx) = feed.connect();
        assert_eq!(frames.len(), 3);
        assert!(frames[0].starts_with("event: panes\n"), "the table first");
        assert!(frames[1].starts_with("event: status\n"));
        assert!(
            frames[2].starts_with("event: pane\n") && frames[2].contains("terminal_5"),
            "then the viewport it would otherwise wait for: {}",
            &frames[2][..40]
        );
    }

    /// Reconnecting while the panes are QUIET is the same case with
    /// nothing live at all: the stream would give nothing, the state
    /// gives everything.
    #[test]
    fn reconnecting_while_quiet_is_not_blank() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        feed.event(fixture_events().remove(1));
        let (first, _) = feed.connect();
        let (again, mut rx) = feed.connect();
        assert_eq!(first, again, "the same full picture, nothing live");
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "and nothing pending on the stream"
        );
    }

    /// The race the order exists for: a publish that lands between a
    /// connection's subscribe and its read is delivered — on the
    /// stream, or already in the state, never lost.
    #[test]
    fn a_publish_between_subscribe_and_read_is_delivered() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        // What `connect` does, taken apart so a publish can be put
        // in the middle.
        let mut rx = feed.tx.subscribe();
        feed.event(fixture_events().remove(0));
        let frames = feed.resync();
        let live: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let saw_viewport = frames
            .iter()
            .chain(live.iter())
            .any(|f| f.starts_with("event: pane\n") && f.contains("terminal_5"));
        assert!(saw_viewport, "in the state, in the stream, or both");
    }

    /// `connect` subscribes BEFORE it reads — proven by holding the
    /// state lock: a `connect` on another thread must register its
    /// receiver while still blocked on the read. Read-then-subscribe
    /// would block first and register nothing until the lock frees,
    /// and that is the window a publish falls through.
    #[test]
    fn connect_subscribes_before_it_reads() {
        let feed = Feed::new(8);
        let held = feed.state.lock().unwrap();
        let connecting = {
            let feed = feed.clone();
            std::thread::spawn(move || feed.connect())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while feed.tx.receiver_count() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "connect never subscribed while the read was blocked"
            );
            std::thread::yield_now();
        }
        assert!(
            !connecting.is_finished(),
            "still blocked on the read, as it must be"
        );
        drop(held);
        let (frames, _rx) = connecting.join().unwrap();
        assert!(!frames.is_empty());
    }

    /// A browser the broadcast reports as lagged is given the state
    /// again; every frame is a full repaint, so redundancy is safe.
    #[test]
    fn a_lagged_browser_is_resynced_from_state() {
        let feed = Feed::new(2);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        let (_, mut rx) = feed.connect();
        for _ in 0..5 {
            feed.event(fixture_events().remove(1));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let frames = feed.resync();
        assert!(frames.iter().any(|f| f.starts_with("event: pane\n")));
    }

    /// A pane that appears one listing AFTER the status change is
    /// picked up on that listing — and it is a membership change, so
    /// the subscription is restarted with it.
    #[test]
    fn a_pane_appearing_later_changes_membership() {
        let feed = Feed::new(8);
        assert_eq!(
            feed.panes(vec![meta("terminal_5", "claude", 120)]),
            TableChange::Membership
        );
        assert_eq!(
            feed.panes(vec![meta("terminal_5", "claude", 120)]),
            TableChange::Unchanged,
            "the listing that saw nothing new does nothing"
        );
        assert_eq!(
            feed.panes(vec![
                meta("terminal_5", "claude", 120),
                meta("terminal_9", "codex", 60)
            ]),
            TableChange::Membership
        );
    }

    /// Only the columns changed: the page is told so it can resize
    /// that term, and the subscription — which names panes, not
    /// sizes — stands.
    #[test]
    fn a_resize_alone_is_meta_not_membership() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        let (_, mut rx) = feed.connect();
        assert_eq!(
            feed.panes(vec![meta("terminal_5", "claude", 80)]),
            TableChange::Meta
        );
        let frame = rx.try_recv().unwrap();
        assert!(frame.starts_with("event: panes\n") && frame.contains("\"columns\":80"));
    }

    /// The child's `pane_closed` marks the pane at once — ahead of any
    /// listing — as a TABLE frame, and a later update from the same
    /// pane reopens it the same way, before its content.
    #[test]
    fn a_close_from_the_child_lands_before_the_next_listing() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        let (_, mut rx) = feed.connect();
        feed.event(fixture_events().remove(2));
        assert!(feed.snapshot().closed.contains("terminal_5"));
        let frame = rx.try_recv().unwrap();
        assert!(
            frame.starts_with("event: panes\n") && frame.contains("\"closed\":[\"terminal_5\"]"),
            "lifecycle is the table's: {frame}"
        );
        feed.event(fixture_events().remove(0));
        assert!(
            !feed.snapshot().closed.contains("terminal_5"),
            "output means open"
        );
        let reopened = rx.try_recv().unwrap();
        assert!(
            reopened.starts_with("event: panes\n") && reopened.contains("\"closed\":[]"),
            "the table says so first: {reopened}"
        );
        assert!(
            rx.try_recv().unwrap().starts_with("event: pane\n"),
            "then the content"
        );
    }

    /// A late browser after a close sees the pane CLOSED, even though
    /// its last viewport is replayed too: content frames carry no
    /// lifecycle, so nothing in the replay can reopen it (codex on
    /// 56d1e6a — a browser probe showed the overlay gone).
    #[test]
    fn a_replayed_viewport_cannot_reopen_a_closed_pane() {
        let feed = Feed::new(8);
        feed.panes(vec![meta("terminal_5", "claude", 120)]);
        feed.event(fixture_events().remove(0));
        feed.event(fixture_events().remove(2));
        let (frames, _) = feed.connect();
        assert!(frames[0].contains("\"closed\":[\"terminal_5\"]"));
        let content: Vec<&String> = frames
            .iter()
            .filter(|f| f.starts_with("event: pane\n"))
            .collect();
        assert_eq!(content.len(), 1, "the viewport is still replayed");
        assert!(
            !content[0].contains("closed") && !content[0].contains("exited"),
            "and says nothing about lifecycle: {}",
            &content[0][..60]
        );
        assert!(
            !frames.iter().any(|f| f.starts_with("event: closed\n")),
            "there is no separate closed event to get out of order"
        );
    }

    /// Every row erases its own tail and the rows below are erased
    /// too: a shorter or empty line replaces what was there instead
    /// of overprinting it.
    #[test]
    fn a_repaint_erases_what_each_row_no_longer_has() {
        let screen = repaint(&["short".into(), "".into(), "end".into()]);
        assert_eq!(screen, "\x1b[Hshort\x1b[K\r\n\x1b[K\r\nend\x1b[K\x1b[J");
        assert_eq!(repaint(&[]), "\x1b[H\x1b[J", "no rows: a cleared screen");
        // The escapes a viewport already carries pass through untouched.
        let styled = repaint(&["\x1b[1mbold\x1b[m".into()]);
        assert!(styled.contains("\x1b[1mbold\x1b[m\x1b[K"));
    }

    fn say(id: &str, text: &str) -> Turn {
        Turn {
            id: id.into(),
            at: Some(1_000),
            who: super::super::transcript::Who::Agent,
            body: Body::Text { text: text.into() },
        }
    }

    fn call(id: &str) -> Turn {
        Turn {
            id: id.into(),
            at: Some(1_000),
            who: super::super::transcript::Who::Agent,
            body: Body::Tool {
                name: "Bash".into(),
                input: "ls".into(),
                output: None,
            },
        }
    }

    /// A tail whose window was handed to a newer tail cannot reset it
    /// — not even from a read it began before the handover — and its
    /// turns are refused too. Without this a stale reset would win,
    /// and every real turn after it would be dropped as stale.
    #[test]
    fn a_superseded_tail_cannot_reset_the_window() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![say("a", "one")]);
        feed.claim_turns("claude", 2, Some("s2"));
        feed.turns_reset("claude", 2, "s2", 1, vec![say("z", "new")]);
        // The old tail, late out of its read.
        feed.turns_reset("claude", 1, "s1", 2, vec![say("stale", "x")]);
        feed.turn("claude", 1, "s2", 1, say("sneak", "x"));
        let w = &feed.snapshot().turns["claude"];
        assert_eq!((w.session.as_str(), w.owner), ("s2", 2));
        assert_eq!(
            w.turns.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["z"]
        );
        // The current tail still can.
        feed.turn("claude", 2, "s2", 1, say("y", "more"));
        assert_eq!(feed.snapshot().turns["claude"].turns.len(), 2);
        // No claim, no window to reset into.
        feed.turns_reset("codex", 9, "s9", 1, vec![say("q", "x")]);
        assert!(!feed.snapshot().turns.contains_key("codex"));
    }

    /// A claim is itself a publication: the window becomes the new
    /// session's, empty, for everyone connected and everyone who
    /// connects after — the old conversation is gone from the page
    /// before the new file is so much as opened, and a binding with
    /// no transcript leaves nothing behind either. Only the claiming
    /// tail can then fill it.
    #[test]
    fn a_claim_shows_the_new_scope_before_anything_is_read() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![say("a", "one")]);
        let (_, mut rx) = feed.connect();
        feed.claim_turns("claude", 2, Some("s2"));
        let mut page = PageModel::default();
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(
            page.agents["claude"],
            AgentTurns {
                session: "s2".into(),
                generation: 0,
                turns: vec![],
                owner: 0
            }
        );
        let (snapshot, _) = feed.connect();
        let mut fresh = PageModel::default();
        for f in &snapshot {
            fresh.apply(f);
        }
        assert_eq!(fresh.agents["claude"].session, "s2");
        assert!(fresh.agents["claude"].turns.is_empty());
        // The old tail, however late, cannot put s1 back.
        feed.turns_reset("claude", 1, "s1", 2, vec![say("stale", "x")]);
        assert_eq!(feed.snapshot().turns["claude"].session, "s2");
        feed.turns_reset("claude", 2, "s2", 1, vec![say("z", "new")]);
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(page.agents["claude"].turns.len(), 1);

        feed.claim_turns("claude", 3, None);
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(
            (
                page.agents["claude"].session.as_str(),
                page.agents["claude"].turns.len()
            ),
            ("", 0)
        );
        feed.turn("claude", 2, "s2", 1, say("late", "x"));
        assert!(feed.snapshot().turns["claude"].turns.is_empty());
    }

    /// The contract: a turn present in the retained window AND
    /// arriving live is ONE turn on the page — the handoff may deliver
    /// it twice, and the upsert by id absorbs that.
    #[test]
    fn a_turn_delivered_twice_is_one_turn_on_the_page() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![say("a", "one")]);
        let (snapshot, mut rx) = feed.connect();
        feed.turn("claude", 1, "s1", 1, say("b", "two"));
        let mut page = PageModel::default();
        for f in &snapshot {
            page.apply(f);
        }
        // What the handoff can do: the same turn from the snapshot's
        // successor AND the stream.
        page.apply(
            &feed
                .resync()
                .into_iter()
                .find(|f| f.starts_with("event: turns\n"))
                .unwrap(),
        );
        page.apply(&rx.try_recv().unwrap());
        let w = &page.agents["claude"];
        assert_eq!(
            w.turns.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    /// A reconnect's `turns` frame replaces the window rather than
    /// doubling it, and a lag resync is only ever redundant.
    #[test]
    fn a_reconnect_replaces_the_window() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![say("a", "one"), say("b", "two")]);
        let mut page = PageModel::default();
        for _ in 0..3 {
            for f in feed.resync() {
                page.apply(&f);
            }
        }
        assert_eq!(page.agents["claude"].turns.len(), 2);
    }

    /// A tool's output re-sends the tool's turn whole and lands on
    /// the same id — no attach message, no ordering to get wrong.
    #[test]
    fn a_tools_output_lands_on_the_tools_own_turn() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![call("toolu_01")]);
        let (frames, mut rx) = feed.connect();
        feed.tool_output("claude", 1, "s1", 1, "toolu_01", "ok".into());
        let mut page = PageModel::default();
        for f in frames {
            page.apply(&f);
        }
        page.apply(&rx.try_recv().unwrap());
        let w = &page.agents["claude"];
        assert_eq!(w.turns.len(), 1, "still one turn");
        assert!(matches!(&w.turns[0].body, Body::Tool { output: Some(o), .. } if o == "ok"));
        // An output for a call older than the window is nothing.
        feed.tool_output("claude", 1, "s1", 1, "toolu_ancient", "x".into());
        assert!(rx.try_recv().is_err());
    }

    /// A turn for a stale session or generation is dropped — by the
    /// feed and by the page — and a `turns` frame with a new
    /// generation clears what was there.
    #[test]
    fn stale_turns_are_dropped_and_a_new_generation_clears() {
        let feed = Feed::new(16);
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, vec![say("a", "one")]);
        let (frames, mut rx) = feed.connect();
        feed.turn("claude", 1, "s1", 0, say("old-gen", "x"));
        feed.turn("claude", 1, "s0", 1, say("old-session", "x"));
        assert!(rx.try_recv().is_err(), "the feed forwards neither");
        let mut page = PageModel::default();
        for f in frames {
            page.apply(&f);
        }
        // The page enforces the same scope on its own.
        page.apply(&turn_frame("claude", "s1", 0, &say("old-gen", "x")));
        page.apply(&turn_frame("claude", "s0", 1, &say("old-session", "x")));
        assert_eq!(page.agents["claude"].turns.len(), 1);
        // The file was re-read: generation 2 replaces the window.
        feed.turns_reset("claude", 1, "s1", 2, vec![say("z", "fresh")]);
        page.apply(&rx.try_recv().unwrap());
        let w = &page.agents["claude"];
        assert_eq!(
            (w.generation, w.turns[0].id.as_str(), w.turns.len()),
            (2, "z", 1)
        );
        // A rebind to another session is a new window too.
        feed.turns_reset("claude", 1, "s2", 1, vec![]);
        page.apply(&rx.try_recv().unwrap());
        assert_eq!(
            (
                page.agents["claude"].session.as_str(),
                page.agents["claude"].turns.len()
            ),
            ("s2", 0)
        );
    }

    /// The window is bounded from the front, on the feed and on the
    /// page alike.
    #[test]
    fn the_window_keeps_the_last_turns_only() {
        let feed = Feed::new(16);
        let many: Vec<Turn> = (0..TURNS_KEPT + 5)
            .map(|i| say(&format!("t{i}"), "x"))
            .collect();
        feed.claim_turns("claude", 1, Some("s1"));
        feed.turns_reset("claude", 1, "s1", 1, many);
        let w = &feed.snapshot().turns["claude"];
        assert_eq!(w.turns.len(), TURNS_KEPT);
        assert_eq!(w.turns[0].id, "t5");
    }

    /// A listing that drops a pane drops what was retained for it:
    /// a closed pane's stale viewport must not be replayed to the
    /// next browser as if it were live.
    #[test]
    fn a_dropped_pane_takes_its_viewport_with_it() {
        let feed = Feed::new(8);
        feed.panes(vec![
            meta("terminal_5", "claude", 120),
            meta("terminal_9", "codex", 60),
        ]);
        feed.event(fixture_events().remove(0));
        feed.panes(vec![meta("terminal_9", "codex", 60)]);
        let s = feed.snapshot();
        assert!(s.viewports.is_empty() && s.panes.len() == 1);
    }
}
