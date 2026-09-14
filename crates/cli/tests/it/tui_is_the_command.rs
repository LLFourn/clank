//! Enforcement gate: the TUI is `clank tui`. `clank status --tui`
//! stays one release as an alias that says so, and the panes of
//! sessions opened before the rename still run the old command — so
//! the alias, the message, and the pane matcher's two spellings are
//! the ONLY places the old name may live. Anything else naming it is
//! a stray the rename sweep missed, found here rather than one review
//! at a time (clank-tui-runs-the-remote-in-process). `clank web` is
//! gone as a command and may be named nowhere.
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs" || x == "md") {
            out.push(p);
        }
    }
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Files that may still spell the old name, and why.
fn may_name_status_tui(rel: &str) -> bool {
    // The alias itself and its flag.
    rel.ends_with("cli/status.rs")
        || rel.ends_with("cli/mod.rs")
        // The pane matcher accepts both spellings; its fixtures use the old one.
        || rel.ends_with("cli/open_zellij.rs")
        || rel.ends_with("status_tui/zellij.rs")
        // This gate.
        || rel.ends_with("tui_is_the_command.rs")
}

#[test]
fn the_old_name_lives_only_in_the_alias_and_the_matcher() {
    let root = crate_root();
    let mut scanned = Vec::new();
    files(&root.join("src"), &mut scanned);
    files(&root.join("tests"), &mut scanned);
    let repo = root
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    for name in ["README.md", "RELEASE-CHECKLIST.md"] {
        scanned.push(repo.join(name));
    }
    let mut strays = Vec::new();
    for path in &scanned {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(repo)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        for (n, line) in text.lines().enumerate() {
            let names_old = line.contains("status --tui") || line.contains("\"--tui\"");
            if names_old && !may_name_status_tui(&rel) {
                strays.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
            if line.contains("clank web") && !rel.ends_with("tui_is_the_command.rs") {
                strays.push(format!(
                    "{rel}:{}: names `clank web`: {}",
                    n + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        strays.is_empty(),
        "the TUI is `clank tui`, and the remote is the TUI's:\n{}",
        strays.join("\n")
    );
}
