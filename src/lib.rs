pub mod daemon;
pub mod domain;
pub mod feedback_path;
pub mod lifecycle;
pub mod mcp_shim;
pub mod review_state;
pub mod storage;
pub mod tools;

// Filesystem-truth-rewrite modules. New core types, pure reducer, and
// pure projections that the in-progress rewrite (see
// `.trinity/plans/filesystem-truth-rewrite.md`) is being built around.
// These live alongside the existing daemon code during the migration;
// the SQL layer will be removed in a later commit.
pub mod attribution;
pub mod disk_format;
pub mod projection;
pub mod reducer;
pub mod repo_state;
