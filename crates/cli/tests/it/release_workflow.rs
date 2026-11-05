//! Enforcement gate: a release tag builds every target on a runner of
//! its own OS and arch, checks the binary it built against the tag, and
//! publishes from one job that waits for every target and alone may
//! write. Also the scripts the workflow packs and publishes with.
//!
//! The rules read the workflow's structure, as `pages_workflow` does: a
//! publish that stops waiting for the matrix, or a leg that gains write
//! access, keeps every action name in place.

use crate::ci_workflow::read_workflow;
use crate::common::crates_dir;
use crate::workflow_tree::{
    Node, at, entries, keys, parse, run_text, step_field, steps, uses_action, value,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Every release target and the runner that builds it natively.
pub(crate) const TARGETS: [(&str, &str); 4] = [
    ("aarch64-apple-darwin", "macos-15"),
    ("x86_64-apple-darwin", "macos-15-intel"),
    ("x86_64-unknown-linux-musl", "ubuntu-24.04"),
    ("aarch64-unknown-linux-musl", "ubuntu-24.04-arm"),
];

const BUILD: &str = r#"cargo build --locked --release --target "${{ matrix.target }}" -p clank --bin clank --bin clank-web-cloudflare"#;
const CHECK: &str =
    r#"scripts/release-check.sh "$GITHUB_REF_NAME" "target/${{ matrix.target }}/release/clank""#;
const ASSETS: &str = r#"scripts/release-assets.sh "${GITHUB_REF_NAME#v}" dist"#;
const NOTES: &str = r#"scripts/release-notes.sh "${GITHUB_REF_NAME#v}" > notes.md"#;
const CREATE: &str = r#"gh release create "$GITHUB_REF_NAME" --verify-tag --title "clank ${GITHUB_REF_NAME#v}" --notes-file notes.md dist/*.tar.gz dist/SHA256SUMS"#;

fn position(steps: &[Node], pred: impl Fn(&Node) -> bool) -> Option<usize> {
    steps.iter().position(pred)
}

fn runs(step: &Node, script: &str) -> bool {
    run_text(step).as_deref() == Some(script)
}

fn in_order(at: &[Option<usize>]) -> bool {
    at.iter().all(Option::is_some) && at.windows(2).all(|w| w[0] < w[1])
}

/// A step that would publish: the gh CLI's release commands or a
/// release action.
fn publishes(step: &Node) -> bool {
    run_text(step).is_some_and(|r| r.contains("gh release") || r.contains("gh api"))
        || step_field(step, "uses").is_some_and(|u| u.contains("release"))
}

/// Every rule the workflow `text` breaks.
fn violations(text: &str) -> Vec<String> {
    let root = parse(text);
    let mut bad = Vec::new();
    let mut want = |ok: bool, rule: &str| {
        if !ok {
            bad.push(rule.to_string());
        }
    };

    want(
        keys(at(&root, &["on"])) == ["push"]
            && keys(at(&root, &["on", "push"])) == ["tags"]
            && value(&root, &["on", "push", "tags"]) == Some(r#"["v*"]"#),
        "only a v* tag push triggers it",
    );
    want(
        at(&root, &["permissions"]).is_none(),
        "no workflow-wide permissions",
    );
    want(
        keys(at(&root, &["jobs"])) == ["build", "publish"],
        "the jobs are build and publish",
    );

    let matrix = at(&root, &["jobs", "build", "strategy", "matrix"]);
    want(
        keys(matrix) == ["include"],
        "the matrix is an explicit include list",
    );
    let mut legs: Vec<(String, String)> =
        at(&root, &["jobs", "build", "strategy", "matrix", "include"])
            .map(|n| {
                n.children
                    .iter()
                    .map(|leg| {
                        (
                            step_field(leg, "target").unwrap_or_default().to_string(),
                            step_field(leg, "runner").unwrap_or_default().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
    legs.sort();
    let mut expected: Vec<(String, String)> = TARGETS
        .iter()
        .map(|(t, r)| (t.to_string(), r.to_string()))
        .collect();
    expected.sort();
    want(
        legs == expected,
        "the matrix is the four targets, each on its native runner",
    );
    want(
        value(&root, &["jobs", "build", "runs-on"]) == Some("${{ matrix.runner }}"),
        "each leg runs on its own runner",
    );
    want(
        entries(at(&root, &["jobs", "build", "permissions"])) == ["contents: read"],
        "build may only read contents",
    );

    let build = steps(&root, "build");
    want(
        in_order(&[
            position(build, |s| uses_action(s, "actions/checkout")),
            position(build, |s| runs(s, BUILD)),
            position(build, |s| runs(s, CHECK)),
            position(build, |s| {
                run_text(s).is_some_and(|r| {
                    r.contains(r#""$bin/clank" --help"#)
                        && r.contains(r#""$bin/clank-web-cloudflare" </dev/null"#)
                })
            }),
            position(build, |s| {
                run_text(s).is_some_and(|r| r.starts_with("scripts/release-archive.sh "))
            }),
            position(build, |s| uses_action(s, "actions/upload-artifact")),
        ]),
        "build builds both executables, checks the tag, runs both, packs, then uploads",
    );
    want(!build.iter().any(publishes), "no build leg publishes");

    let publish = at(&root, &["jobs", "publish"]).map_or(&[][..], |n| &n.children);
    want(
        value(publish, &["needs"]) == Some("build"),
        "publish needs the whole build matrix",
    );
    want(
        at(publish, &["if"]).is_none(),
        "publish runs only when every leg succeeded",
    );
    want(
        entries(at(publish, &["permissions"])) == ["contents: write"],
        "publish holds exactly contents: write",
    );
    let publish_steps = steps(&root, "publish");
    let create = position(publish_steps, |s| runs(s, CREATE));
    want(
        in_order(&[
            position(publish_steps, |s| uses_action(s, "actions/checkout")),
            position(publish_steps, |s| {
                uses_action(s, "actions/download-artifact")
            }),
            position(publish_steps, |s| runs(s, ASSETS)),
            position(publish_steps, |s| runs(s, NOTES)),
            create,
        ]) && create == Some(publish_steps.len() - 1),
        "publish gathers the archives, sums them, takes the notes, then creates the release last",
    );
    want(
        publish_steps.iter().filter(|s| publishes(s)).count() == 1,
        "the release is created in one step",
    );

    want(
        !text.contains("continue-on-error"),
        "no step's failure is ignored",
    );
    want(
        text.match_indices("secrets.")
            .all(|(i, _)| text[i..].starts_with("secrets.GITHUB_TOKEN ")),
        "no secret but GITHUB_TOKEN is read",
    );
    bad
}

#[test]
fn the_release_workflow_publishes_only_what_every_target_built_and_checked() {
    let bad = violations(&read_workflow("release.yml"));
    assert!(bad.is_empty(), "release.yml breaks: {bad:?}");
}

/// The real workflow with `old` replaced by `new`.
fn mutated(old: &str, new: &str) -> String {
    let text = read_workflow("release.yml");
    assert!(text.contains(old), "release.yml lacks {old:?}");
    text.replacen(old, new, 1)
}

fn assert_rejected(text: &str, rule: &str) {
    let bad = violations(text);
    assert!(
        bad.iter().any(|b| b == rule),
        "expected {rule:?}, got {bad:?}"
    );
}

#[test]
fn a_publish_that_does_not_wait_for_the_matrix_is_rejected() {
    assert_rejected(
        &mutated("    needs: build\n", ""),
        "publish needs the whole build matrix",
    );
    assert_rejected(
        &mutated("    needs: build\n", "    needs: build\n    if: always()\n"),
        "publish runs only when every leg succeeded",
    );
}

#[test]
fn a_matrix_leg_that_can_write_is_rejected() {
    assert_rejected(
        &mutated("      contents: read\n", "      contents: write\n"),
        "build may only read contents",
    );
}

#[test]
fn a_matrix_leg_that_publishes_is_rejected() {
    let upload = "      - uses: actions/upload-artifact@v4\n";
    assert_rejected(
        &mutated(
            upload,
            &format!("      - run: gh release upload \"$GITHUB_REF_NAME\" dist/*\n\n{upload}"),
        ),
        "no build leg publishes",
    );
}

#[test]
fn a_missing_target_is_rejected() {
    assert_rejected(
        &mutated(
            "          - target: x86_64-apple-darwin\n            runner: macos-15-intel\n",
            "",
        ),
        "the matrix is the four targets, each on its native runner",
    );
}

#[test]
fn a_cross_compiled_target_is_rejected() {
    assert_rejected(
        &mutated("runner: ubuntu-24.04-arm", "runner: ubuntu-24.04"),
        "the matrix is the four targets, each on its native runner",
    );
}

#[test]
fn a_build_that_skips_the_tag_check_is_rejected() {
    assert_rejected(
        &mutated(&format!("        run: {CHECK}\n"), "        run: true\n"),
        "build builds both executables, checks the tag, runs both, packs, then uploads",
    );
}

#[test]
fn a_secret_is_rejected() {
    assert_rejected(
        &mutated(
            "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
            "GH_TOKEN: ${{ secrets.RELEASE_PAT }}",
        ),
        "no secret but GITHUB_TOKEN is read",
    );
}

fn script(name: &str) -> PathBuf {
    crates_dir().join("../scripts").join(name)
}

/// Stand-ins for both executables, each printing its name, `version`
/// and `target`, so an installed file says which archive it came from.
pub(crate) fn fake_bins(dir: &Path, version: &str, target: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    for bin in ["clank", "clank-web-cloudflare"] {
        let path = dir.join(bin);
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho '{bin} {version} {target}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Runs a release script, returning success and stderr.
pub(crate) fn run_script(name: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(script(name))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {name}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub(crate) fn pack(bins: &Path, version: &str, target: &str, out: &Path) {
    let (ok, err) = run_script(
        "release-archive.sh",
        &[
            version,
            target,
            bins.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
    );
    assert!(ok, "release-archive.sh {target}: {err}");
}

#[test]
fn an_archive_holds_both_executables_the_licence_and_the_notices() {
    let dir = tempfile::tempdir().unwrap();
    let bins = dir.path().join("bins");
    fake_bins(&bins, "1.2.3", "x86_64-apple-darwin");
    pack(
        &bins,
        "1.2.3",
        "x86_64-apple-darwin",
        &dir.path().join("out"),
    );
    let list = Command::new("tar")
        .arg("-tzf")
        .arg(
            dir.path()
                .join("out/clank-1.2.3-x86_64-apple-darwin.tar.gz"),
        )
        .output()
        .unwrap();
    assert!(list.status.success());
    let mut files: Vec<String> = String::from_utf8(list.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.ends_with('/'))
        .map(str::to_string)
        .collect();
    files.sort();
    let top = "clank-1.2.3-x86_64-apple-darwin";
    assert_eq!(
        files,
        [
            format!("{top}/LICENSE"),
            format!("{top}/THIRD-PARTY-NOTICES.md"),
            format!("{top}/clank"),
            format!("{top}/clank-web-cloudflare"),
            format!("{top}/crates/cli/notices/rust-dependencies.txt"),
        ]
    );
}

fn all_archives(version: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (target, _) in TARGETS {
        let bins = dir.path().join("bins").join(target);
        fake_bins(&bins, version, target);
        pack(&bins, version, target, &dir.path().join("dist"));
    }
    std::fs::remove_dir_all(dir.path().join("bins")).unwrap();
    dir
}

#[test]
fn the_assets_are_every_target_and_one_checksum_each() {
    let dir = all_archives("1.2.3");
    let dist = dir.path().join("dist");
    let (ok, err) = run_script("release-assets.sh", &["1.2.3", dist.to_str().unwrap()]);
    assert!(ok, "{err}");
    let sums = std::fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let mut named: Vec<&str> = sums
        .lines()
        .map(|l| l.split_once("  ").expect("<sha256>  <file>").1)
        .collect();
    named.sort();
    let mut want: Vec<String> = TARGETS
        .iter()
        .map(|(t, _)| format!("clank-1.2.3-{t}.tar.gz"))
        .collect();
    want.sort();
    assert_eq!(named, want);
}

#[test]
fn a_missing_or_stray_archive_is_refused() {
    let dir = all_archives("1.2.3");
    let dist = dir.path().join("dist");
    std::fs::remove_file(dist.join("clank-1.2.3-aarch64-unknown-linux-musl.tar.gz")).unwrap();
    let (ok, err) = run_script("release-assets.sh", &["1.2.3", dist.to_str().unwrap()]);
    assert!(!ok && err.contains("exactly the release archives"), "{err}");
    assert!(!dist.join("SHA256SUMS").exists());

    let dir = all_archives("1.2.3");
    let dist = dir.path().join("dist");
    std::fs::write(
        dist.join("clank-1.2.3-riscv64gc-unknown-linux-musl.tar.gz"),
        "",
    )
    .unwrap();
    let (ok, err) = run_script("release-assets.sh", &["1.2.3", dist.to_str().unwrap()]);
    assert!(!ok && err.contains("exactly the release archives"), "{err}");
}

#[test]
fn the_notes_are_the_versions_changelog_section() {
    let dir = tempfile::tempdir().unwrap();
    let changelog = dir.path().join("CHANGELOG.md");
    std::fs::write(
        &changelog,
        "# Changelog\n\n## [Unreleased]\n\n- next\n\n## [0.2.0]\n\n- two\n\n## [0.1.0]\n\n- one\n",
    )
    .unwrap();
    let out = Command::new(script("release-notes.sh"))
        .args(["0.2.0", changelog.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "- two\n");

    let (ok, err) = run_script("release-notes.sh", &["0.3.0", changelog.to_str().unwrap()]);
    assert!(!ok && err.contains("no notes under '## [0.3.0]'"), "{err}");
}
