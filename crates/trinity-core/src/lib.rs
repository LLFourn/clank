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
//! - [`ids`] — validated identifier newtypes (`AgentLabel`,
//!   `PlanKey`, `CommitSha`, `RepoBasename`, `ContentHash`,
//!   `PlanId`). Serde-transparent over String with parse-time
//!   validation enforced on deserialize.
//! - [`model`] — legacy daemon fold-state types (`Plan`,
//!   `PlanTimelineEvent`, `CommitGate`, `Feedback`, `CommitNode`,
//!   `CommitAttribution`). Being phased out by
//!   `core-state-rewrite.md`'s sans-io fold in [`repo_state`];
//!   consumers migrate off these one piece at a time.
//! - [`repo_state`] — new sans-io fold state (`RepoState`,
//!   `PlanState`, `CommitEvent`, `Warning`, projection types).
//!   The cache encodes this directly.
//! - [`api`] — public response DTOs (`GetContextResponse`,
//!   `CommitDetailResponse`, etc.) plus projection-only structs
//!   (`PlanRow`, `CommitRow`, `PrHint`, etc.). Wire shapes that
//!   the daemon's response projection builds from `model` types.
//!
//! Every type derives both `Serialize` and `Deserialize` so the
//! daemon (producer) and the frontend (consumer) round-trip
//! through identical definitions.

pub mod api;
pub mod ids;
pub mod model;
pub mod repo_state;
pub mod vocab;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use trinity_core::CommitKind;`).
pub use vocab::{
    CommitGateState, CommitKind, PlanLifecycle, PlanTouchKind, PlanWorktreeStatus, Posture,
    PrHintOptionKind, ReviewGateState, ReviewTargetPhase, Verdict, WaitingReason, WaitingRole,
};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey, RepoBasename,
};
