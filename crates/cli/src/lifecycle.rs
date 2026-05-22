//! Identifiers used across the filesystem-truth model.
//!
//! The newtypes themselves (`AgentLabel`, `PlanKey`, `CommitSha`,
//! `RepoBasename`, `ContentHash`, `PlanId`) live in `clank-core`
//! so the daemon and the WASM frontend can share them with
//! validate-on-deserialize guarantees. This module re-exports them
//! and provides the daemon-only `content_hash` factory (which calls
//! `blake3`, not a wire crate dep).

pub use clank_core::ids::{
    AgentLabel, CommitSha, ContentHash, IdError, ParsePlanIdError, PlanId, PlanKey, RepoBasename,
};

/// Stable content hash for a plan-file body. Identifies "is this the
/// same plan or a new one?" in the rebuild + worktree-status logic.
///
/// Daemon-only because it depends on `blake3`. The wire crate has
/// `ContentHash::parse` for the validate-on-deserialize side.
pub fn content_hash(body: &str) -> ContentHash {
    ContentHash::from_hex_unchecked(blake3::hash(body.as_bytes()).to_hex().to_string())
}
