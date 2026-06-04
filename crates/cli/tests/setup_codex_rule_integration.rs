//! Integration tests for `clank setup`'s `~/.codex/rules/default.rules`
//! handling. The rule write follows the same conventions as the
//! other setup writes: idempotent, honors `--dry-run`, fails closed
//! on user-explicit denials.

use std::path::Path;
use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn run_setup(home: &Path, dry_run: bool) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("setup").env("HOME", home);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.output().expect("spawn clank setup")
}

fn rules_path(home: &Path) -> std::path::PathBuf {
    home.join(".codex/rules/default.rules")
}

const EXPECTED_LINE: &str = r#"prefix_rule(pattern=["clank"], decision="allow")"#;

#[test]
fn setup_writes_rule_in_fresh_home() {
    let home = tempfile::tempdir().unwrap();
    let out = run_setup(home.path(), false);
    assert!(
        out.status.success(),
        "setup failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(rules_path(home.path())).expect("rules file exists");
    assert!(
        body.contains(EXPECTED_LINE),
        "expected `{EXPECTED_LINE}` in rules; got:\n{body}"
    );
}

#[test]
fn setup_creates_parent_dirs() {
    let home = tempfile::tempdir().unwrap();
    // No ~/.codex dir at all before setup.
    assert!(!home.path().join(".codex").exists());
    let out = run_setup(home.path(), false);
    assert!(out.status.success(), "setup failed");
    assert!(
        rules_path(home.path()).exists(),
        "rules file should be created with parent dirs"
    );
}

#[test]
fn setup_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let path = rules_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{EXPECTED_LINE}\n")).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let out = run_setup(home.path(), false);
    assert!(out.status.success());
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before, after, "rules file should be unchanged");
    let count = after.matches(EXPECTED_LINE).count();
    assert_eq!(count, 1, "expected exactly one rule line; got {count}");
}

#[test]
fn setup_preserves_other_rules() {
    let home = tempfile::tempdir().unwrap();
    let path = rules_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let preexisting = "prefix_rule(pattern=[\"cargo\", \"test\"], decision=\"allow\")\n\
                       prefix_rule(pattern=[\"git\", \"commit\"], decision=\"allow\")\n";
    std::fs::write(&path, preexisting).unwrap();
    let out = run_setup(home.path(), false);
    assert!(out.status.success());
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains(r#"pattern=["cargo", "test"]"#),
        "cargo test rule was clobbered: {after}"
    );
    assert!(
        after.contains(r#"pattern=["git", "commit"]"#),
        "git commit rule was clobbered: {after}"
    );
    assert!(after.contains(EXPECTED_LINE), "clank rule was not appended");
}

#[test]
fn setup_errors_on_existing_deny_for_clank() {
    let home = tempfile::tempdir().unwrap();
    let path = rules_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let deny_line = r#"prefix_rule(pattern=["clank"], decision="deny")"#;
    let original = format!("{deny_line}\n");
    std::fs::write(&path, &original).unwrap();
    let out = run_setup(home.path(), false);
    assert!(
        !out.status.success(),
        "setup must fail-closed when user has explicit deny rule; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // File is left unchanged.
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after, original, "file must not be modified on error");
    // Diagnostic mentions the offending line.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("denies the `clank` pattern"),
        "stderr should explain the failure; got: {stderr}"
    );
}

#[test]
fn setup_ignores_more_specific_patterns() {
    // Pre-existing `["clank", "init"]` is a *different* pattern;
    // setup should still append the bare `["clank"]` rule.
    let home = tempfile::tempdir().unwrap();
    let path = rules_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "prefix_rule(pattern=[\"clank\", \"init\"], decision=\"allow\")\n",
    )
    .unwrap();
    let out = run_setup(home.path(), false);
    assert!(out.status.success());
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains(EXPECTED_LINE),
        "bare clank rule should still be appended; got:\n{after}"
    );
}

#[test]
fn doctor_warns_when_rule_missing() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success());

    let out = Command::new(clank_bin())
        .arg("doctor")
        .arg("--repo")
        .arg(repo.path())
        .arg("--json")
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON");
    let checks = parsed.as_array().expect("array");
    let rule_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("~/.codex/rules/default.rules"))
        .expect("expected doctor check for default.rules");
    assert_eq!(rule_check["status"], "warn");
    let msg = rule_check["message"].as_str().unwrap_or("");
    assert!(msg.contains("clank setup"), "msg should suggest fix: {msg}");
}

#[test]
fn doctor_ok_when_rule_present() {
    let home = tempfile::tempdir().unwrap();
    let path = rules_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{EXPECTED_LINE}\n")).unwrap();

    let repo = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap();
    assert!(status.success());

    let out = Command::new(clank_bin())
        .arg("doctor")
        .arg("--repo")
        .arg(repo.path())
        .arg("--json")
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON");
    let rule_check = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"].as_str() == Some("~/.codex/rules/default.rules"))
        .expect("rule check entry");
    assert_eq!(rule_check["status"], "ok");
}

#[test]
fn setup_dry_run_reports_but_does_not_write() {
    let home = tempfile::tempdir().unwrap();
    let out = run_setup(home.path(), true);
    assert!(
        out.status.success(),
        "setup --dry-run failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dry-run"),
        "stdout should mention dry-run; got: {stdout}"
    );
    assert!(
        stdout.contains("default.rules"),
        "stdout should mention the rules file; got: {stdout}"
    );
    assert!(
        !rules_path(home.path()).exists(),
        "rules file must NOT be created under --dry-run"
    );
}
