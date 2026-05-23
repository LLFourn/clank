//! Pure data types for Clank: closed-vocabulary enums, validated
//! identifier newtypes, the sans-io repo fold, and the wire-shape
//! DTOs produced by projection. No runtime, no git IO, no tokio —
//! compiles to wasm32 unchanged for any future consumer.
//!
//! ## Modules
//!
//! - [`vocab`] — closed-vocab enums (`PlanLifecycle`,
//!   `WaitingReason`, …). Wire form is snake_case.
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
//! - [`feedback_view`] — typed `FeedbackView` of one plan's review
//!   files. Pure data; the CLI scans the filesystem into this shape
//!   and feeds it to `plan_view::project`.
//! - [`plan_view`] — per-plan projection: `gate_state`,
//!   `WaitingOn`, `worktree_status`. The single source of truth
//!   `status` and `wfw` both project from.
//! - [`wait`] — agent-perspective wait surface for `clank wfw`:
//!   the flat tagged `WaitItem` enum (`Master` / `Reviewer` /
//!   `Finished`), `derive_work`, and `detect_finished`. Consumed
//!   by `wfw`; not by `status`.
//! - [`api`] — wire-shape response DTOs (`FinishPreviewResponse`,
//!   `RewritePreviewResponse`, …) produced by the CLI's preview
//!   builders.
//!
//! Every type derives both `Serialize` and `Deserialize` so
//! producers and consumers round-trip through identical
//! definitions.

pub mod api;
pub mod feedback_view;
pub mod ids;
pub mod model;
pub mod plan_view;
pub mod repo_state;
pub mod vocab;
pub mod wait;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use clank_core::Verdict;`).
pub use vocab::{CommitGateState, PlanLifecycle, PlanWorktreeStatus, Verdict, WaitingReason};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitRef, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey,
    RepoBasename,
};
