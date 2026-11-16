//! Release checks split into data validation and the actual Cargo/binary calls.
//! Tests drive [`run`] with a stand-in [`Toolchain`]; only a real release
//! builds binaries.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn version(tag: &str) -> Result<&str> {
    let invalid = || anyhow::anyhow!("tag '{tag}' is not v<MAJOR>.<MINOR>.<PATCH>");
    let value = tag.strip_prefix('v').ok_or_else(invalid)?;
    let parts: Vec<_> = value.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|p| !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))),
        invalid()
    );
    Ok(value)
}

fn declared(meta: &Value) -> Result<&str> {
    meta["packages"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["name"] == "clank"))
        .and_then(|p| p["version"].as_str())
        .context("Cargo metadata contains no clank package version")
}

fn check_declared(wanted: &str, declared: &str) -> Result<()> {
    ensure!(
        wanted == declared,
        "the tag says {wanted}, but the clank crate declares {declared}"
    );
    Ok(())
}

fn check_binary(wanted: &str, output: &str) -> Result<()> {
    let got = output.trim_end_matches('\n');
    ensure!(
        got == format!("clank {wanted}"),
        "built '{got}', but the tag says 'clank {wanted}'"
    );
    Ok(())
}

fn check_changelog(wanted: &str, changelog: &str) -> Result<()> {
    ensure!(
        changelog
            .lines()
            .any(|line| line == format!("## [{wanted}]")),
        "CHANGELOG.md has no '## [{wanted}]' section"
    );
    Ok(())
}

