//! Enforcement gate: PRODUCTION code must not aim a signal at
//! anything except from a module that validates the target.
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
//!
//! It scanned for SPELLINGS at first — `kill(-`, `killpg(`, a negated
//! pid built for a subprocess — and that left the single-process form
//! uncovered while the hazard is the same one digit over: `kill(0, …)`
//! is the caller's own group and `kill(-1, …)` is everything. Two
//! production sites were sitting in that gap. A spelling list is a
//! guess about what the next person will write, so the gate now names
//! the modules that may signal AT ALL and refuses the rest.

use crate::common::{Production, crates_dir, rs_files, strip_test_code};

/// The files that may aim a signal, each owning a validated target
/// type whose constructor refuses the ids that do not name one target.
///
/// Two entries rather than one because they are separate crates: a
/// `pub(crate)` type cannot cross that boundary, so the rule is
/// enforced twice rather than shared.
const SIGNAL_MODULES: &[&str] = &["cli/src/signal.rs", "web-cloudflare/src/tunnel.rs"];

fn owns_a_signal_target(path: &std::path::Path) -> bool {
    let p = path.to_string_lossy().replace('\\', "/");
    SIGNAL_MODULES.iter().any(|m| p.ends_with(m))
}

/// Whether a line delivers or probes a signal through `libc`.
///
/// Deliberately not a spelling list: ANY mention from outside a
/// validating module is the violation, including signal 0, because a
/// probe that reads a bad id as an answer is how a caller decides a
/// group is empty when it asked about every process on the machine.
fn names_a_libc_signal(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") {
        return false;
    }
    t.contains("libc::kill(") || t.contains("libc::killpg(")
}

/// A negated pid built for a `kill` SUBPROCESS.
///
/// No module owns this one. There is no type on the far side of a
/// `Command`, so the only safe spelling is one formatted from an id a
/// newtype has already validated.
fn builds_a_subprocess_group(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("//") {
        return None;
    }
    (t.contains("format!(\"-{") && t.contains("}\")") && !t.contains("pgid"))
        .then_some("a negated pid built for a `kill` subprocess")
}

#[test]
fn no_signal_is_aimed_from_outside_a_validating_module() {
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
    let mut signal_modules_seen = 0;
    let production = Production::of(&files);
    for f in &files {
        let owns = owns_a_signal_target(f);
        signal_modules_seen += usize::from(owns);
        let prod = production.text(f);
        for (i, line) in prod.lines().enumerate() {
            if !owns && names_a_libc_signal(line) {
                violations.push(format!(
                    "{}:{} — a signal aimed from outside a validating module",
                    f.display(),
                    i + 1
                ));
            }
            if let Some(what) = builds_a_subprocess_group(line) {
                violations.push(format!("{}:{} — {what}", f.display(), i + 1));
            }
        }
    }
    assert_eq!(
        signal_modules_seen,
        SIGNAL_MODULES.len(),
        "a module named in SIGNAL_MODULES was not scanned, so its exemption is          unearned and every other file's violation would be reported against a          rule nothing enforces: {SIGNAL_MODULES:?}"
    );

    assert!(
        violations.is_empty(),
        "a signal is aimed from outside a validating module ({} site(s)). \
         Go through `crate::signal`, whose `Pid` and `Pgid` constructors refuse \
         the ids that do not name one target — the number cannot be checked at \
         the call site, because the check is what gets deleted:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

#[test]
fn the_scanner_sees_what_it_claims_to() {
    let signals = |src: &str| -> usize {
        strip_test_code(src)
            .lines()
            .filter(|l| names_a_libc_signal(l))
            .count()
    };

    // Every form, including the one a spelling list missed: validated
    // or not, a call from outside a signal module is the violation.
    assert_eq!(signals("    unsafe { libc::kill(-pgid, sig) };\n"), 1);
    assert_eq!(signals("    unsafe { libc::killpg(group, signal) };\n"), 1);
    assert_eq!(
        signals("    unsafe { libc::kill(pid, libc::SIGTERM) };\n"),
        1
    );
    assert_eq!(
        signals("    unsafe { libc::kill(-pgid.get(), sig) };\n"),
        1,
        "an accessor is not an exemption: the module is"
    );
    assert_eq!(
        signals("    let rc = unsafe { libc::kill(pid, 0) };\n"),
        1,
        "a probe too — a bad id there is a wrong answer about a group"
    );

    // Prose about the hazard is not the hazard.
    assert_eq!(
        signals("// `kill(-1, …)` is every process this user may signal\n"),
        0
    );

    // The subprocess spelling has no owning module.
    let subprocess = |src: &str| -> usize {
        strip_test_code(src)
            .lines()
            .filter(|l| builds_a_subprocess_group(l).is_some())
            .count()
    };
    assert_eq!(subprocess("        .arg(format!(\"-{group}\"))\n"), 1);
    assert_eq!(subprocess("        .arg(format!(\"-{pgid}\"))\n"), 0);

    // And the exemption is by PATH, so it cannot be claimed by a file
    // that merely has a similar name.
    assert!(owns_a_signal_target(std::path::Path::new(
        "/x/crates/cli/src/signal.rs"
    )));
    assert!(!owns_a_signal_target(std::path::Path::new(
        "/x/crates/cli/src/cli/signal.rs"
    )));
    assert!(!owns_a_signal_target(std::path::Path::new(
        "/x/crates/other/src/signal.rs"
    )));
}
