//! Enforcement gate: a `///` run must document the item under it.
//!
//! Consecutive `///` runs separated by blank lines all attach to the
//! next item, so deleting a function and leaving its docstring is
//! silent — the prose migrates onto whatever follows and compiles.
//! `80f01612` removed the wait generation, claim and turn-end state
//! and left 21 such blocks across five files, which is how
//! `stop_hook.rs` came to describe `hook_held`, `wait.lock` probing
//! and `reachability` long after all three were gone, and how one
//! function ended up carrying six docstrings, five of them for tests
//! that no longer existed.
//!
//! An audit fixes that once. This makes it fail the build.

use crate::common::{crates_dir, rs_files};

/// Every `///` run that is not immediately followed by its item.
///
/// Attributes count as "immediately": `#[test]`, `#[derive(..)]` and
/// friends sit between a doc comment and the thing it documents. A
/// BLANK line does not, and neither does another `///` run — those
/// two are the shapes a deleted item leaves behind.
///
/// A private `use` counts as no item either. It is syntactically
/// documentable, so the compiler and the blank-line rule both accept
/// it, and it is exactly where a deleted test's docstring lands: at
/// the top of a `mod tests`, on `use super::*`. A `pub use` is a real
/// re-export and may carry documentation.
///
/// `//!` is deliberately not checked: an inner doc documents the file
/// and is legitimately followed by blank lines and `use` statements.
fn orphan_doc_lines(source: &str) -> Vec<usize> {
    let lines: Vec<&str> = source.lines().collect();
    let is_doc = |l: &str| l.trim_start().starts_with("///");
    let mut orphans = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_doc(lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && is_doc(lines[i]) {
            i += 1;
        }
        let mut item = i;
        while lines
            .get(item)
            .is_some_and(|l| l.trim_start().starts_with("#["))
        {
            item += 1;
        }
        // End of file, a blank line, the start of another run, or a
        // private import: in each the item this run describes is not
        // there.
        match lines.get(item) {
            None => orphans.push(start + 1),
            Some(next) if next.trim().is_empty() => orphans.push(start + 1),
            Some(next) if next.trim_start().starts_with("use ") => orphans.push(start + 1),
            Some(_) => {}
        }
    }
    orphans
}

#[test]
fn no_orphan_doc_comments() {
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
        // NOT `unwrap_or_default`: an unreadable file would scan as
        // an empty one, contribute no violations, and be counted as
        // covered — a gate that reports success for files it never
        // read is the failure it exists to prevent.
        let body = std::fs::read_to_string(f)
            .unwrap_or_else(|e| panic!("cannot read {} for scanning: {e}", f.display()));
        for line in orphan_doc_lines(&body) {
            violations.push(format!("{}:{line}", f.display()));
        }
    }

    assert!(
        violations.is_empty(),
        "doc comment with no item under it ({} site(s)). The item it \
         described is gone, or it drifted off the one it belongs to: \
         delete it, or move it back onto its item. Rust attaches it to \
         whatever follows, so nothing else will tell you:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

/// The scanner itself, since the gate above is only as good as this.
///
/// The accepted shapes matter as much as the rejected ones: a gate
/// that flagged `#[test]` would be turned off within a day.
#[test]
fn orphan_scanner_flags_only_detached_docs() {
    let attached = [
        "/// doc\nfn f() {}\n",
        "/// doc\n#[test]\nfn f() {}\n",
        "/// doc\n#[derive(Debug)]\n#[serde(default)]\nstruct S;\n",
        "/// doc\n// an ordinary comment\nfn f() {}\n",
        "//! inner doc\n\nuse std::fs;\n",
        "/// one\nfn a() {}\n\n/// two\nfn b() {}\n",
        "/// a documented re-export\npub use inner::Thing;\n",
    ];
    for src in attached {
        assert!(
            orphan_doc_lines(src).is_empty(),
            "flagged an attached doc: {src:?}"
        );
    }

    let detached = [
        ("/// doc\n\nfn f() {}\n", 1),
        ("/// gone\n\n/// kept\nfn f() {}\n", 1),
        ("fn f() {}\n\n/// trailing\n", 3),
        ("mod tests {\n    /// gone\n    use super::*;\n}\n", 2),
    ];
    for (src, line) in detached {
        assert_eq!(
            orphan_doc_lines(src),
            vec![line],
            "missed a detached doc: {src:?}"
        );
    }
}
