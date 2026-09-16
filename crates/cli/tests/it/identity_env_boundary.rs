//! Enforcement gate: PRODUCTION code outside `agent_env.rs` must not
//! read an identity variable out of the process environment.
//!
//! `agent_env.rs` owns the six variables that decide WHO an agent is
//! and hands every caller an `Ambient` to read them through. A read
//! anywhere else is ambient state: it cannot be stated by a fixture,
//! so a test that reaches it sees whatever the developer's shell
//! holds. Every agent running inside clank exports `CLANK_AGENT`,
//! which outranks every other signal, and four tests failed on this
//! machine and passed in CI for exactly that reason.
//!
//! Test code is EXEMPT: a fixture may name whatever it likes, because
//! naming it is the opposite of inheriting it.

use crate::common::{crates_dir, rs_files, strip_test_code};

/// The variables that decide identity. Kept as data, in the order
/// `SESSION_IDENTITY_VARS` lists them — a seventh variable added to
/// that list belongs here too.
const IDENTITY_VARS: &[&str] = &[
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "OPENCODE_SESSION_ID",
    "GROK_AGENT",
    "CLANK_AGENT",
    "CLANK_BOOTSTRAP_SESSION_ID",
];

/// The one file allowed to read them.
const OWNER: &str = "agent_env.rs";

/// Naming a variable is fine — the launcher scrubs them by name, and
/// `SESSION_IDENTITY_VARS` is that list. READING one is the
/// violation, and in this tree a read is `env::var` on the same line.
fn reads_the_environment(line: &str) -> bool {
    line.contains("env::var") || line.contains("env::vars")
}

#[test]
fn identity_is_read_only_where_it_can_be_stated() {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir())
        .expect("read crates/")
        .flatten()
    {
        let src = entry.path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "scanned no source files");

    let mut violations: Vec<String> = Vec::new();
    for f in &files {
        if f.file_name().is_some_and(|n| n == OWNER) {
            continue;
        }
        let prod = strip_test_code(&std::fs::read_to_string(f).unwrap_or_default());
        for (i, line) in prod.lines().enumerate() {
            if !reads_the_environment(line) {
                continue;
            }
            for var in IDENTITY_VARS {
                if line.contains(var) {
                    violations.push(format!("{}:{} — reads {var}", f.display(), i + 1));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "identity read from the process environment outside {OWNER} ({} site(s)). \
         Take an `Ambient` and read through it, so a test can state what the \
         binary reads:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}
