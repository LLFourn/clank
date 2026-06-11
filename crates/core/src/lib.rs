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
//! - [`checkpoint`] — pure spacing policy for the fold's on-disk
//!   state checkpoints (`should_checkpoint`, `prune_plan`): every
//!   commit near the tip, gaps doubling with distance.
//! - [`model`] — small surviving DTOs (`Feedback`, `CommitGate`)
//!   used by projection-time review-gate computation. Not folded
//!   into state; built on demand from
//!   `.clank/agents/<author>/feedback/<plan-or-_>/<ref>.md` files.
//! - [`feedback_view`] — typed `FeedbackView` of one plan's review
//!   files. Pure data; the CLI scans the filesystem into this shape
//!   and feeds it to gate computation.
//! - [`plan_view`] — shared `WaitingOn` / `WorktreeFacts` types
//!   the CLI projects from. Gate computation itself lives in
//!   [`wait`] (one place, no parallel implementation).
//! - [`wait`] — gate computation and agent-perspective wait
//!   surface: [`wait::compute_gate`] is the single gate-state
//!   function; [`repo_state::RepoState::derive_status`] folds it
//!   over every plan; `detect_finished` finds plans that flipped
//!   to finished since startup.
//! - [`api`] — wire-shape response DTOs (`FinishPreviewResponse`,
//!   `RewritePreviewResponse`, …) produced by the CLI's preview
//!   builders.
//! - [`agent_config`] — typed schema for per-agent, per-machine
//!   state (`.clank/agents/<label>/config.json`): session
//!   binding, auto-mode, and wfw timeout.
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
pub mod checkpoint;
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
    AutoMode, CommitGateState, HookEvent, PlanLifecycle, PlanWorktreeStatus, Role, Tool, Verdict,
    WaitingReason,
};

// Re-export the identifier newtypes at crate root.
pub use ids::{
    AgentLabel, CommitRef, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey,
    RepoBasename, SessionId,
};

pub use agent_config::{AgentConfig, Session};
pub use hook_io::{
    CLAUDE_CONTINUATION_EXIT, CodexBlockDecision, HOOK_OK_EXIT, HookInput, HookOutcome,
};
pub use identity::{IdentityInputs, ResolveError, resolve_agent_identity};
