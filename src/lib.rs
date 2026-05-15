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
pub mod runtime_snapshot;

// IO + runtime layers.
pub mod fs_watcher;
pub mod git_io;
pub mod mcp_response;
pub mod runtime;
pub mod ui_response;

// The MCP stdio shim — forwards tool calls to the daemon's HTTP endpoint.
pub mod mcp_shim;

// The HTTP + MCP server backed by the filesystem-truth runtime.
pub mod server;

// Static tool catalog. The descriptor + catalog() function are used by
// `mcp_shim` to answer `tools/list` without the daemon being up.
pub mod tools;
