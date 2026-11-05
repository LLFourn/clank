//! `scripts/release-check.sh` refuses a release whose binary, tag and
//! changelog disagree. Each case runs a copy of the script in a fixture
//! tree with a stand-in binary that prints a chosen `--version`; nothing
//! is built or published.

use crate::common::crates_dir;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A fixture tree: the script, a changelog, a `clank` crate declaring
/// `declares` whose binary prints `builds`, and a stand-in prebuilt
/// binary printing `prints`.
fn fixture(changelog: &str, declares: &str, builds: &str, prints: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::copy(
        crates_dir().join("../scripts/release-check.sh"),
        root.join("scripts/release-check.sh"),
    )
    .unwrap();
    std::fs::write(root.join("CHANGELOG.md"), changelog).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"clank\"\nversion = \"{declares}\"\nedition = \"2021\"\n\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        format!("fn main() {{ println!(\"{builds}\"); }}\n"),
    )
    .unwrap();
    let lock = std::process::Command::new(cargo())
        .args(["generate-lockfile", "--offline"])
        .current_dir(root)
        .status()
        .unwrap();
    assert!(lock.success());
    let bin = root.join("prebuilt-clank");
    std::fs::write(&bin, format!("#!/bin/sh\necho '{prints}'\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn cargo() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn run(root: &Path, tag: &str, prebuilt: bool, target_dir: Option<&Path>) -> (bool, String) {
    let mut cmd = std::process::Command::new(root.join("scripts/release-check.sh"));
    cmd.arg(tag);
    if prebuilt {
        cmd.arg(root.join("prebuilt-clank"));
    }
    cmd.env_remove("CARGO_TARGET_DIR");
    if let Some(dir) = target_dir {
        cmd.env("CARGO_TARGET_DIR", dir);
    }
    let out = cmd.output().expect("run release-check.sh");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn check(root: &Path, tag: &str) -> (bool, String) {
    run(root, tag, true, None)
}

const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0]\n\nFirst.\n";

#[test]
fn a_matching_release_passes() {
    let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0", "clank 0.1.0");
    assert!(check(f.path(), "v0.1.0").0);
}

#[test]
fn a_tag_the_binary_was_not_built_from_is_refused() {
    let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0", "clank 0.1.0");
    let (ok, err) = check(f.path(), "v0.2.0");
    assert!(
        !ok && err.contains("tag says 0.2.0, but the clank crate declares 0.1.0"),
        "{err}"
    );
}

#[test]
fn a_build_off_the_tag_or_dirty_is_refused() {
    for prints in ["clank 0.1.0+1846c7f8", "clank 0.1.0+1846c7f8.dirty"] {
        let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0", prints);
        let (ok, err) = check(f.path(), "v0.1.0");
        assert!(!ok && err.contains(prints), "{prints}: {err}");
    }
}

#[test]
fn a_version_missing_from_the_changelog_is_refused() {
    let f = fixture(
        "# Changelog\n\n## [Unreleased]\n",
        "0.1.0",
        "clank 0.1.0",
        "clank 0.1.0",
    );
    let (ok, err) = check(f.path(), "v0.1.0");
    assert!(!ok && err.contains("no '## [0.1.0]' section"), "{err}");
}

#[test]
fn a_tag_that_is_not_v_semver_is_refused() {
    let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0", "clank 0.1.0");
    for tag in [
        "0.1.0",
        "v01.1.0",
        "v0.1.0-",
        "v0.1.0-rc.1",
        "v0.1.0+1846c7f8.dirty",
    ] {
        let (ok, err) = check(f.path(), tag);
        assert!(
            !ok && err.contains("is not v<MAJOR>.<MINOR>.<PATCH>"),
            "{tag}: {err}"
        );
    }
}

/// With no binary given, the script checks the executable Cargo reports
/// building, wherever the target directory is, never a stale one at the
/// default path.
#[test]
fn the_built_executable_is_found_from_cargo_not_guessed() {
    let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0+fresh.dirty", "unused");
    let stale = f.path().join("target/release");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("clank"), "#!/bin/sh\necho 'clank 0.1.0'\n").unwrap();
    std::fs::set_permissions(stale.join("clank"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (ok, err) = run(f.path(), "v0.1.0", false, Some(elsewhere.path()));
    assert!(!ok && err.contains("clank 0.1.0+fresh.dirty"), "{err}");

    let f = fixture(CHANGELOG, "0.1.0", "clank 0.1.0", "unused");
    let (ok, err) = run(f.path(), "v0.1.0", false, Some(elsewhere.path()));
    assert!(ok, "{err}");
}

/// The repo's own changelog has a section for the version every crate
/// declares, so a release cut from this tree passes the check.
#[test]
fn the_changelog_covers_the_workspace_version() {
    let manifest = std::fs::read_to_string(crates_dir().join("../Cargo.toml")).unwrap();
    let version = manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|v| v.strip_suffix('"'))
        .expect("[workspace.package] version");
    let changelog = std::fs::read_to_string(crates_dir().join("../CHANGELOG.md")).unwrap();
    assert!(
        changelog.lines().any(|l| l == format!("## [{version}]")),
        "CHANGELOG.md has no section for {version}"
    );
}
