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

use crate::common::{Production, crates_dir, rs_files};

/// The variables that decide identity — the production list itself,
/// not a copy of it.
///
/// It WAS a copy, with a comment asking whoever added a variable to
/// add it here too. `GROK_SESSION_ID` was added to the production
/// list and not here, and since this gate only guards what it lists,
/// it quietly stopped guarding reads of the one variable that had
/// just been added. A list whose correctness depends on someone
/// remembering is not a gate.
use clank::agent_env::SESSION_IDENTITY_VARS as IDENTITY_VARS;

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
    let production = Production::of(&files);
    for f in &files {
        if f.file_name().is_some_and(|n| n == OWNER) {
            continue;
        }
        let prod = production.text(f);
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
