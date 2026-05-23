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
//!   into state; built on demand from
//!   `.clank/agents/<author>/feedback/<plan-or-_>/<ref>.md` files.
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
//! - [`agent_config`] — typed schemas for per-agent
//!   (`.clank/agents/<label>/config.json`) and repo-level
//!   (`.clank/config.json`) settings, plus the `role_for` helper
//!   used by the identity resolver.
//! - [`identity`] — pure `resolve_agent_identity` function +
//!   `IdentityInputs` / `ResolveError` types. The single
//!   "who am I" resolver shared by stop-hook, auto, wfw, doctor.
//! - [`hook_io`] — typed Stop-hook stdin (`HookInput`) and the
//!   adapter's decision (`HookOutcome`); plus the codex
//!   `{decision:"block",reason:...}` wire shape.
//!
//! Every type derives both `Serialize` and `Deserialize` so
//! producers and consumers round-trip through identical
//! definitions.

pub mod agent_config;
pub mod api;
pub mod feedback_body;
pub mod feedback_view;
pub mod hook_io;
pub mod identity;
pub mod ids;
pub mod model;
pub mod plan_view;
pub mod repo_state;
pub mod vocab;
pub mod wait;

// Re-export the closed-vocab enums at crate root for ergonomic
// imports (`use clank_core::Verdict;`).
pub use vocab::{
    AutoMode, CommitGateState, PlanLifecycle, PlanWorktreeStatus, Role, Tool, Verdict,
    WaitingReason,
};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitRef, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey,
    RepoBasename, SessionId,
};

pub use agent_config::{AgentConfig, RepoConfig, Session, role_for};
pub use hook_io::{
    CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
};
pub use identity::{IdentityInputs, ResolveError, resolve_agent_identity};
