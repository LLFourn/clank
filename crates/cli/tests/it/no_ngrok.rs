//! Enforcement gate: ngrok is gone, and it stays gone.
//!
//! It asked for an account before anything worked, and clank asked for
//! a reserved domain on top of that, which was our demand rather than
//! ngrok's. The accountless tunnel needs neither, and the `command`
//! provider runs ngrok's own agent for anyone who wants it — so clank
//! does not have to model it (ngrok-asks-too-much).
//!
//! One mention survives on purpose: the retired variant and the
//! refusal that names both replacements. A config still naming ngrok
//! has to PARSE, because `read_user_config` is shared with the roster
//! and team commands and a variant removed from the enum would take
//! `clank team` down with the tunnel.

use std::path::{Path, PathBuf};

/// Using the SDK, as opposed to SAYING the word. Prose may name it —
/// the retired variant explains itself, and a test may describe what
/// it stands in for — but nothing may call it.
const USES: &[&str] = &[
    "ngrok::",
    "use ngrok",
    "NGROK_AUTHTOKEN",
    "extern crate ngrok",
];

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
fn nothing_calls_the_ngrok_sdk() {
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

    let mut violations = Vec::new();
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        for (i, line) in src.lines().enumerate() {
            // A doc comment naming it is prose; a call is not.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if let Some(hit) = USES.iter().find(|u| line.contains(**u)) {
                violations.push(format!(
                    "{}:{}: {hit} in {}",
                    f.display(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "ngrok is retired; `command` runs its agent for anyone who wants it:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_crate_does_not_depend_on_ngrok() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read crates/cli/Cargo.toml");
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        assert!(
            !line.to_ascii_lowercase().starts_with("ngrok"),
            "the ngrok SDK is back in the manifest: {line}"
        );
    }

    // And with it, the family of crates nothing else in the tree uses.
    let lock = std::fs::read_to_string(crates_dir().parent().unwrap().join("Cargo.lock"))
        .expect("read Cargo.lock");
    for gone in [
        "name = \"ngrok\"",
        "name = \"muxado\"",
        "name = \"awaitdrop\"",
    ] {
        assert!(!lock.contains(gone), "still locked: {gone}");
    }
}

/// The README must not offer the retired PROVIDER, and must document
/// the migration — which names ngrok, legitimately. Banning the word
/// would reject the documentation the removal owes its users.
#[test]
fn the_readme_retires_the_provider_and_documents_the_migration() {
    let readme = std::fs::read_to_string(crates_dir().parent().unwrap().join("README.md"))
        .expect("read README.md");
    assert!(
        !readme.contains(r#""provider": "ngrok""#),
        "the README still offers the retired provider"
    );
    assert!(
        readme.contains(r#"{"provider": "quick"}"#),
        "and still offers the accountless one"
    );
    // The migration, and the reason its `url` is load-bearing.
    assert!(
        readme.contains(r#""run": ["ngrok", "http", "--url""#),
        "the README does not show how to keep ngrok through `command`"
    );
    assert!(
        readme.contains("terminal UI rather than to stdout"),
        "nor why the recipe must claim its endpoint"
    );
}
