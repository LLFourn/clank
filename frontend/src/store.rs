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
    /// `None` for repo-level events. Plan-scoped events carry the
    /// canonical plan identity `<repo_basename>/<stem>.md`.
    #[serde(default)]
    pub plan_id: Option<String>,
    /// Plan stem only — convenience for display.
    #[serde(default)]
    pub slug: Option<String>,
    /// `"active"` | `"done"`, or `None` for repo-level events.
    #[serde(default)]
    pub state: Option<String>,
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

    /// Toggle mute and use the click as a user gesture to unlock the
    /// shared `AudioContext` (browsers gate audio playback on a gesture
    /// — the very first chime after page load otherwise has no audio).
    pub fn toggle_mute(&self) {
        let new_value = !self.muted.get_untracked();
        self.muted.set(new_value);
        write_persisted_mute(new_value);
        if !new_value {
            resume_audio_ctx();
        }
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

// Browsers cap concurrent AudioContexts (~6 per tab) and require a
// user gesture before audio can play. Keep a single context, lazy-init
// on first chime, and resume() it whenever the mute toggle is clicked
// (a real user gesture) so subsequent chimes audible.
thread_local! {
    static AUDIO_CTX: std::cell::RefCell<Option<web_sys::AudioContext>>
        = const { std::cell::RefCell::new(None) };
}

fn with_audio_ctx<F>(f: F)
where
    F: FnOnce(&web_sys::AudioContext),
{
    AUDIO_CTX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            match web_sys::AudioContext::new() {
                Ok(ctx) => *slot = Some(ctx),
                Err(_) => return,
            }
        }
        if let Some(ctx) = slot.as_ref() {
            f(ctx);
        }
    });
}

/// Resume the shared AudioContext. Call from a real user-gesture
/// handler (mute toggle click) so the browser permits audio playback.
fn resume_audio_ctx() {
    with_audio_ctx(|ctx| {
        let _ = ctx.resume();
    });
}

/// Short sine-wave ping at 880Hz with a quick attack/decay envelope.
/// Fire-and-forget — failures (no AudioContext support, autoplay block
/// before the first user gesture) are silently swallowed. Reuses one
/// AudioContext across the tab's lifetime.
fn chime() {
    let _ = try_chime();
}

fn try_chime() -> Result<(), wasm_bindgen::JsValue> {
    let mut result: Result<(), wasm_bindgen::JsValue> = Ok(());
    with_audio_ctx(|ctx| {
        result = chime_with(ctx);
    });
    result
}

fn chime_with(ctx: &web_sys::AudioContext) -> Result<(), wasm_bindgen::JsValue> {
    let t = ctx.current_time();
    let osc = ctx.create_oscillator()?;
    let gain = ctx.create_gain()?;
    osc.set_type(web_sys::OscillatorType::Sine);
    osc.frequency().set_value_at_time(880.0, t)?;
    gain.gain().set_value_at_time(0.0001, t)?;
    gain.gain()
        .exponential_ramp_to_value_at_time(0.10, t + 0.01)?;
    gain.gain()
        .exponential_ramp_to_value_at_time(0.0001, t + 0.15)?;
    osc.connect_with_audio_node(&gain)?;
    gain.connect_with_audio_node(&ctx.destination())?;
    osc.start_with_when(t)?;
    osc.stop_with_when(t + 0.18)?;
    Ok(())
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
