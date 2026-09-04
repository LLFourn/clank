//! The one integration harness (the-test-suite-takes-a-million-years).
//!
//! Every file here used to be its own test executable, and each one
//! linked the whole `clank` library and was a new binary for macOS to
//! assess on first launch after every lib change. As modules of one
//! harness they are the same tests, same in-process calls, same
//! fixtures — one link, one assessment.

mod common;

mod agent_swap_integration;
mod diff_tree_changes_scenarios;
mod doctor_unbound_reviewer_integration;
mod feedback_write_integration;
mod finish_integration;
mod fork_integration;
mod git_boundary;
mod html_integration;
mod no_json_literal_config_writes;
mod open_integration;
mod pick_integration;
mod pr_review_integration;
mod rereview_integration;
mod stash_integration;
mod status_log_integration;
mod stop_hook_peek_no_hooks;
mod wait_config_reload;
mod wait_event_sources;
mod wait_for_observer;
mod zellij_cost_boundary;
mod zellij_ownership_boundary;
