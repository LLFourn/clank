//! Pure data types for Clank: closed-vocabulary enums, validated
//! identifier newtypes, the sans-io repo fold, and the wire-shape
//! DTOs produced by projection. No runtime, no git IO, no tokio —
//! compiles to wasm32 unchanged for any future consumer.
//!
//! ## Modules
//!
//! - [`vocab`] — closed-vocab enums (`PlanLifecycle`,
//!   `CommitKind`, `WaitingReason`, …). Wire form is snake_case.
//! - [`ids`] — validated identifier newtypes (`AgentLabel`,
//!   `PlanKey`, `CommitSha`, `RepoBasename`, `ContentHash`,
//!   `PlanId`). Serde-transparent over `String` with parse-time
//!   validation enforced on deserialize.
//! - [`repo_state`] — the sans-io fold (`RepoState`, `PlanState`,
//!   `CommitEvent`, `Warning`, …) plus the projection types
//!   (`CommitNode`, `CommitReview`, …) consumers build from it.
//!   The on-disk state cache encodes `RepoState` directly.
//! - [`model`] — small surviving DTOs (`Feedback`, `CommitGate`)
//!   used by projection-time review-gate computation. Not folded
//!   into state; built on demand from `.clank/feedback/` files.
//! - [`api`] — wire-shape response DTOs (`FinishPreviewResponse`,
//!   `RewritePreviewResponse`, …) produced by the CLI's preview
//!   builders.
//!
//! Every type derives both `Serialize` and `Deserialize` so
//! producers and consumers round-trip through identical
//! definitions.

pub mod api;
pub mod ids;
pub mod model;
pub mod repo_state;
pub mod vocab;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use clank_core::CommitKind;`).
pub use vocab::{
    CommitGateState, CommitKind, PlanLifecycle, PlanTouchKind, PlanWorktreeStatus, Posture,
    PrHintOptionKind, ReviewGateState, ReviewTargetPhase, Verdict, WaitingReason, WaitingRole,
};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey, RepoBasename,
};
