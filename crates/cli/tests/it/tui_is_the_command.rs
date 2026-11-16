//! The public command is `tui`; retired spellings are parser errors, and
//! nothing a user or agent reads still names them. `clank status --tui`
//! and `clank web` are gone: a stray in source, tests or docs is one the
//! rename sweep missed, found here rather than one review at a time.
use clank::cli::command::Cli;
use clap::Parser;
use std::path::{Path, PathBuf};

#[test]
fn tui_parses_and_the_retired_commands_do_not() {
    assert!(Cli::try_parse_from(["clank", "tui"]).is_ok());
    for args in [vec!["clank", "status", "--tui"], vec!["clank", "web"]] {
        assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
    }
}

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

fn names_a_retired_command(line: &str) -> bool {
    line.contains("status --tui") || line.contains("\"--tui\"") || line.contains("clank web")
}

#[test]
fn source_and_docs_name_no_retired_command() {
    let repo = crate::common::crates_dir()
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let mut scanned = Vec::new();
    for dir in ["crates/cli/src", "crates/cli/tests", "docs", "book"] {
        files(&repo.join(dir), &mut scanned);
    }
    for name in ["README.md", "RELEASE-CHECKLIST.md"] {
        scanned.push(repo.join(name));
    }
    let mut strays = Vec::new();
    for path in &scanned {
        if path.ends_with("tests/it/tui_is_the_command.rs") {
            continue;
        }
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let rel = path.strip_prefix(&repo).unwrap_or(path).display();
        for (n, line) in text.lines().enumerate() {
            if names_a_retired_command(line) {
                strays.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        scanned.iter().any(|p| p.ends_with("crates/cli/src/lib.rs")),
        "the scan never reached the source"
    );
    assert!(
        strays.is_empty(),
        "the TUI is `clank tui`, and the remote is the TUI's:\n{}",
        strays.join("\n")
    );
}
