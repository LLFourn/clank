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

/// Cargo reports `source: null` for every path or patched crate, not only
/// the workspace's own. The notice generator skips exactly the workspace's
/// packages, by Cargo package id, and refuses any other such package, a
/// path dependency sharing a member's name included, so a compiled path
/// dependency can never leave the notice silently.
#[test]
fn a_path_dependency_outside_the_workspace_is_refused_not_dropped() {
    let probe = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
import render
about = json.loads(sys.argv[2])
try:
    kept = render.third_party(about, set(json.loads(sys.argv[3])))
    print("kept", sorted(name for name, _ in kept))
except SystemExit as refused:
    print("refused", refused)
"#;
    let member = |name: &str| format!("path+file:///repo/crates/{name}#{name}@0.0.1");
    let workspace = serde_json::json!([member("clank"), member("clank-core")]);
    let package = |name: &str, version: &str, id: String, source: Option<&str>| serde_json::json!({"package": {"name": name, "version": version, "id": id, "source": source}});
    let tokio = package(
        "tokio",
        "1.0.0",
        "registry+https://github.com/rust-lang/crates.io-index#tokio@1.0.0".into(),
        Some("registry+https://github.com/rust-lang/crates.io-index"),
    );
    let run = |crates: Vec<serde_json::Value>| {
        let about = serde_json::json!({"crates": crates, "licenses": []});
        let out = std::process::Command::new("python3")
            .args(["-B", "-I", "-c", probe])
            .arg(root().join("scripts/notices"))
            .arg(about.to_string())
            .arg(workspace.to_string())
            .output()
            .expect("run python3");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    let ours = run(vec![
        package("clank", "0.0.1", member("clank"), None),
        package("clank-core", "0.0.1", member("clank-core"), None),
        tokio.clone(),
    ]);
    assert_eq!(ours.trim(), "kept ['tokio']");

    let outside = run(vec![
        package("clank", "0.0.1", member("clank"), None),
        package(
            "outside-probe",
            "1.0.0",
            "path+file:///elsewhere/probe#outside-probe@1.0.0".into(),
            None,
        ),
        tokio.clone(),
    ]);
    assert!(
        outside.starts_with("refused outside-probe 1.0.0 is a path dependency"),
        "{outside}"
    );

    let shadow = run(vec![
        package("clank", "0.0.1", member("clank"), None),
        package(
            "clank-core",
            "9.0.0",
            "path+file:///elsewhere/shadow#clank-core@9.0.0".into(),
            None,
        ),
        tokio,
    ]);
    assert!(
        shadow.starts_with("refused clank-core 9.0.0 is a path dependency"),
        "{shadow}"
    );
}

/// cargo-about reports workspace package ids with the physical path while
/// `cargo metadata` keeps the path it was given, so the notice scripts
/// resolve the checkout physically: run through a symlinked checkout, the
/// workspace's own packages must still be recognised as its own.
#[test]
fn the_notice_check_passes_through_a_symlinked_checkout() {
    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("checkout");
    std::os::unix::fs::symlink(root(), &link).unwrap();
    let out = std::process::Command::new(link.join("scripts/notices/check.sh"))
        .current_dir(&link)
        .output()
        .expect("run check.sh");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
