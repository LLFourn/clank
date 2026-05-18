//! Shared core types between the Trinity daemon and the Leptos
//! WASM frontend. Pure data: closed-vocabulary enums, identifiers,
//! and response DTOs. No `tokio` / `axum` / `leptos` / runtime /
//! git IO; compiles to wasm32 unchanged.
//!
//! See `.trinity/plans/trinity-core-unification.md` for the
//! rationale. Phase 1 renamed `trinity-wire → trinity-core`;
//! Phases 2-8 split the crate into `model` (fold-state types) and
//! `api` (response DTOs) and collapse the daemon's parallel
//! response builders into one projection module.
//!
//! ## Modules
//!
//! - [`vocab`] — closed-vocabulary enums (`PlanLifecycle`,
//!   `CommitKind`, `WaitingReason`, etc.). Wire form is snake_case.
//! - [`dto`] — public response DTOs (`GetContextResponse`,
//!   `CommitDetailResponse`, etc.) and the structs that compose
//!   them (`Feedback`, `CommitGate`, `TimelineEvent`). Phase 4
//!   splits this into `model` + `api`.
//!
//! Every DTO derives both `Serialize` and `Deserialize` so the
//! daemon (producer) and the frontend (consumer) round-trip
//! through identical types.

pub mod dto;
pub mod ids;
pub mod vocab;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use trinity_core::CommitKind;`).
pub use vocab::{
    CommitGateState, CommitKind, DiffLineKind, ExpectedAction, PlanLifecycle, PlanTouchKind,
    PlanWorktreeStatus, Posture, ReviewGateState, ReviewTargetPhase, Verdict, WaitingReason,
    WaitingRole,
};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey, RepoBasename,
};
