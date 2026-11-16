//! Gate (a-watch-is-a-typed-producer): the `watch:` text that names a
//! watch in selectors and records is spelled by ONE format and parse pair,
//! `watch_store::watch_text` and `watch_store::parse_watch_text`. Every
//! other production site works with the typed `Producer`, so an agent
//! whose label reads `watch:1` can never be taken for a watch.

use crate::common::{Production, crates_dir, rs_files, strip_test_code};

const PAIR_FILE: &str = "watch_store.rs";
const PAIR: &[&str] = &["fn watch_text(", "fn parse_watch_text("];

/// Whether `line` spells the watch prefix in code: `watch:` not part of a
/// longer word, followed by neither `:` (a `tree_watch::` path) nor
/// whitespace (prose such as `status watch: …`).
fn spells_prefix(line: &str) -> bool {
    let code = match line.trim_start().starts_with("//") {
        true => return false,
        false => line.split(" // ").next().unwrap_or(line),
    };
    code.match_indices("watch:").any(|(at, _)| {
        let before = code[..at].chars().next_back();
        let after = code[at + "watch:".len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c == ':' || c.is_whitespace())
    })
}

/// The line numbers (from 0) of `src` inside the pair's functions: from
/// each signature to the first line that closes it at column 0.
fn pair_lines(src: &str) -> std::collections::BTreeSet<usize> {
    let lines: Vec<&str> = src.lines().collect();
    let mut inside = std::collections::BTreeSet::new();
    for start in lines
        .iter()
        .enumerate()
        .filter(|(_, l)| PAIR.iter().any(|sig| l.contains(sig)))
        .map(|(i, _)| i)
    {
        for (i, line) in lines.iter().enumerate().skip(start) {
            inside.insert(i);
            if *line == "}" {
                break;
            }
        }
    }
    inside
}

fn violations(name: &str, prod: &str) -> Vec<usize> {
    let allowed = match name == PAIR_FILE {
        true => pair_lines(prod),
        false => Default::default(),
    };
    prod.lines()
        .enumerate()
        .filter(|(i, line)| spells_prefix(line) && !allowed.contains(i))
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn the_watch_prefix_is_spelled_only_by_its_format_and_parse_pair() {
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
    let production = Production::of(&files);
    let mut found = Vec::new();
    let mut pair_sites = 0;
    for f in &files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let prod = production.text(f);
        if name == PAIR_FILE {
            pair_sites += prod.lines().filter(|l| spells_prefix(l)).count();
        }
        found.extend(
            violations(name, &prod)
                .into_iter()
                .map(|line| format!("{}:{line}", f.display())),
        );
    }
    assert!(
        pair_sites > 0,
        "the pair no longer spells the prefix; this gate scans for the wrong text"
    );
    assert!(
        found.is_empty(),
        "the `watch:` prefix is spelled outside watch_store's watch_text/parse_watch_text \
         ({} site(s)); build or match it through that pair, or carry a Producer:\n  {}",
        found.len(),
        found.join("\n  ")
    );
}

#[test]
fn the_scanner_flags_a_second_site_and_spares_prose_paths_and_tests() {
    let src = r#"
pub(crate) fn watch_text(id: u64) -> String {
    format!("watch:{id}")
}

pub(crate) fn elsewhere(text: &str) -> bool {
    text.starts_with("watch:")
}

fn prose() {
    eprintln!("status watch: cannot read stdin");
    let _ = crate::proc_identity::tree_watch::KernelWatcher::new();
    // a comment may say watch:1
}

#[cfg(test)]
mod tests {
    fn fixture() -> &'static str {
        "watch:7:key"
    }
}
"#;
    let prod = strip_test_code(src);
    assert_eq!(violations(PAIR_FILE, &prod), [7]);
    assert_eq!(violations("events.rs", &prod), [3, 7]);
}
