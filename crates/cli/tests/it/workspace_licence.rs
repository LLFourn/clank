//! Every crate in the workspace is 0BSD, the published `clank` package
//! carries its licence and notices, and the two licence gates
//! (cargo-deny in CI, cargo-about for the shipped notice) admit the same
//! dependency licences.

use crate::common::crates_dir;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn root() -> PathBuf {
    crates_dir()
        .parent()
        .expect("crates/ has a parent")
        .to_path_buf()
}

#[test]
fn every_workspace_crate_declares_0bsd() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .arg("--manifest-path")
        .arg(root().join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let members = meta["packages"].as_array().unwrap();
    assert_eq!(members.len(), 5, "a workspace crate was added or removed");
    for p in members {
        let name = p["name"].as_str().unwrap();
        assert_eq!(p["license"], "0BSD", "{name}");
        assert_eq!(
            p["repository"], "https://github.com/LLFourn/clank",
            "{name}"
        );
        assert!(
            p["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{name}"
        );
    }
}

#[test]
fn the_clank_package_carries_its_licence_and_notices() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .args([
            "package",
            "-p",
            "clank",
            "--list",
            "--offline",
            "--allow-dirty",
        ])
        .arg("--manifest-path")
        .arg(root().join("Cargo.toml"))
        .output()
        .expect("run cargo package");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: BTreeSet<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    for file in [
        "LICENSE",
        "THIRD-PARTY-NOTICES.md",
        "notices/rust-dependencies.txt",
    ] {
        assert!(listed.contains(file), "{file} is not packaged");
    }
}

fn quoted_list(file: &str, key: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(root().join(file)).unwrap();
    let start = text
        .find(&format!("\n{key} = ["))
        .unwrap_or_else(|| panic!("no {key} list in {file}"));
    let list = &text[start..];
    let list = &list[..list.find("\n]").unwrap()];
    list.lines()
        .filter_map(|l| l.trim().strip_prefix('"')?.strip_suffix("\","))
        .map(str::to_string)
        .collect()
}

#[test]
fn deny_and_about_admit_the_same_licences() {
    let deny = quoted_list("deny.toml", "allow");
    assert!(deny.contains("MPL-2.0"), "{deny:?}");
    assert_eq!(deny, quoted_list("scripts/notices/about.toml", "accepted"));
}
