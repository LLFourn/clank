// Core types and pure projections.
pub mod disk_format;
pub mod disk_snapshot;
pub mod lifecycle;
pub mod rebuild;
pub mod repo_state;
pub mod session_probe;

// IO + runtime layers.
pub mod agent_env;
pub mod agent_store;
pub mod feedback_scan;
pub mod fs_plan_state_lookup;
pub mod fs_watcher;
pub mod git_io;
pub mod git_plumbing;
pub mod hook_config;
pub mod init_facts;
pub mod owner_sentinel;
pub mod preview;
pub mod proc_identity;
pub mod repo_watch;
pub mod runtime;
pub mod shell_quote;
pub mod state_cache;
pub mod worktree_facts;

// Operator CLI subcommands. Local-only mutations.
pub mod cli;
