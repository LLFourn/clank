//! Enforcement gate: `book/reference/` is exactly what `clank docs cli`
//! writes from the current clap tree, so a help change cannot land
//! without its pages, and the commit that carries them touches `book/`,
//! which is what rebuilds the site.

use clank::cli::command::Cli;
use clank::cli::docs::{reference_pages, write_reference};
use clap::CommandFactory;
use std::collections::BTreeMap;
use std::path::Path;

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&p).unwrap())
        })
        .collect()
}

/// The names whose bytes differ between `want` and `have`, either way.
fn differences(want: &BTreeMap<String, Vec<u8>>, have: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut names: Vec<&String> = want.keys().chain(have.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|n| match (want.get(n), have.get(n)) {
            (Some(_), None) => Some(format!("{n}: missing")),
            (None, Some(_)) => Some(format!("{n}: no command writes it")),
            (Some(a), Some(b)) if a != b => Some(format!("{n}: stale")),
            _ => None,
        })
        .collect()
}

#[test]
fn the_checked_in_reference_is_fresh() {
    let checked_in = crate::common::crates_dir()
        .parent()
        .expect("crates/ has a parent")
        .join("book/reference");
    let fresh = tempfile::tempdir().unwrap();
    write_reference(fresh.path(), &reference_pages(Cli::command())).unwrap();
    let bad = differences(&files(fresh.path()), &files(&checked_in));
    assert!(
        bad.is_empty(),
        "book/reference is not what the CLI says; run \
         `cargo run -p clank -- docs cli book/reference` and commit it:\n{}",
        bad.join("\n")
    );
}

#[test]
fn a_changed_extra_or_missing_page_is_a_difference() {
    let page = |s: &str| s.as_bytes().to_vec();
    let want = BTreeMap::from([
        ("a.html".to_string(), page("a")),
        ("b.html".to_string(), page("b")),
    ]);
    assert!(differences(&want, &want.clone()).is_empty());
    let have = BTreeMap::from([
        ("a.html".to_string(), page("a, edited")),
        ("c.html".to_string(), page("c")),
    ]);
    assert_eq!(
        differences(&want, &have),
        [
            "a.html: stale",
            "b.html: missing",
            "c.html: no command writes it"
        ]
    );
}
