//! Re-exports for review-gate types. The struct is defined in
//! `trinity_core::model::CommitGate` (the storage shape); the wire
//! response builders translate to `api::CommitGate` (with rendered
//! body_html) at projection time.

pub use trinity_core::CommitGateState;
pub use trinity_core::model::CommitGate;
