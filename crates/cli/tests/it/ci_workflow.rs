//! Enforcement gate: CI runs the tests with no agent identity in its
//! environment, never opts out of the Node harness, and checks every
//! dependency's licence and that the notice the binary ships is current.
//!
//! YAML cannot read the Rust list, so the workflow keeps its own copy
//! of the identity variables. This holds the copy to the production
//! list: a variable added to `SESSION_IDENTITY_VARS` and not to CI
//! would reach CI's test runs and fail ownership checks there only
//! when some runner happened to set it.

use crate::common::crates_dir;
use clank::agent_env::SESSION_IDENTITY_VARS;
use std::collections::BTreeSet;

const UNSET: &str = "unset $SESSION_IDENTITY_VARS;";

fn workflow() -> String {
    let path = crates_dir()
        .parent()
        .expect("crates/ has a parent")
        .join(".github/workflows/ci.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn every_test_step_unsets_every_identity_variable() {
    let text = workflow();
    let defined: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("SESSION_IDENTITY_VARS:"))
        .collect();
    let [defined] = defined[..] else {
        panic!("expected one SESSION_IDENTITY_VARS definition, found {defined:?}");
    };
    let listed: BTreeSet<&str> = defined.split_whitespace().collect();
    let production: BTreeSet<&str> = SESSION_IDENTITY_VARS.iter().copied().collect();
    assert_eq!(listed, production);

    let runs: Vec<&str> = text.lines().filter(|l| l.contains("cargo test")).collect();
    assert!(!runs.is_empty(), "the workflow runs no tests");
    for line in runs {
        let command = line.trim_start().trim_start_matches("run:").trim_start();
        assert!(
            command.starts_with(UNSET),
            "this test step keeps the runner's identity variables: {line}"
        );
    }
}

#[test]
fn the_workflow_never_skips_the_node_harness() {
    assert!(!workflow().contains("CLANK_SKIP_NODE_HARNESS"));
}

fn has_step(text: &str, run: &str) -> bool {
    text.lines()
        .any(|l| l.trim().strip_prefix("run:").map(str::trim) == Some(run))
}

#[test]
fn ci_checks_dependency_licences_and_their_notice() {
    let text = workflow();
    assert!(
        has_step(&text, "cargo deny --locked check licenses"),
        "no cargo deny licence step"
    );
    assert!(
        has_step(&text, "scripts/notices/check.sh"),
        "no notice freshness step"
    );
    assert!(
        text.contains(
            r#"cargo install cargo-about --locked --features cli --version "$(cat scripts/notices/cargo-about.version)""#
        ),
        "cargo-about is not installed at the version the notice was generated with"
    );
}
