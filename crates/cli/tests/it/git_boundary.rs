//! Enforcement gate (gix-not-git-gate): PRODUCTION code outside the two
//! sanctioned git-access layers must not name `git` or `gix` directly.
//! `git_io.rs` owns reads, `git_plumbing.rs` owns mutations; the backend
//! (gix vs subprocess) is their implementation detail and every other
//! module calls their typed API. Test code is EXEMPT — fixtures
//! legitimately spawn `git` and open gix repos (the no-binary-spawning
//! rule bans spawning the *clank* binary, not git).
//!
//! This is the whole point of the centralization: a new git/gix use
//! anywhere else fails `cargo test`, so the boundary can't erode.

use crate::common::{crates_dir, rs_files, strip_test_code};

/// The only files allowed to name `git`/`gix` directly.
const LAYER_FILES: &[&str] = &["git_io.rs", "git_plumbing.rs"];

/// What (if anything) a production line illegally names.
fn line_violation(line: &str) -> Option<&'static str> {
    if line.contains("Command::new(\"git\")") {
        return Some("raw `git` subprocess");
    }
    // `use gix::…` and `gix::…` call sites both contain `gix::`.
    if line.contains("gix::") {
        return Some("direct `gix` usage");
    }
    // Bare crate import enabling unqualified `gix` types.
    let t = line.trim_start();
    if t.starts_with("use gix;") || t.starts_with("use gix ") {
        return Some("`gix` crate import");
    }
    None
}

#[test]
fn no_raw_git_or_gix_outside_layer() {
    let mut files = Vec::new();
    // Every crate's `src/` only — NOT `tests/`, which is exempt test code.
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
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if LAYER_FILES.contains(&name) {
            continue;
        }
        let prod = strip_test_code(&std::fs::read_to_string(f).unwrap_or_default());
        for (i, line) in prod.lines().enumerate() {
            if let Some(what) = line_violation(line) {
                violations.push(format!("{}:{} — {what}", f.display(), i + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "git/gix used outside the git_io/git_plumbing layer ({} site(s)). \
         Route through the layer's typed API; if genuinely unavoidable, add \
         the call inside the layer with a one-line gix-can't-do-X justification:\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
}

#[test]
fn scanner_flags_prod_and_exempts_test_code() {
    let count = |src: &str| -> usize {
        strip_test_code(src)
            .lines()
            .filter(|l| line_violation(l).is_some())
            .count()
    };

    // Production raw git / gix → flagged.
    assert_eq!(
        count("fn f() { std::process::Command::new(\"git\"); }\n"),
        1
    );
    assert_eq!(count("use gix::Repository;\n"), 1);
    assert_eq!(count("    let r = gix::open(p)?;\n"), 1);
    assert_eq!(count("use gix;\n"), 1);

    // The same inside a #[cfg(test)] module → exempt.
    let test_mod = "#[cfg(test)]\nmod tests {\n    fn f() { let _ = std::process::Command::new(\"git\"); gix::open(\".\"); }\n}\n";
    assert_eq!(count(test_mod), 0, "test module must be exempt");

    // A #[cfg(test)] fn is exempt; production AFTER it is still scanned.
    let after =
        "#[cfg(test)]\nfn helper() { gix::open(\".\"); }\nfn prod() { Command::new(\"git\"); }\n";
    assert_eq!(count(after), 1, "only the post-test production line counts");
}
