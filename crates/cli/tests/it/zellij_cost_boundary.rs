//! Enforcement gate: PRODUCTION code must not invoke a zellij action
//! that routes through the server's `populate_session_layout_metadata`.
//!
//! That function resolves every pane's process cwd and command line —
//! `get_cwds` via sysinfo, and `get_all_cmds_by_ppid`, which SHELLS OUT
//! to `ps -ao ppid,args`. Measured on a 1308-process machine:
//! `ps -ao ppid,args` alone 1071 ms, `list-clients` 1035–1041 ms (±3 ms
//! over six runs), against ~30 ms for `list-panes --json` and 34 ms for
//! `current-tab-info`.
//!
//! The cost tracks processes on the MACHINE, not panes in the session,
//! so it grows with every agent started anywhere — and it is paid on
//! the reconcile path, where it widens the window in which a second
//! driver can double-open a pane.
//!
//! Comments are NOT scanned, deliberately. Three of the sites this gate
//! first flagged were rationale for NOT calling these actions, and a
//! gate that deletes its own explanation teaches nothing. An actual
//! call has to name the action as a string literal, which is what is
//! matched.

use std::path::{Path, PathBuf};

/// Actions served by `populate_session_layout_metadata`.
const EXPENSIVE_ACTIONS: &[&str] = &["list-clients", "dump-layout"];

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

#[test]
fn no_process_enumerating_zellij_actions_in_production() {
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
        let body = std::fs::read_to_string(f).unwrap_or_default();
        for (i, line) in body.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for action in EXPENSIVE_ACTIONS {
                if code.contains(&format!("\"{action}\"")) {
                    violations.push(format!("{}:{} — `{action}`", f.display(), i + 1));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "zellij action(s) that enumerate every process on the machine, \
         on a path that must stay cheap ({} site(s)). `list-panes --json` \
         carries pane state including `is_focused` / `exited` / \
         `exit_status`, and `current-tab-info` names the active tab; \
         between them they answer what these were used for, for ~1/30th \
         the cost:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}
