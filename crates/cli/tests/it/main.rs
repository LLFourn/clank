//! The one integration harness (the-test-suite-takes-a-million-years).
//!
//! Every file here used to be its own test executable, and each one
//! linked the whole `clank` library and was a new binary for macOS to
//! assess on first launch after every lib change. As modules of one
//! harness they are the same tests, same in-process calls, same
//! fixtures — one link, one assessment.

mod common;

mod agent_setup_acceptance;
mod agent_swap_integration;
mod block_blast_radius;
mod clank_dir_boundary;
mod diff_tree_changes_scenarios;
mod doctor_unbound_reviewer_integration;
mod feedback_read_integration;
mod feedback_write_integration;
mod finish_integration;
mod fork_integration;
mod git_boundary;
mod grok_launch_acceptance;
mod html_integration;
mod identity_env_boundary;
mod inbox_wakes;
mod no_json_literal_config_writes;
mod no_ngrok;
mod no_orphan_doc_comments;
mod open_integration;
mod pick_integration;
mod plan_admission;
mod pr_review_integration;
mod rereview_integration;
mod signal_boundary;
mod standby_wakes;
mod stash_integration;
mod status_log_integration;
mod tui_is_the_command;
mod wait_config_reload;
mod wait_event_sources;
mod wait_for_observer;
mod wake_receipt_acceptance;
mod web_host_process;
mod web_timeline_boundary;
mod zellij_cost_boundary;
mod zellij_ownership_boundary;