/// Use Cargo's artifact path, including custom target directories/targets.
/// Ignore libraries, examples and other binaries even if reported afterwards.
fn executable(messages: &str) -> Result<PathBuf> {
    let mut found = None;
    for line in messages.lines().filter(|line| !line.trim().is_empty()) {
        let m: Value = serde_json::from_str(line).context("invalid Cargo build message")?;
        if m["reason"] == "compiler-artifact"
            && m["target"]["name"] == "clank"
            && m["target"]["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "bin"))
            && let Some(path) = m["executable"].as_str()
        {
            found = Some(PathBuf::from(path));
        }
    }
    found.context("cargo reported no clank executable")
}

fn query(spec: clank::proc_run::Spec) -> Result<String> {
    use clank::proc_run::{Out, run_blocking};
    let output = run_blocking(
        spec.stdout(Out::Capture {
            cap: 64 * 1024 * 1024,
        })
        .stderr(Out::Inherit),
        std::time::Duration::from_secs(30 * 60),
    )?;
    ensure!(
        output.status.success(),
        "release command failed: {}",
        output.status
    );
    String::from_utf8(output.stdout).context("release command returned non-UTF-8 output")
}

/// The commands a release check runs, so the check itself is testable
/// without building or running a binary.
pub trait Toolchain {
    /// `cargo metadata` JSON for the workspace at `manifest`.
    fn metadata(&self, manifest: &Path) -> Result<String>;
    /// `cargo build --message-format=json` output for the release `clank`.
    fn build(&self, manifest: &Path) -> Result<String>;
    /// What `binary --version` prints.
    fn version(&self, binary: &Path) -> Result<String>;
}

pub struct Cargo;

impl Cargo {
    fn cargo() -> std::ffi::OsString {
        std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
    }
}

impl Toolchain for Cargo {
    fn metadata(&self, manifest: &Path) -> Result<String> {
        query(
            clank::proc_run::Spec::new(Self::cargo())
                .args([
                    "metadata",
                    "--locked",
                    "--offline",
                    "--no-deps",
                    "--format-version",
                    "1",
                    "--manifest-path",
                ])
                .arg(manifest),
        )
    }

    fn build(&self, manifest: &Path) -> Result<String> {
        query(
            clank::proc_run::Spec::new(Self::cargo())
                .args([
                    "build",
                    "--locked",
                    "--release",
                    "-p",
                    "clank",
                    "--bin",
                    "clank",
                    "--message-format=json",
                    "--manifest-path",
                ])
                .arg(manifest),
        )
    }

    fn version(&self, binary: &Path) -> Result<String> {
        // Do not change cwd here: explicit relative binary paths are relative
        // to the caller, just as in release-check.sh before it delegated to Rust.
        query(clank::proc_run::Spec::new(binary).arg("--version"))
    }
}

pub fn run(root: &Path, tag: &str, binary: Option<&Path>, tools: &impl Toolchain) -> Result<()> {
    let wanted = version(tag)?;
    let manifest = root.join("Cargo.toml");
    check_declared(
        wanted,
        declared(&serde_json::from_str(&tools.metadata(&manifest)?)?)?,
    )?;
    let binary = match binary {
        Some(path) => path.to_owned(),
        None => executable(&tools.build(&manifest)?)?,
    };
    check_binary(wanted, &tools.version(&binary)?)?;
    check_changelog(wanted, &std::fs::read_to_string(root.join("CHANGELOG.md"))?)?;
    println!("release {tag}: version and changelog agree");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn releases_require_a_plain_tag_and_matching_package_binary_and_changelog() {
        for tag in ["v0.1.0", "v10.20.30"] {
            assert_eq!(version(tag).unwrap(), &tag[1..]);
        }
        for tag in [
            "0.1.0",
            "v01.1.0",
            "v0.1.0-",
            "v0.1.0-rc.1",
            "v0.1.0+abc.dirty",
            "v0.1",
            "v0..1",
            "v0.1.0.1",
            "v0.１.0",
        ] {
            assert!(version(tag).is_err(), "{tag}");
        }
        let meta = json!({"packages":[{"name":"other","version":"9.0.0"},{"name":"clank","version":"0.1.0"}]});
        check_declared("0.1.0", declared(&meta).unwrap()).unwrap();
        assert!(check_declared("0.2.0", declared(&meta).unwrap()).is_err());
        assert!(declared(&json!({"packages":[]})).is_err());
        check_binary("0.1.0", "clank 0.1.0\n").unwrap();
        for got in [
            "clank 0.1.0+abc",
            "clank 0.1.0+abc.dirty",
            "clank 0.2.0",
            "other 0.1.0",
            "clank 0.1.0\nextra",
        ] {
            assert!(check_binary("0.1.0", got).is_err(), "{got}");
        }
        check_changelog("0.1.0", "# Changelog\n\n## [Unreleased]\n\n## [0.1.0]\n").unwrap();
        for body in [
            "## [Unreleased]\n",
            "prose naming ## [0.1.0]\n",
            "## [0.1.0-rc.1]\n",
        ] {
            assert!(check_changelog("0.1.0", body).is_err(), "{body}");
        }
    }

    #[test]
    fn cargo_artifacts_select_the_actual_binary_not_a_conventional_path() {
        let artifact = |name: &str, kind: &str, path: Option<&str>| {
            json!({
            "reason":"compiler-artifact", "target":{"name":name,"kind":[kind]}, "executable":path,
        }).to_string()
        };
        let path = "/elsewhere/target/aarch64-apple-darwin/release/clank";
        let messages = [
            artifact("clank", "lib", None),
            artifact("clank", "bin", Some(path)),
            artifact("clank-web-cloudflare", "bin", Some("/other")),
            artifact("clank", "example", Some("/example")),
            json!({"reason":"build-finished","success":true}).to_string(),
        ]
        .join("\n");
        assert_eq!(executable(&messages).unwrap(), Path::new(path));
        assert!(executable(&artifact("clank", "lib", None)).is_err());
        assert!(executable("not JSON").is_err());
    }

    /// Answers as Cargo and a built binary would, and remembers which binary
    /// was asked its version.
    struct Fake {
        declares: &'static str,
        built: &'static str,
        prints: &'static str,
        asked: std::cell::RefCell<Option<PathBuf>>,
    }

    impl Fake {
        fn new(declares: &'static str, prints: &'static str) -> Self {
            Self {
                declares,
                built: "/built/release/clank",
                prints,
                asked: Default::default(),
            }
        }
    }

    impl Toolchain for Fake {
        fn metadata(&self, _: &Path) -> Result<String> {
            Ok(json!({"packages":[{"name":"clank","version":self.declares}]}).to_string())
        }
        fn build(&self, _: &Path) -> Result<String> {
            Ok(json!({
                "reason":"compiler-artifact",
                "target":{"name":"clank","kind":["bin"]},
                "executable":self.built,
            })
            .to_string())
        }
        fn version(&self, binary: &Path) -> Result<String> {
            *self.asked.borrow_mut() = Some(binary.to_owned());
            Ok(format!("{}\n", self.prints))
        }
    }

    fn workspace(changelog: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("CHANGELOG.md"), changelog).unwrap();
        root
    }

    const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0]\n\nFirst.\n";

    #[test]
    fn run_refuses_a_dirty_or_off_tag_binary() {
        let root = workspace(CHANGELOG);
        let given = Path::new("prebuilt/clank");
        let fake = Fake::new("0.1.0", "clank 0.1.0");
        run(root.path(), "v0.1.0", Some(given), &fake).unwrap();
        assert_eq!(fake.asked.borrow().as_deref(), Some(given));
        for prints in ["clank 0.1.0+1846c7f8", "clank 0.1.0+1846c7f8.dirty"] {
            let error = run(
                root.path(),
                "v0.1.0",
                Some(given),
                &Fake::new("0.1.0", prints),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(prints), "{prints}: {error:#}");
        }
    }

    #[test]
    fn run_checks_the_binary_cargo_built_when_none_is_given() {
        let root = workspace(CHANGELOG);
        let fake = Fake::new("0.1.0", "clank 0.1.0+fresh.dirty");
        let error = run(root.path(), "v0.1.0", None, &fake).unwrap_err();
        assert!(format!("{error:#}").contains("clank 0.1.0+fresh.dirty"));
        assert_eq!(fake.asked.borrow().as_deref(), Some(Path::new(fake.built)));
    }

    #[test]
    fn run_refuses_a_tag_the_crate_or_changelog_disagrees_with() {
        let root = workspace(CHANGELOG);
        let error = run(
            root.path(),
            "v0.2.0",
            Some(Path::new("clank")),
            &Fake::new("0.1.0", "clank 0.2.0"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("the tag says 0.2.0, but the clank crate declares 0.1.0"),
            "{error:#}"
        );
        let bare = workspace("# Changelog\n\n## [Unreleased]\n");
        let error = run(
            bare.path(),
            "v0.1.0",
            Some(Path::new("clank")),
            &Fake::new("0.1.0", "clank 0.1.0"),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("no '## [0.1.0]' section"),
            "{error:#}"
        );
    }

    #[test]
    fn the_changelog_covers_the_workspace_version() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let manifest: toml::Value = toml::from_str(&manifest).unwrap();
        let version = manifest["workspace"]["package"]["version"]
            .as_str()
            .unwrap();
        check_changelog(
            version,
            &std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap(),
        )
        .unwrap();
    }
}
