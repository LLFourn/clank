//! Shared wire contracts between the Trinity daemon and the Leptos
//! WASM frontend. Pure data: closed-vocabulary enums + response
//! DTOs. No `tokio` / `axum` / `leptos` / runtime / git IO; compiles
//! to wasm32 unchanged.
//!
//! See `.trinity/plans/purge-stringly-typed.md` for the rationale.
//! Phase 2 defines the types; Phases 3-7 migrate daemon + frontend
//! to use them.
//!
//! ## Modules
//!
//! - [`vocab`] — closed-vocabulary enums (`PlanLifecycle`,
//!   `CommitKind`, `WaitingReason`, etc.). Wire form is snake_case.
//! - [`dto`] — public response DTOs (`GetContextResponse`,
//!   `CommitDetailResponse`, etc.) and the structs that compose
//!   them (`Feedback`, `CommitGate`, `TimelineEvent`).
//!
//! Every wire DTO derives both `Serialize` and `Deserialize` so
//! the daemon (producer) and the frontend (consumer) round-trip
//! through identical types.

pub mod dto;
pub mod vocab;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use trinity_wire::CommitKind;`).
pub use vocab::{
    CommitGateState, CommitKind, DiffLineKind, ExpectedAction, PlanLifecycle, PlanTouchKind,
    PlanWorktreeStatus, Posture, ReviewGateState, ReviewTargetPhase, Verdict, WaitingReason,
    WaitingRole,
};
