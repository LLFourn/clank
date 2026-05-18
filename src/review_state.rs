//! Re-exports for review-gate types. The struct is defined in
//! `trinity_core::model::CommitGate` and re-exported as
//! `api::CommitGate` — one type per concept. Markdown bodies on
//! the gate's feedback are raw; the wasm frontend renders to
//! HTML at display time.

pub use trinity_core::CommitGateState;
pub use trinity_core::model::CommitGate;
