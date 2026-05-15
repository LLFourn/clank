//! SSE-backed reactive event store.
//!
//! `EventStore` is provided once at app boot via `provide_context` and
//! consumed by route components that need live invalidation:
//!
//! - `tick: RwSignal<u64>` increments on every event; resources key on it
//!   to invalidate. Coarse-grained for Phase 4 — every event invalidates
//!   every subscribed resource. Later phases can replace `tick` with
//!   per-(repo, session_id) signals.
//! - `recent: RwSignal<Vec<LiveEvent>>` is the rolling log feeding the
//!   homepage activity sidebar.
//!
//! `connect_sse` opens an `EventSource` to `/events`, parses each message
//! into a `LiveEvent`, and pushes into the store. The browser handles
//! reconnection automatically; if the daemon goes away the EventSource
//! enters a CONNECTING state until it comes back.

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

const RECENT_CAP: usize = 50;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct LiveEvent {
    pub ts: i64,
    pub repo: String,
    pub session_id: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Copy)]
pub struct EventStore {
    pub tick: RwSignal<u64>,
    pub recent: RwSignal<Vec<LiveEvent>>,
}

impl EventStore {
    pub fn new() -> Self {
        Self {
            tick: RwSignal::new(0),
            recent: RwSignal::new(Vec::new()),
        }
    }

    pub fn push(&self, event: LiveEvent) {
        self.tick.update(|t| *t = t.wrapping_add(1));
        self.recent.update(|v| {
            v.insert(0, event);
            v.truncate(RECENT_CAP);
        });
    }
}

/// Open an `EventSource` against the daemon's `/events` endpoint and
/// pipe every message into the store. The EventSource handle and the
/// onmessage closure intentionally leak — they live for the lifetime of
/// the tab. The browser handles reconnect; we don't observe transport
/// state here for Phase 4 (a "disconnected" indicator can come later).
pub fn connect_sse(store: EventStore) {
    let es = match web_sys::EventSource::new("/events") {
        Ok(es) => es,
        Err(_) => return,
    };
    let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
        let Some(text) = ev.data().as_string() else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<LiveEvent>(&text) else {
            return;
        };
        store.push(parsed);
    });
    es.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();
    // Hold the EventSource open for the tab's lifetime.
    Box::leak(Box::new(es));
}
