// Core types and pure projections.
pub mod diff_parser;
pub mod disk_format;
pub mod disk_snapshot;
pub mod lifecycle;
pub mod rebuild;
pub mod repo_state;

// IO + runtime layers.
pub mod fs_watcher;
pub mod git_io;
pub mod preview;
pub mod runtime;
pub mod state_cache;

// Operator CLI subcommands. Local-only mutations.
pub mod cli;
