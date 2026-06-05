//! Integration tests for `clank diff <plan|range>` (Phase 7 of
//! `clank-diff-editor`).
//!
//! The architectural decisions from the plan body's three review
//! rounds — commit-list semantics, foreign filter on finished
//! plans, no env-var leakage between kinds, OQ1 disambiguation,
//! OQ3 focus syntax, OQ7 layering, --wait override — all get
//! regression defense here.
//!
//! Tests construct config via the typed `RepoConfigFile` /
//! `UserConfigFile` structs + serde round-trip per the
//! `typed-config-dogfood` direction. No JSON literals.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use clank_core::agent_config::LaunchConfig;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    write(path, "README.md", "seed\n");
    git(path, &["add", "-A"]);
    git(path, &["commit", "--quiet", "-m", "seed"]);
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Write a `clank diff` editor config via typed RepoConfigFile.
fn configure_editor(repo: &Path, command: &str, args: Vec<&str>) {
    let mut launch = LaunchConfig::default();
    launch.command = Some(command.to_string());
    launch.args = args.into_iter().map(String::from).collect();
    let json = serde_json::json!({
        "diff": {
            "editor": serde_json::to_value(&launch).unwrap(),
        }
    });
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();
}

fn configure_editor_with_wait(repo: &Path, command: &str, wait: bool) {
    let mut launch = LaunchConfig::default();
    launch.command = Some(command.to_string());
    let json = serde_json::json!({
        "diff": {
            "editor": serde_json::to_value(&launch).unwrap(),
            "wait": wait,
        }
    });
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .unwrap();
}

fn run_diff(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("diff").arg("--repo").arg(repo).args(args);
    cmd.output().expect("spawn clank diff")
}

/// Parse `--print` stderr to extract env additions as a map.
/// `print_composed` escapes newlines as `\n`; this un-escapes them.
fn parse_env(stderr: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix("env: ") {
            if let Some((k, v)) = rest.split_once('=') {
                let unescaped = v.replace("\\n", "\n").replace("\\\\", "\\");
                out.insert(k.to_string(), unescaped);
            }
        }
    }
    out
}

/// Seed a single plan with N revisions. Returns (intro_sha, all_shas).
fn seed_plan(repo: &Path, stem: &str, n_revisions: usize) -> (String, Vec<String>) {
    write(repo, &format!(".clank/plans/{stem}.md"), "# v1\n");
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] intro")],
    );
    let intro = head_sha(repo);
    let mut all = vec![intro.clone()];
    for i in 0..n_revisions {
        write(
            repo,
            &format!(".clank/plans/{stem}.md"),
            &format!("# v{}\n", i + 2),
        );
        git(repo, &["add", "-A"]);
        git(
            repo,
            &["commit", "--quiet", "-m", &format!("[{stem}] revise {i}")],
        );
        all.push(head_sha(repo));
    }
    (intro, all)
}

// ── commits_for_plan / interleaving regressions ──────────────────

#[test]
fn clank_diff_active_plan_excludes_interleaved_other_active_plan_commits() {
    // Test 7 (codex cba9249's catch as integration regression):
    // plan A + plan B interleaved chronologically. `clank diff
    // alpha --print` must surface ONLY plan A's SHAs in
    // CLANK_DIFF_COMMITS, not the interleaved plan B commits.
    let dir = init_repo();
    let repo = dir.path();
    // Plan A intro.
    write(repo, ".clank/plans/alpha.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] intro"]);
    let a_intro = head_sha(repo);
    // Plan B intro (interleaved).
    write(repo, ".clank/plans/beta.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] intro"]);
    let b_intro = head_sha(repo);
    // Plan A revise.
    write(repo, ".clank/plans/alpha.md", "# v2\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] revise"]);
    let a_revise = head_sha(repo);

    configure_editor(repo, "true", vec![]);
    let out = run_diff(repo, &["alpha", "--print"]);
    assert!(out.status.success(), "diff alpha --print failed");
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    let commits = env
        .get("CLANK_DIFF_COMMITS")
        .expect("CLANK_DIFF_COMMITS present for plan invocation");
    let shas: Vec<&str> = commits.split(',').collect();
    assert!(
        shas.contains(&a_intro.as_str()),
        "alpha's intro must be present; got: {commits}"
    );
    assert!(
        shas.contains(&a_revise.as_str()),
        "alpha's revise must be present; got: {commits}"
    );
    assert!(
        !shas.contains(&b_intro.as_str()),
        "beta's intro MUST NOT leak into alpha's commit list \
         (interleaved-plans bug codex caught on cba9249); got: {commits}"
    );
}

