//! Enforcement gate: PRODUCTION code must not aim a signal at a
//! process GROUP through a bare integer.
//!
//! `kill(-1, …)` is POSIX for "every process this user may signal",
//! `kill(0, …)` and `killpg(0, …)` are the caller's own group, and the
//! API puts all of them one digit away from "that one process tree".
//! Clank genuinely needs the tree — a command source spawns the user's
//! own command, which spawns its own children, and only a group signal
//! reaches the lot — so the capability cannot simply be removed. What
//! can be removed is the ability to SPELL the catastrophic cases.
//!
//! This exists because the `if pgid <= 1` that used to stand in for it
//! turned out to be deletable, and was deleted: a mutation run removed
//! it, the run was interrupted before restoring the file, and a unit
//! test written to prove the guard held — by calling
//! `signal_group(1, SIGKILL)` — ran `kill(-1, SIGKILL)` and took down a
//! developer's whole login session, twice in one day. A guard made of
//! a statement protects nothing from a process whose job is to delete
//! statements. A guard made of a TYPE has nothing to delete, and this
//! gate is what keeps the next one from being written as a statement.

use crate::common::{crates_dir, rs_files, strip_test_code};

/// What (if anything) a production line illegally does.
fn line_violation(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with("///") {
        return None;
    }
    // A negated integer expression as a signal target: the group form
    // of `kill`. Allowed only through a validated newtype's accessor.
    if (t.contains("libc::kill(-") || t.contains("kill(-")) && !t.contains(".get()") {
        return Some("a process GROUP signalled through a bare integer");
    }
    // `killpg` takes the group unnegated, so the shape is different and
    // the hazard is the same: 0 means the caller's own group.
    if t.contains("libc::killpg(") && !t.contains(".0") && !t.contains(".get()") {
        return Some("`killpg` on an unvalidated group");
    }
    // The subprocess spelling, `kill -9 -<pgid>`.
    if t.contains("format!(\"-{") && t.contains("}\")") && !t.contains("pgid") {
        return Some("a negated pid built for a `kill` subprocess");
    }
    None
}

#[test]
fn no_process_group_is_signalled_through_a_bare_integer() {
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
        let prod = strip_test_code(&std::fs::read_to_string(f).unwrap_or_default());
        for (i, line) in prod.lines().enumerate() {
            if let Some(what) = line_violation(line) {
                violations.push(format!("{}:{} — {what}", f.display(), i + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "a process group is signalled through an unvalidated number ({} site(s)). \
         Take a validated `Pgid` instead, whose constructor refuses the ids that \
         do not name one group — the number cannot be checked at the call site, \
         because the check is what gets deleted:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

#[test]
fn the_scanner_sees_the_shapes_it_claims_to() {
    let count = |src: &str| -> usize {
        strip_test_code(src)
            .lines()
            .filter(|l| line_violation(l).is_some())
            .count()
    };

    // The three spellings, unvalidated → flagged.
    assert_eq!(count("    unsafe { libc::kill(-pgid, sig) };\n"), 1);
    assert_eq!(count("    unsafe { libc::killpg(group, signal) };\n"), 1);
    assert_eq!(count("        .arg(format!(\"-{group}\"))\n"), 1);

    // Through a validated newtype → allowed.
    assert_eq!(count("    unsafe { libc::kill(-pgid.get(), sig) };\n"), 0);
    assert_eq!(count("    unsafe { libc::killpg(group.0, signal) };\n"), 0);
    assert_eq!(count("        .arg(format!(\"-{pgid}\"))\n"), 0);

    // A single-process signal is a different thing and not this gate's.
    assert_eq!(count("    unsafe { libc::kill(pid, libc::SIGTERM) };\n"), 0);
    // And prose about the hazard is not the hazard.
    assert_eq!(
        count("/// `kill(-1, …)` is every process this user may signal\n"),
        0
    );
}
