// Core types and pure projections.
pub mod attribution;
pub mod diff_parser;
pub mod disk_format;
pub mod disk_snapshot;
pub mod lifecycle;
pub mod projection;
pub mod rebuild;
pub mod repo_state;
pub mod review_state;

// IO + runtime layers.
pub mod fs_watcher;
pub mod git_io;
pub mod preview;
pub mod responses;
pub mod runtime;
pub mod state_cache;

// Operator CLI subcommands (init / finish / purge). Mutations
// live here.
pub mod cli;