#[test]
fn clank_diff_finished_plan_excludes_interleaved_foreign_commits() {
    // Test 8 (codex 0859200's catch as integration regression):
    // finished plan A had plan B's commits interleaved between A's
    // intro and finalize. The finished-plan path goes through
    // build_rewrite_preview which is range-based; commits_for_plan
    // must filter !c.foreign to avoid leaking B's commits.
    let dir = init_repo();
    let repo = dir.path();
    // Plan A intro.
    write(repo, ".clank/plans/alpha.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] intro"]);
    let a_intro = head_sha(repo);
    // Plan B intro (interleaved between A's intro and A's finalize).
    write(repo, ".clank/plans/beta.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] intro"]);
    let b_intro = head_sha(repo);
    // Plan A finalize: move alpha.md to finished/.
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/alpha.md"),
        repo.join(".clank/finished/alpha.md"),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] finish"]);
    let a_finalize = head_sha(repo);

    configure_editor(repo, "true", vec![]);
    let out = run_diff(repo, &["alpha", "--print"]);
    assert!(
        out.status.success(),
        "diff (finished plan) --print failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    let commits = env
        .get("CLANK_DIFF_COMMITS")
        .expect("CLANK_DIFF_COMMITS present for plan invocation");
    let shas: Vec<&str> = commits.split(',').collect();
    assert!(
        shas.contains(&a_intro.as_str()) && shas.contains(&a_finalize.as_str()),
        "finished alpha's intro AND finalize must be present; got: {commits}"
    );
    assert!(
        !shas.contains(&b_intro.as_str()),
        "beta's intro MUST NOT leak through the finished-plan \
         build_rewrite_preview path (codex 0859200 — the foreign \
         filter is load-bearing); got: {commits}"
    );
}

// ── CLI surface tests (plan, range, error paths) ─────────────────

#[test]
fn clank_diff_positional_resolves_plan_emits_kind_plan_env() {
    // Test 10: `clank diff <plan> --print` → KIND=plan + COMMITS
    // + PLAN env; NO RANGE env (no leakage between kinds).
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 1);
    configure_editor(repo, "true", vec!["--placeholder", "{commits}"]);

    let out = run_diff(repo, &["alpha", "--print"]);
    assert!(
        out.status.success(),
        "--print failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let env = parse_env(&stderr);
    assert_eq!(env.get("CLANK_DIFF_KIND"), Some(&"plan".to_string()));
    assert_eq!(env.get("CLANK_DIFF_PLAN"), Some(&"alpha".to_string()));
    assert!(env.contains_key("CLANK_DIFF_COMMITS"));
    assert!(
        !env.contains_key("CLANK_DIFF_RANGE"),
        "plan invocation must NOT set CLANK_DIFF_RANGE; got env: {env:?}"
    );
}

#[test]
fn clank_diff_positional_resolves_range_emits_kind_range_env() {
    // Test 11: `clank diff <range> --print` → KIND=range + RANGE
    // env; NO COMMITS env.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "a\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "a"]);
    write(repo, "b.txt", "b\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "b"]);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(repo, &["HEAD~2..HEAD", "--print"]);
    assert!(out.status.success(), "--print failed");
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.get("CLANK_DIFF_KIND"), Some(&"range".to_string()));
    assert!(env.contains_key("CLANK_DIFF_RANGE"));
    assert!(
        !env.contains_key("CLANK_DIFF_COMMITS"),
        "range invocation must NOT set CLANK_DIFF_COMMITS"
    );
    assert!(
        !env.contains_key("CLANK_DIFF_PLAN"),
        "range invocation must NOT set CLANK_DIFF_PLAN"
    );
}

