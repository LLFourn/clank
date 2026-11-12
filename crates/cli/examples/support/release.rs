//! Release checks split into data validation and the actual Cargo/binary calls.
//! Tests exercise the validators directly; only a real release builds binaries.
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

pub fn run(root: &Path, tag: &str, binary: Option<&Path>) -> Result<()> {
    use clank::proc_run::Spec;
    let wanted = version(tag)?;
    let manifest = root.join("Cargo.toml");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let metadata = query(
        Spec::new(&cargo)
            .args([
                "metadata",
                "--locked",
                "--offline",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(&manifest),
    )?;
    check_declared(wanted, declared(&serde_json::from_str(&metadata)?)?)?;
    let binary = match binary {
        Some(path) => path.to_owned(),
        None => executable(&query(
            Spec::new(&cargo)
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
                .arg(&manifest),
        )?)?,
    };
    // Do not change cwd here: explicit relative binary paths are relative to
    // the caller, just as in release-check.sh before it delegated to Rust.
    let output = query(Spec::new(binary).arg("--version"))?;
    check_binary(wanted, &output)?;
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
