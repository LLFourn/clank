// Core types and pure projections.
pub mod disk_format;
pub mod disk_snapshot;
pub mod lifecycle;
pub mod rebuild;
pub mod repo_state;

// IO + runtime layers.
pub mod agent_env;
pub mod agent_store;
pub mod feedback_scan;
pub mod fs_review_lookup;
pub mod fs_watcher;
pub mod git_io;
pub mod hook_config;
pub mod init_facts;
pub mod preview;
pub mod runtime;
pub mod state_cache;
pub mod worktree_facts;

// Operator CLI subcommands. Local-only mutations.
pub mod cli;
