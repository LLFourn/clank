//! Regression tests for `role-reviewers-to-reviewer-rename`:
//! the user-facing CLI help text for every `--role` flag and
//! every default-value must use the canonical singular
//! `reviewer`, NOT the plural alias.
//!
//! Codex caught (on 7fb2822) that `clank agent add --help`,
//! `clank init --help`, `clank wfw --help`, and
//! `clank auto on --help` were still saying "reviewers".

use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn help_for(args: &[&str]) -> String {
    let mut cmd = Command::new(clank_bin());
    cmd.args(args).arg("--help");
    let out = cmd.output().expect("spawn clank --help");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn agent_add_help_uses_canonical_singular_role() {
    let help = help_for(&["agent", "add"]);
    assert!(
        help.contains("[default: reviewer]"),
        "agent add --help must show [default: reviewer]; got:\n{help}"
    );
    // The `Defaults to` docstring line itself.
    assert!(
        help.contains("Defaults to `reviewer`"),
        "agent add docstring must say `Defaults to \\`reviewer\\``; got:\n{help}"
    );
    // Plural must NOT appear as the default (allowed in alias
    // documentation, but not as the unqualified default).
    assert!(
        !help.contains("[default: reviewers]"),
        "agent add must NOT show [default: reviewers]; got:\n{help}"
    );
}

#[test]
fn init_help_uses_canonical_singular_role() {
    let help = help_for(&["init"]);
    assert!(
        help.contains("role = reviewer."),
        "init --help should describe default as `role = reviewer.`; got:\n{help}"
    );
    assert!(
        !help.contains("role = reviewers."),
        "init --help must NOT describe default as `role = reviewers.`; got:\n{help}"
    );
}

#[test]
fn wfw_help_uses_canonical_singular_role() {
    let help = help_for(&["wfw"]);
    assert!(
        help.contains("else `reviewer`"),
        "wfw --help should say `else \\`reviewer\\``; got:\n{help}"
    );
    assert!(
        !help.contains("else `reviewers`"),
        "wfw --help must NOT say `else \\`reviewers\\``; got:\n{help}"
    );
}

#[test]
fn auto_on_help_uses_canonical_singular_role() {
    let help = help_for(&["auto", "on"]);
    assert!(
        help.contains("`master` or `reviewer`"),
        "auto on --help should say `\\`master\\` or \\`reviewer\\``; got:\n{help}"
    );
    assert!(
        !help.contains("`master` or `reviewers`"),
        "auto on --help must NOT say `\\`master\\` or \\`reviewers\\``; got:\n{help}"
    );
}
