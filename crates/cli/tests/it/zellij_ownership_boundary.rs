//! Enforcement gate: panes have ONE owner.
//!
//! The status TUI's reconciler observes the roster and moves panes to
//! match. An action that also drove panes would be a second owner, and
//! the reconciler would destroy what it created — so `agent swap`,
//! `agent add` and friends are pure config writes and the pane work
//! follows from the roster. `open_zellij.rs` implements the pane
//! operations; `status_tui/zellij.rs` is the only caller, inside its
//! `PaneIo` impl.
//!
//! A runtime test cannot enforce this. `apply_swap` takes no `PaneIo`,
//! so asserting that a fake io stayed empty is tautological — it
//! watches a capability the action never had (codex on 5b8988f). The
//! real risk is code reaching AROUND the seam: spawning zellij
//! directly, or calling the pane helpers, both of which are reachable
//! crate-wide. That is a source-level property, so this is a
//! source-level gate.
//!
//! Test code is NOT exempt, and `crates/*/tests` IS scanned: a leaked
//! zellij server from a test once congested the machine, so a spawn
//! there is a bug too. This differs from `git_boundary`, where
//! fixtures may legitimately spawn `git`.
//!
//! The raw `$ZELLIJ` read is banned outside the owners for the same
//! reason as the spawn: it is the other way to make a zellij decision
//! without the owner knowing. `open_zellij::in_session` is the one
//! definition of "inside zellij" (zellij-is-the-workspace).
//!
//! Comments are not scanned, matching `zellij_cost_boundary` — a gate
//! that deletes the rationale for not calling something teaches
//! nothing.

use std::path::{Path, PathBuf};

/// The ONE file that may start a `zellij` process or read `$ZELLIJ`.
const SPAWNER: &str = "cli/open_zellij.rs";

/// May CALL the pane-mutation helpers: the spawner that implements
/// them, and the reconciler that is their sole legitimate caller. The
/// reconciler is not a spawner — it reaches zellij only through the
/// spawner's typed API (zellij-is-the-workspace).
const MUTATION_CALLERS: &[&str] = &[SPAWNER, "cli/status_tui/zellij.rs"];

/// Pane MUTATIONS. Reads (`snapshot_panes`, `agent_pane_pairs`,
/// `reviewers_are_stacked`, …) are fine anywhere — observing panes
/// creates no second owner. Only changing them does.
const PANE_MUTATIONS: &[&str] = &[
    "focus_pane",
    "add_reviewer_pane",
    "stack_reviewer_panes",
    "remove_reviewer_panes",
    "relocate_for_promote",
];

/// Whether a line starts a zellij process or reads its environment —
/// legal in the spawner only.
fn spawn_violation(line: &str) -> Option<String> {
    let code = line.split("//").next().unwrap_or("");
    if code.contains(r#"Command::new("zellij")"#) {
        return Some("raw `zellij` subprocess".to_string());
    }
    // "Am I inside zellij" has ONE definition, `open_zellij::
    // in_session`. A raw read elsewhere is a second one that can
    // drift from it — and it is how callers reached around the seam
    // to make zellij decisions the owner never saw
    // (zellij-is-the-workspace).
    if code.contains(r#"var_os("ZELLIJ")"#) || code.contains(r#"var("ZELLIJ")"#) {
        return Some("raw `$ZELLIJ` read".to_string());
    }
    None
}

/// Whether a line calls a pane MUTATION — legal in the spawner and
/// the reconciler only.
fn mutation_violation(line: &str) -> Option<String> {
    let code = line.split("//").next().unwrap_or("");
    PANE_MUTATIONS
        .iter()
        .find(|op| code.contains(*op))
        .map(|op| format!("pane mutation `{op}`"))
}

/// What (if anything) a line illegally names, for a file that is
/// neither spawner nor reconciler.
fn line_violation(line: &str) -> Option<String> {
    spawn_violation(line).or_else(|| mutation_violation(line))
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .to_path_buf()
}

/// An enforcement gate has to name what it bans, as data.
fn is_gate(path: &str) -> bool {
    path.ends_with("_boundary.rs")
}

#[test]
fn only_the_pane_io_modules_operate_on_panes() {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir())
        .expect("read crates/")
        .flatten()
    {
        for sub in ["src", "tests"] {
            let dir = entry.path().join(sub);
            if dir.is_dir() {
                rs_files(&dir, &mut files);
            }
        }
    }
    assert!(!files.is_empty(), "scanned no source files");
    let scanned_tests = files
        .iter()
        .any(|f| f.to_string_lossy().contains("/tests/"));
    assert!(scanned_tests, "the tests/ tree must be in scope");

    let mut violations: Vec<String> = Vec::new();
    for f in &files {
        let unix = f.to_string_lossy().replace('\\', "/");
        if unix.ends_with(SPAWNER) || is_gate(&unix) {
            continue;
        }
        // The reconciler may mutate panes but may not spawn: its only
        // route to zellij is the spawner's typed API.
        let check: fn(&str) -> Option<String> =
            if MUTATION_CALLERS.iter().any(|o| unix.ends_with(o)) {
                spawn_violation
            } else {
                line_violation
            };
        let body = std::fs::read_to_string(f).unwrap_or_default();
        for (i, line) in body.lines().enumerate() {
            if let Some(what) = check(line) {
                violations.push(format!("{}:{} — {what}", f.display(), i + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "pane operation(s) outside the pane-IO modules ({} site(s)). \
         Panes have ONE owner: the status TUI reconciler, driven by the \
         roster. An action that opens, closes, moves or focuses a pane \
         directly races that reconciler, which will then undo it. Write \
         the roster and let convergence follow:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

/// The gate's own scanner, on synthetic input — the first draft of this
/// file latched on the first `#[cfg(test)]` and silently skipped every
/// line after it, passing while scanning almost nothing.
#[test]
fn scanner_catches_both_ways_around_the_seam() {
    assert!(
        line_violation(r#"    let _ = std::process::Command::new("zellij").arg("action");"#)
            .is_some(),
        "a raw spawn is a violation"
    );
    assert!(
        line_violation("    crate::cli::open_zellij::add_reviewer_pane(repo, label);").is_some(),
        "calling the helper reaches around the seam just as effectively"
    );
    assert!(
        line_violation(r#"    // we deliberately do NOT Command::new("zellij") here"#).is_none(),
        "rationale in a comment is not a call"
    );
    assert!(
        line_violation(r#"    let inside = std::env::var_os("ZELLIJ").is_some();"#).is_some(),
        "a raw $ZELLIJ read is the third way around the seam"
    );
    assert!(
        line_violation("    let inside = open_zellij::in_session();").is_none(),
        "asking the owner is the sanctioned form"
    );
    assert!(
        line_violation("    let panes = open_zellij::snapshot_panes();").is_none(),
        "reading panes creates no second owner"
    );
    assert!(is_gate("crates/cli/tests/zellij_ownership_boundary.rs"));
    assert!(!is_gate("crates/cli/src/cli/status_tui/mod.rs"));
}
