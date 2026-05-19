//! SSE-backed reactive event store.
//!
//! `EventStore` is provided once at app boot via `provide_context` and
//! consumed by route components that need live invalidation:
//!
//! - `tick: RwSignal<u64>` increments on every event; resources key on it
//!   to invalidate. Coarse-grained — every event invalidates every
//!   subscribed resource.
//! - `recent: RwSignal<Vec<LiveEvent>>` is the rolling log feeding the
//!   homepage activity sidebar.
//! - `muted: RwSignal<bool>` controls the chime, persisted to
//!   `localStorage`.
//! - `connected: RwSignal<bool>` mirrors the EventSource's open state.
//!   The app shell renders a "reconnecting…" badge while this is false.
//!
//! `connect_sse` opens an `EventSource` to `/events`, parses each message
//! into a `LiveEvent`, and pushes into the store. The browser handles
//! transport-level reconnection automatically; `ReconnectState` decides
//! when a reconnect should bump `tick` to force resources to re-fetch.
//!
//! Each event plays a short Web Audio ping (unless muted, or coalesced
//! within 300 ms of the previous event). The mute state is read on boot
//! and written through on toggle.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

pub use trinity_core::api::LiveEvent;

const RECENT_CAP: usize = 50;
const CHIME_COALESCE_MS: f64 = 300.0;
const MUTE_LS_KEY: &str = "trinity.muted";

pub fn live_event_ts(e: &LiveEvent) -> i64 {
    match e {
        LiveEvent::Repo(r) => r.ts,
        LiveEvent::Plan(p) => p.ts,
    }
}

pub fn live_event_kind_str(e: &LiveEvent) -> String {
    match e {
        LiveEvent::Repo(r) => r.payload.to_string(),
        LiveEvent::Plan(p) => p.payload.to_string(),
    }
}

#[derive(Clone, Copy)]
pub struct EventStore {
    pub tick: RwSignal<u64>,
    pub recent: RwSignal<Vec<LiveEvent>>,
    pub muted: RwSignal<bool>,
    pub connected: RwSignal<bool>,
}

impl EventStore {
    pub fn new() -> Self {
        Self {
            tick: RwSignal::new(0),
            recent: RwSignal::new(Vec::new()),
            muted: RwSignal::new(read_persisted_mute()),
            connected: RwSignal::new(true),
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

/// State machine for SSE reconnects. The DOM-touching `connect_sse`
/// wraps this in `Rc<RefCell<_>>` and threads it through the `onopen`
/// + `onerror` closures; tests exercise it directly.
///
/// `was_disconnected` is a **consumed** flag: `on_open` reads-and-clears
/// it. So the very first open after page load (no prior error) returns
/// `false` from `on_open`, while every open that follows at least one
/// error returns `true` exactly once.
#[derive(Default, Debug)]
pub struct ReconnectState {
    was_disconnected: bool,
}

impl ReconnectState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_error(&mut self) {
        self.was_disconnected = true;
    }

    /// Returns `true` if the caller should bump `tick` (this open is a
    /// recovery from a previous error). Subsequent calls without an
    /// intervening `on_error` return `false`.
    pub fn on_open(&mut self) -> bool {
        std::mem::replace(&mut self.was_disconnected, false)
    }
}

/// Open an `EventSource` against the daemon's `/events` endpoint and
/// pipe every message into the store. Also plays a short Web Audio
/// chime per event (coalesced + mute-aware) and drives the connection
/// indicator. The EventSource handle and the closures intentionally
/// leak — they live for the lifetime of the tab.
pub fn connect_sse(store: EventStore) {
    let es = match web_sys::EventSource::new("/events") {
        Ok(es) => es,
        Err(_) => {
            store.connected.set(false);
            return;
        }
    };
    let reconnect = std::rc::Rc::new(std::cell::RefCell::new(ReconnectState::new()));
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

    let onopen_state = (store, reconnect.clone());
    let onopen = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        let (store, reconnect) = &onopen_state;
        let needs_refresh = reconnect.borrow_mut().on_open();
        store.connected.set(true);
        if needs_refresh {
            store.tick.update(|t| *t = t.wrapping_add(1));
        }
    });
    es.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();

    let onerror_state = (store, reconnect);
    let onerror = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        let (store, reconnect) = &onerror_state;
        reconnect.borrow_mut().on_error();
        store.connected.set(false);
    });
    es.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

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

#[cfg(test)]
mod reconnect_tests {
    use super::ReconnectState;

    #[test]
    fn first_open_does_not_refresh() {
        let mut s = ReconnectState::new();
        assert!(!s.on_open());
    }

    #[test]
    fn error_then_open_refreshes_once() {
        let mut s = ReconnectState::new();
        s.on_error();
        assert!(s.on_open());
    }

    #[test]
    fn duplicate_open_without_error_does_not_refresh() {
        let mut s = ReconnectState::new();
        s.on_error();
        assert!(s.on_open());
        assert!(!s.on_open());
    }

    #[test]
    fn two_errors_then_one_open_refreshes_once() {
        let mut s = ReconnectState::new();
        s.on_error();
        s.on_error();
        assert!(s.on_open());
        assert!(!s.on_open());
    }
}
