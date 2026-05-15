//! SSE-backed reactive event store.
//!
//! `EventStore` is provided once at app boot via `provide_context` and
//! consumed by route components that need live invalidation:
//!
//! - `tick: RwSignal<u64>` increments on every event; resources key on it
//!   to invalidate. Coarse-grained for Phase 4 — every event invalidates
//!   every subscribed resource.
//! - `recent: RwSignal<Vec<LiveEvent>>` is the rolling log feeding the
//!   homepage activity sidebar.
//! - `muted: RwSignal<bool>` controls the chime, persisted to
//!   `localStorage`.
//!
//! `connect_sse` opens an `EventSource` to `/events`, parses each message
//! into a `LiveEvent`, and pushes into the store. The browser handles
//! reconnection automatically.
//!
//! Phase 5 replaces the inline maud chime: every event now plays a short
//! Web Audio ping (unless muted, or coalesced within 300ms of the previous
//! event). The mute state is read on boot and written through on toggle.

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

const RECENT_CAP: usize = 50;
const CHIME_COALESCE_MS: f64 = 300.0;
const MUTE_LS_KEY: &str = "trinity.muted";

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
    pub muted: RwSignal<bool>,
}

impl EventStore {
    pub fn new() -> Self {
        Self {
            tick: RwSignal::new(0),
            recent: RwSignal::new(Vec::new()),
            muted: RwSignal::new(read_persisted_mute()),
        }
    }

    pub fn push(&self, event: LiveEvent) {
        self.tick.update(|t| *t = t.wrapping_add(1));
        self.recent.update(|v| {
            v.insert(0, event);
            v.truncate(RECENT_CAP);
        });
    }

    pub fn toggle_mute(&self) {
        let new_value = !self.muted.get_untracked();
        self.muted.set(new_value);
        write_persisted_mute(new_value);
    }
}

/// Open an `EventSource` against the daemon's `/events` endpoint and
/// pipe every message into the store. Also plays a short Web Audio
/// chime per event (coalesced + mute-aware). The EventSource handle
/// and onmessage closure intentionally leak — they live for the
/// lifetime of the tab.
pub fn connect_sse(store: EventStore) {
    let es = match web_sys::EventSource::new("/events") {
        Ok(es) => es,
        Err(_) => return,
    };
    // Last-chime timestamp; closed over by the onmessage closure to
    // coalesce flurries. Wrapped in Rc<Cell<_>> because the closure
    // is FnMut.
    let last_chime = std::rc::Rc::new(std::cell::Cell::new(0.0_f64));
    let onmessage_state = (store, last_chime.clone());
    let onmessage =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let (store, last_chime) = &onmessage_state;
            let Some(text) = ev.data().as_string() else {
                return;
            };
            let Ok(parsed) = serde_json::from_str::<LiveEvent>(&text) else {
                return;
            };
            store.push(parsed);

            if store.muted.get_untracked() {
                return;
            }
            let now = performance_now();
            if now - last_chime.get() < CHIME_COALESCE_MS {
                return;
            }
            last_chime.set(now);
            chime();
        });
    es.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();
    Box::leak(Box::new(es));
}

/// Short sine-wave ping at 880Hz with a quick attack/decay envelope.
/// Fire-and-forget — failures (no AudioContext support, autoplay block)
/// are silently swallowed.
fn chime() {
    let ctx = match web_sys::AudioContext::new() {
        Ok(ctx) => ctx,
        Err(_) => return,
    };
    let t = ctx.current_time();
    let Ok(osc) = ctx.create_oscillator() else {
        return;
    };
    let Ok(gain) = ctx.create_gain() else { return };
    osc.set_type(web_sys::OscillatorType::Sine);
    let _ = osc.frequency().set_value_at_time(880.0, t);
    let _ = gain.gain().set_value_at_time(0.0001, t);
    let _ = gain
        .gain()
        .exponential_ramp_to_value_at_time(0.10, t + 0.01);
    let _ = gain
        .gain()
        .exponential_ramp_to_value_at_time(0.0001, t + 0.15);
    if osc.connect_with_audio_node(&gain).is_err() {
        return;
    }
    if gain
        .connect_with_audio_node(&ctx.destination())
        .is_err()
    {
        return;
    }
    let _ = osc.start_with_when(t);
    let _ = osc.stop_with_when(t + 0.18);
}

fn performance_now() -> f64 {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return 0.0,
    };
    window.performance().map(|p| p.now()).unwrap_or(0.0)
}

fn read_persisted_mute() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return false;
    };
    matches!(storage.get_item(MUTE_LS_KEY), Ok(Some(ref v)) if v == "1")
}

fn write_persisted_mute(muted: bool) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return;
    };
    let value = if muted { "1" } else { "0" };
    let _ = storage.set_item(MUTE_LS_KEY, value);
}