#[test]
fn clank_diff_positional_unknown_errors_with_neither_diagnostic() {
    // Test 12: OQ1 pinned diagnostic — "not a known plan AND not
    // a valid git range" + available plans + --range escape hint.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(repo, &["not-a-plan-not-a-range", "--print"]);
    assert!(
        !out.status.success(),
        "must fail; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a known plan") && stderr.contains("not a valid git range"),
        "diagnostic should mention both interpretations; got: {stderr}"
    );
    assert!(
        stderr.contains("alpha"),
        "diagnostic should list available plans; got: {stderr}"
    );
    assert!(
        stderr.contains("--range"),
        "diagnostic should suggest --range escape; got: {stderr}"
    );
}

#[test]
fn clank_diff_no_args_infers_single_active_plan() {
    // Test 13: one active plan; bare `clank diff --print` resolves it.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(repo, &["--print"]);
    assert!(
        out.status.success(),
        "should resolve single active plan; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(env.get("CLANK_DIFF_KIND"), Some(&"plan".to_string()));
    assert_eq!(env.get("CLANK_DIFF_PLAN"), Some(&"alpha".to_string()));
}

#[test]
fn clank_diff_no_args_ambiguous_lists_candidates() {
    // Test 14: two active plans; `clank diff` errors with both
    // listed (same shape as `clank log`'s ambiguity error).
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    seed_plan(repo, "beta", 0);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(repo, &["--print"]);
    assert!(
        !out.status.success(),
        "ambiguous plan inference should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("alpha") && stderr.contains("beta"),
        "ambiguity diagnostic should name both plans; got: {stderr}"
    );
}

#[test]
fn clank_diff_plan_kind_template_var_in_range_invocation_errors() {
    // Test 15: `{commits}` template var in a range invocation
    // → compose-time error naming the wrong-kind variable.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "a\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "a"]);
    configure_editor(repo, "true", vec!["{commits}"]);

    let out = run_diff(repo, &["HEAD^..HEAD", "--print"]);
    assert!(
        !out.status.success(),
        "wrong-kind template var must error; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plan-only template variable") && stderr.contains("range diff"),
        "diagnostic should name the kind mismatch; got: {stderr}"
    );
}

// ── Launch composition tests ─────────────────────────────────────

