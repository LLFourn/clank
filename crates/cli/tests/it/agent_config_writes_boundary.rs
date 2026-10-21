//! Enforcement gate: every write of `.clank/agents/<label>/config.json`
//! goes through `agent_store`'s locked transaction.
//!
//! The file had about seventy writers, each loading the whole config,
//! editing it and saving it back with no lock. Two overlapping writers
//! meant the later one put back the earlier one's stale view, and an
//! `auto_mode: "off"` the user had just set came back `"on"` minutes
//! later. `agent_store` now holds the label's `binding.lock` from the
//! load to the rename, and this gate keeps the bypasses closed:
//!
//! - `save_agent_config` is named nowhere outside `agent_store`, test
//!   code included. It is private, so the compiler already refuses a
//!   call; the gate also refuses a comment or a re-export that reads
//!   as permission to bring a public one back.
//! - Production code outside `agent_store` does not spell an agent
//!   config's path at all, by `agent_config_path(`, a bare
//!   `"config.json"`, or a literal ending in it other than the repo's own
//!   `.clank/config.json`, except at the read-only sites declared below.
//!   Forbidding the spelling rather than the write is deliberate: a path
//!   bound to a variable and written three lines later is the shape a
//!   write-shaped pattern misses.

use crate::common::{Production, crates_dir, rs_files};

const STORE: &str = "agent_store.rs";

/// Production sites that spell a config path without writing it, as
/// (path under `crates/`, the trimmed line, why it is not a write).
const READ_ONLY_SITES: &[(&str, &str, &str)] = &[
    (
        "cli/src/repo_watch.rs",
        "\"config.json\",",
        "the REPO's `.clank/config.json` as a name the watcher wakes on, \
         not an agent's config, and never written",
    ),
    (
        "cli/src/cli/doctor.rs",
        "agent_config_path(repo, label).display(),",
        "names the file in a diagnostic",
    ),
];

fn spells_agent_config_path(line: &str) -> bool {
    line.contains("agent_config_path(") || config_literals(line).any(names_an_agent_config)
}

/// The contents of each string literal on the line that ends in
/// `config.json`.
fn config_literals(line: &str) -> impl Iterator<Item = &str> {
    line.match_indices("config.json\"")
        .filter_map(move |(end, _)| {
            let end = end + "config.json".len();
            let start = line[..end].rfind('"')? + 1;
            Some(&line[start..end])
        })
}

/// Every `config.json` but the repo's own: the repo-level file sits
/// directly under `.clank/`, and an agent's sits under `.clank/agents/`.
fn names_an_agent_config(literal: &str) -> bool {
    literal.contains("agents/") || !literal.ends_with(".clank/config.json")
}

fn all_sources() -> Vec<std::path::PathBuf> {
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
    files
}

fn is(f: &std::path::Path, name: &str) -> bool {
    f.file_name().and_then(|n| n.to_str()) == Some(name)
}

fn under_crates(f: &std::path::Path) -> String {
    f.strip_prefix(crates_dir())
        .unwrap_or(f)
        .to_string_lossy()
        .into_owned()
}

#[test]
fn nothing_outside_the_store_names_save_agent_config() {
    let violations: Vec<String> = all_sources()
        .iter()
        .filter(|f| !is(f, STORE) && !is(f, "agent_config_writes_boundary.rs"))
        .flat_map(|f| {
            let body = std::fs::read_to_string(f).unwrap_or_default();
            body.lines()
                .enumerate()
                .filter(|(_, l)| l.contains("save_agent_config"))
                .map(|(i, _)| format!("{}:{}", under_crates(f), i + 1))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        violations.is_empty(),
        "`save_agent_config` named outside agent_store. Edit the fields you \
         mean to change with `agent_store::update_agent_config`:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn production_spells_an_agent_config_path_only_where_declared() {
    let files: Vec<_> = all_sources()
        .into_iter()
        .filter(|f| under_crates(f).split('/').nth(1) == Some("src"))
        .collect();
    let production = Production::of(&files);
    let mut seen = vec![false; READ_ONLY_SITES.len()];
    let mut violations = Vec::new();
    for f in files.iter().filter(|f| !is(f, STORE)) {
        let rel = under_crates(f);
        for (i, line) in production.text(f).lines().enumerate() {
            if !spells_agent_config_path(line) {
                continue;
            }
            match READ_ONLY_SITES
                .iter()
                .position(|(file, text, _)| *file == rel && *text == line.trim())
            {
                Some(at) => seen[at] = true,
                None => violations.push(format!("{rel}:{} — {}", i + 1, line.trim())),
            }
        }
    }
    assert!(
        violations.is_empty(),
        "production code outside agent_store spells an agent config path. \
         Write through `agent_store::update_agent_config`; a read-only use \
         must be declared in READ_ONLY_SITES with its reason:\n  {}",
        violations.join("\n  ")
    );
    let stale: Vec<_> = READ_ONLY_SITES
        .iter()
        .zip(&seen)
        .filter(|(_, seen)| !**seen)
        .map(|((file, text, _), _)| format!("{file}: {text}"))
        .collect();
    assert!(
        stale.is_empty(),
        "declared sites no longer exist; remove them:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn the_path_spellings_are_recognized() {
    assert!(spells_agent_config_path(
        "std::fs::write(dir.join(\"config.json\"), body)?;"
    ));
    assert!(spells_agent_config_path(
        "let p = agent_store::agent_config_path(repo, label);"
    ));
    assert!(spells_agent_config_path(
        "std::fs::write(repo.join(\".clank/agents/claude/config.json\"), body)?;"
    ));
    assert!(spells_agent_config_path(
        "let p = format!(\"{root}/agents/{label}/config.json\");"
    ));
    assert!(!spells_agent_config_path(
        "write_typed_config(&repo.join(\".clank/config.json\"), &file)"
    ));
    assert!(!spells_agent_config_path(
        "use crate::agent_store::{agent_config_path, agents_root};"
    ));
}