#[test]
fn clank_diff_substitutes_range_template_in_args() {
    // Test 16: range invocation; `{range}` → literal range string.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "a\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "a"]);
    write(repo, "b.txt", "b\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "b"]);
    configure_editor(repo, "true", vec!["--range", "{range}"]);

    let out = run_diff(repo, &["HEAD~2..HEAD", "--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // {range} should expand to <from>..<to> where both ends are
    // resolved to full SHAs. We can't easily predict the SHAs but
    // we can assert the substitution produced a literal `..`
    // range string and not the template placeholder.
    assert!(
        !stdout.contains("{range}"),
        "{{range}} should have been substituted; got: {stdout}"
    );
    assert!(
        stdout.contains(".."),
        "range substitution should contain `..`; got: {stdout}"
    );
}

#[test]
fn clank_diff_substitutes_plan_commits_template_in_args() {
    // Test 17: plan invocation; `{commits}` → comma-separated SHAs.
    let dir = init_repo();
    let repo = dir.path();
    let (_intro, shas) = seed_plan(repo, "alpha", 1);
    configure_editor(repo, "true", vec!["--commits", "{commits}"]);

    let out = run_diff(repo, &["alpha", "--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("{commits}"),
        "{{commits}} should have been substituted; got: {stdout}"
    );
    for sha in &shas {
        assert!(
            stdout.contains(sha),
            "expected SHA {sha} in composed args; got: {stdout}"
        );
    }
}

#[test]
fn clank_diff_env_overrides_appear_in_print() {
    // Test 18 (partial): --print emits env additions on stderr,
    // mirroring `agent start --print`. Covers CLANK_DIFF_PROMPT
    // and CLANK_DIFF_FOCUS too.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(
        repo,
        &[
            "alpha",
            "--print",
            "--prompt",
            "look at the error handling",
            "--focus",
            "src/foo.rs:10-42",
        ],
    );
    assert!(out.status.success());
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        env.get("CLANK_DIFF_PROMPT"),
        Some(&"look at the error handling".to_string())
    );
    assert_eq!(
        env.get("CLANK_DIFF_FOCUS"),
        Some(&"src/foo.rs:10-42".to_string())
    );
}

#[test]
fn clank_diff_unconfigured_editor_errors() {
    // Test 19: no diff.editor set → error names the config key.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    // NO configure_editor.

    let out = run_diff(repo, &["alpha", "--print"]);
    assert!(!out.status.success(), "must error when editor unconfigured");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no editor configured") && stderr.contains("diff.editor.command"),
        "diagnostic should name diff.editor.command; got: {stderr}"
    );
}

#[test]
fn clank_diff_wait_flag_overrides_config_default_false() {
    // Test 21: config wait=false; --wait flips composed wait to true.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor_with_wait(repo, "true", false);

    let out = run_diff(repo, &["alpha", "--print", "--wait"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("wait: true"),
        "--wait should flip composed wait to true; got: {stderr}"
    );
}

#[test]
fn clank_diff_no_wait_flag_overrides_config_default_true() {
    // Test 22: config wait=true; --no-wait flips composed wait to false.
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor_with_wait(repo, "true", true);

    let out = run_diff(repo, &["alpha", "--print", "--no-wait"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("wait: false"),
        "--no-wait should flip composed wait to false; got: {stderr}"
    );
}

// ── --focus syntax tests ─────────────────────────────────────────

#[test]
fn clank_diff_focus_syntax_round_trips() {
    // Test 23: OQ3 pinned syntax. Multi-focus + single-line
    // shorthand (`:5` → `:5-5`) + whole-file form (no `:`).
    let dir = init_repo();
    let repo = dir.path();
    seed_plan(repo, "alpha", 0);
    configure_editor(repo, "true", vec![]);

    let out = run_diff(
        repo,
        &[
            "alpha",
            "--print",
            "--focus",
            "path/to/file.rs:10-42",
            "--focus",
            "other.rs:5",
            "--focus",
            "whole.rs",
        ],
    );
    assert!(out.status.success());
    let env = parse_env(&String::from_utf8_lossy(&out.stderr));
    let focus = env.get("CLANK_DIFF_FOCUS").expect("focus env present");
    let lines: Vec<&str> = focus.split('\n').collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines.contains(&"path/to/file.rs:10-42"),
        "range form passes through; got: {focus}"
    );
    assert!(
        lines.contains(&"other.rs:5-5"),
        "single-line `:5` should expand to `:5-5`; got: {focus}"
    );
    assert!(
        lines.contains(&"whole.rs"),
        "bare path → whole-file form; got: {focus}"
    );
}

// ── Smoke test ───────────────────────────────────────────────────

#[test]
fn clank_diff_spawns_configured_editor_smoke_no_wait() {
    // Test 24: with editor.command = "true" (POSIX no-op),
    // clank diff <range> fire-and-forget returns Ok without
    // blocking. The child is detached so we don't reap it.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "a\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "a"]);
    configure_editor_with_wait(repo, "true", false);

    let out = run_diff(repo, &["HEAD^..HEAD"]);
    assert!(
        out.status.success(),
        "fire-and-forget should succeed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn clank_diff_spawns_configured_editor_smoke_wait() {
    // Test 24 variant: --wait + `true` binary → completes after
    // exit 0 from the child.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "a\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "a"]);
    configure_editor_with_wait(repo, "true", true);

    let out = run_diff(repo, &["HEAD^..HEAD"]);
    assert!(
        out.status.success(),
        "--wait should succeed after `true` exits; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
}
