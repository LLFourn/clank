//! Integration tests for `clank open`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

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
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn run_open(path: &Path, home: &Path) -> Value {
    let out = Command::new(clank_bin())
        .args(["open", "dry", "--json"])
        .arg(path)
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank open dry");
    assert!(
        out.status.success(),
        "clank open dry failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn run_open_raw(arg: &str, home: &Path) -> Value {
    let out = Command::new(clank_bin())
        .args(["open", "dry", "--json", arg])
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank open");
    assert!(
        out.status.success(),
        "clank open failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn rec_kinds(v: &Value) -> Vec<String> {
    v["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn path_missing_returns_clean_response() {
    let home = tempfile::tempdir().unwrap();
    let nonexistent = home.path().join("does-not-exist-xyz");
    let v = run_open(&nonexistent, home.path());
    assert_eq!(v["state"], "path_missing");
    assert!(v.get("repo_root").is_none());
    assert!(v.get("git").is_none());
    assert!(v.get("clank").is_none());
    let kinds = rec_kinds(&v);
    assert_eq!(kinds.first().map(String::as_str), Some("init_directory"));
}

#[test]
fn path_not_directory() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("a.txt");
    std::fs::write(&file, "hi").unwrap();
    let v = run_open(&file, home.path());
    assert_eq!(v["state"], "path_not_directory");
    assert!(v["recommendations"].as_array().unwrap().is_empty());
}

#[test]
fn empty_directory() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let v = run_open(target.path(), home.path());
    assert_eq!(v["state"], "empty_directory");
    let kinds = rec_kinds(&v);
    assert_eq!(kinds.first().map(String::as_str), Some("git_init"));
}

#[test]
fn directory_not_git() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("README.md"), "x").unwrap();
    let v = run_open(target.path(), home.path());
    assert_eq!(v["state"], "directory_not_git");
}

#[test]
fn git_without_clank() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    assert_eq!(v["state"], "clank_init_needed");
    assert_eq!(v["git"]["is_repo"], true);
    assert_eq!(v["git"]["head_branch"], "main");
    assert_eq!(v["git"]["dirty"], false);
    let kinds = rec_kinds(&v);
    assert_eq!(kinds, vec!["clank_init", "bind_agent"]);
}

#[test]
fn clank_field_populated_when_clank_dir_present_without_config_json() {
    // Regression: in the old model, a repo with `.clank/` but no
    // `config.json` was misclassified as GitWithoutClank — clank
    // info was dropped entirely. Under the new model the state
    // is `clank_init_needed` (init artifacts are still missing),
    // but the `clank` field must still be populated.
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/.gitkeep", "");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);
    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    assert!(
        v.get("clank").is_some() && !v["clank"].is_null(),
        "clank field must be populated when .clank/ is present; got {v}"
    );
}

#[test]
fn linked_worktree_without_config_json_populates_clank_field() {
    // Pin the original bug shape: linked worktree, `.clank/` in
    // the linked tree only, no `config.json`. State is now
    // `clank_init_needed` (the linked tree's hooks/gitignore/
    // permissions aren't init'd) but `clank` info populates.
    let home = tempfile::tempdir().unwrap();
    let main = init_repo();
    let main_repo = main.path();
    write(main_repo, "README.md", "x");
    git(main_repo, &["add", "-A"]);
    git(main_repo, &["commit", "--quiet", "-m", "seed"]);

    let wt_parent = tempfile::tempdir().unwrap();
    let wt_path = wt_parent.path().join("linked");
    git(
        main_repo,
        &[
            "worktree",
            "add",
            wt_path.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );

    write(&wt_path, ".clank/plans/.gitkeep", "");

    let v = run_open(&wt_path, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    assert_eq!(v["git"]["is_linked_worktree"], true);
    assert!(
        v.get("clank").is_some() && !v["clank"].is_null(),
        "clank field must be populated in linked worktree; got {v}"
    );
}

#[test]
fn clank_initialized_with_master_and_reviewer() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/claude/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Master,
            session: Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse("00000000-0000-4000-8000-000000000001")
                    .unwrap(),
                tool: clank_core::vocab::Tool::Claude,
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            }),
            ..Default::default()
        })
        .unwrap(),
    );
    write(
        repo,
        ".clank/agents/codex/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Reviewer,
            session: Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse("00000000-0000-4000-8000-000000000002")
                    .unwrap(),
                tool: clank_core::vocab::Tool::Codex,
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            }),
            ..Default::default()
        })
        .unwrap(),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    assert_eq!(v["clank"]["master_agents"], serde_json::json!(["claude"]));
    assert_eq!(v["clank"]["agents"].as_array().unwrap().len(), 2);
    // No JSONL on disk under fake HOME → not resumable, so each
    // agent gets a bind_agent (no resume_agent). The init gaps
    // also surface a clank_init recommendation; the count of
    // bind_agents specifically must be 2.
    let kinds = rec_kinds(&v);
    let bind_count = kinds.iter().filter(|k| *k == "bind_agent").count();
    assert_eq!(bind_count, 2, "expected 2 bind_agent recs; got {kinds:?}");
    assert!(
        !kinds.iter().any(|k| k == "resume_agent"),
        "no resume_agent should appear without on-disk JSONL"
    );
}

#[test]
fn clank_initialized_subdir_resolves_to_repo_root() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(repo, "src/lib.rs", "fn main() {}\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(&repo.join("src"), home.path());
    let opened = v["opened_path"].as_str().unwrap();
    let root = v["repo_root"].as_str().unwrap();
    assert!(
        opened.ends_with("src"),
        "opened_path should be subdir; got {opened}"
    );
    assert_ne!(opened, root);
    assert!(
        root.ends_with(repo.file_name().unwrap().to_str().unwrap()),
        "repo_root should be the toplevel; got {root}"
    );
}

#[test]
fn master_agents_empty_when_all_reviewers() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/a/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Reviewer,
            ..Default::default()
        })
        .unwrap(),
    );
    write(
        repo,
        ".clank/agents/b/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Reviewer,
            ..Default::default()
        })
        .unwrap(),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    assert_eq!(v["clank"]["master_agents"], serde_json::json!([]));
}

#[test]
fn master_agents_lists_multiple_masters() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/a/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Master,
            ..Default::default()
        })
        .unwrap(),
    );
    write(
        repo,
        ".clank/agents/b/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            role: clank_core::vocab::Role::Master,
            ..Default::default()
        })
        .unwrap(),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    let masters = v["clank"]["master_agents"].as_array().unwrap();
    assert_eq!(masters.len(), 2);
    let mut labels: Vec<&str> = masters.iter().map(|x| x.as_str().unwrap()).collect();
    labels.sort();
    assert_eq!(labels, vec!["a", "b"]);
}

#[test]
fn git_dir_normalized_from_repo_root() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    let git_dir = v["git"]["git_dir"].as_str().unwrap();
    assert!(
        Path::new(git_dir).is_absolute(),
        "git_dir should be absolute: {git_dir}"
    );
    assert!(
        git_dir.ends_with(".git"),
        "git_dir should end with .git: {git_dir}"
    );
}

#[test]
fn resume_recommended_when_session_jsonl_exists() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    let uuid = "00000000-0000-4000-8000-000000000aaa";
    write(
        repo,
        ".clank/agents/claude/config.json",
        &format!(
            r#"{{"session":{{"id":"{uuid}","tool":"claude","updated_at":"2026-01-01T00:00:00Z"}}}}"#
        ),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    // Fake the Claude session file under the test HOME. The
    // inspector accepts ANY project directory under
    // ~/.claude/projects containing <UUID>.jsonl.
    let projects = home.path().join(".claude/projects/somewhere");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(projects.join(format!("{uuid}.jsonl")), "{}\n").unwrap();

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    let resume = recs
        .iter()
        .find(|r| r["kind"] == "resume_agent")
        .unwrap_or_else(|| panic!("no resume_agent recommendation; got {recs:?}"));
    assert_eq!(resume["command_hint"], format!("claude --resume {uuid}"));
    assert_eq!(v["clank"]["agents"][0]["session_resumable"], true);
}

#[test]
fn stale_session_id_falls_back_to_bind_agent() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"session":{"id":"00000000-0000-4000-8000-deadbeefcafe","tool":"claude","updated_at":"2026-01-01T00:00:00Z"}}"#,
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    let bind = recs
        .iter()
        .find(|r| r["kind"] == "bind_agent")
        .unwrap_or_else(|| panic!("no bind_agent recommendation; got {recs:?}"));
    assert_eq!(bind["label"], "claude");
    assert!(
        !recs.iter().any(|r| r["kind"] == "resume_agent"),
        "stale session should fall back to bind, not resume; got {recs:?}"
    );
    assert_eq!(v["clank"]["agents"][0]["session_resumable"], false);
}

#[test]
fn codex_session_recommended_when_rollout_jsonl_exists() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    let uuid = "019e73af-9204-7790-a997-b2e80ee0a6d1";
    write(
        repo,
        ".clank/agents/codex/config.json",
        &format!(
            r#"{{"session":{{"id":"{uuid}","tool":"codex","updated_at":"2026-01-01T00:00:00Z"}}}}"#
        ),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let day_dir = home.path().join(".codex/sessions/2026/05/29");
    std::fs::create_dir_all(&day_dir).unwrap();
    std::fs::write(
        day_dir.join(format!("rollout-2026-05-29T22-22-26-{uuid}.jsonl")),
        "{}\n",
    )
    .unwrap();

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    let resume = recs
        .iter()
        .find(|r| r["kind"] == "resume_agent")
        .unwrap_or_else(|| panic!("no resume_agent recommendation; got {recs:?}"));
    assert_eq!(resume["command_hint"], format!("codex resume {uuid}"));
}

fn gap_kinds(v: &Value) -> Vec<String> {
    v["init_gaps"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|g| g["kind"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn init_needed_when_clank_dir_missing() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    assert_eq!(v["state"], "clank_init_needed");
    let gaps = gap_kinds(&v);
    assert!(
        gaps.iter().any(|g| g == "missing_clank_dir"),
        "expected missing_clank_dir; got {gaps:?}"
    );
}

#[test]
fn init_needed_when_claude_permissions_missing() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/.gitignore",
        "/agents/\n/cache/\n/feedback/\n/queue/\n",
    );
    let v = run_open(repo, home.path());
    let gaps = gap_kinds(&v);
    assert!(
        gaps.iter().any(|g| g == "missing_claude_permissions"),
        "expected missing_claude_permissions; got {gaps:?}"
    );
}

#[test]
fn init_needed_when_post_rewrite_hook_absent() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/.gitignore",
        "/agents/\n/cache/\n/feedback/\n/queue/\n",
    );
    write(
        repo,
        ".claude/settings.local.json",
        r#"{"permissions":{"allow":["Write(.clank/agents/**)","Edit(.clank/agents/**)","Read(.clank/agents/**)"]}}"#,
    );
    let v = run_open(repo, home.path());
    let gaps = gap_kinds(&v);
    assert!(
        gaps.iter().any(|g| g == "missing_post_rewrite_hook"),
        "expected missing_post_rewrite_hook; got {gaps:?}"
    );
}

#[test]
fn legacy_clank_gitignore_is_init_gap() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", "feedback/\ncache/\n");
    let v = run_open(repo, home.path());
    let gaps = gap_kinds(&v);
    assert!(
        gaps.iter().any(|g| g == "missing_clank_gitignore"),
        "legacy body should be an init gap; got {gaps:?}"
    );
}

#[test]
fn foreign_clank_gitignore_is_warning_not_init_gap() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", "totally-foreign-content\n");
    let v = run_open(repo, home.path());
    let gaps = gap_kinds(&v);
    assert!(
        !gaps.iter().any(|g| g == "missing_clank_gitignore"),
        "foreign body should NOT be an init gap (init bails on it); got {gaps:?}"
    );
    let warnings = v["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains(".clank/.gitignore") && s.contains("drifted")
        }),
        "expected drift warning; got {warnings:?}"
    );
}

#[test]
fn foreign_post_rewrite_hook_is_warning_not_init_gap() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    let hook = repo.join(".git/hooks/post-rewrite");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho not-clank\n").unwrap();
    let v = run_open(repo, home.path());
    let gaps = gap_kinds(&v);
    assert!(
        !gaps.iter().any(|g| g == "missing_post_rewrite_hook"),
        "foreign hook should NOT be an init gap (init only warns); got {gaps:?}"
    );
    let warnings = v["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains("post-rewrite") && s.contains("--force-hooks")
        }),
        "expected foreign-hook warning mentioning --force-hooks; got {warnings:?}"
    );
}

#[test]
fn clank_ready_after_full_init() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    // clank init --yes does the full repair sweep.
    let out = Command::new(clank_bin())
        .args(["init", "--yes"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank init");
    assert!(
        out.status.success(),
        "clank init --yes failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_ready", "got {v}");
    assert!(
        v.get("init_gaps").is_none(),
        "init_gaps should be omitted when empty; got {v}"
    );
    let recs = v["recommendations"].as_array().unwrap();
    assert!(
        !recs.iter().any(|r| r["kind"] == "clank_init"),
        "no clank_init recommendation when ready; got {recs:?}"
    );
}

#[test]
fn clank_init_recommendation_carries_gaps_array() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    let recs = v["recommendations"].as_array().unwrap();
    let init_rec = recs
        .iter()
        .find(|r| r["kind"] == "clank_init")
        .unwrap_or_else(|| panic!("no clank_init rec; got {recs:?}"));
    let gaps = init_rec["gaps"].as_array().expect("gaps array");
    assert!(
        !gaps.is_empty(),
        "gaps should be populated for init-needed state"
    );
    let top_gap_kinds = gap_kinds(&v);
    let rec_gaps: Vec<String> = gaps
        .iter()
        .map(|g| g.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        rec_gaps, top_gap_kinds,
        "recommendation's gaps must mirror top-level init_gaps kinds in order"
    );
}

#[test]
fn clank_ready_unaffected_by_unbound_agents() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    let out = Command::new(clank_bin())
        .args(["init", "--yes"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank init");
    assert!(
        out.status.success(),
        "clank init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // No `.clank/agents/<label>/` dirs created (init runs
    // without a bound agent session under the test env).
    let agents_root = repo.join(".clank/agents");
    if agents_root.exists() {
        // Remove any auto-created agent dirs so the test pins
        // the "no known agents" branch deterministically.
        for entry in std::fs::read_dir(&agents_root).unwrap().flatten() {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }

    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_ready", "got {v}");
    let agents = v["clank"]["agents"].as_array().expect("agents array");
    assert!(agents.is_empty(), "expected empty agents; got {agents:?}");
    let kinds = rec_kinds(&v);
    assert_eq!(
        kinds,
        vec!["bind_agent"],
        "ready repo with no agents must surface a single bind_agent rec"
    );
}

#[test]
fn warning_for_ancestor_gitignore_excluding_clank_paths() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    // Root gitignore EXCLUDES .clank/ entirely — exactly the
    // kind of misconfiguration init only warns about.
    write(repo, ".gitignore", ".clank/\n");

    let v = run_open(repo, home.path());
    let warnings = v["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains("ancestor .gitignore") && s.contains(".clank/plans")
        }),
        "expected ancestor-gitignore warning; got {warnings:?}"
    );
    let gaps = gap_kinds(&v);
    // This is NOT a fixable InitGap — init doesn't repair it.
    assert!(
        !gaps.iter().any(|g| g.contains("gitignore_entries")),
        "ancestor-gitignore exclusion must NOT be an InitGap; got {gaps:?}"
    );
}

#[test]
fn warnings_field_omitted_when_empty() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let v = run_open(target.path(), home.path());
    assert!(v.get("warnings").is_none());
}

#[test]
fn linked_worktree_git_dir_outside_repo_root() {
    let home = tempfile::tempdir().unwrap();
    let main = init_repo();
    let main_repo = main.path();
    std::fs::write(main_repo.join("README.md"), "x").unwrap();
    git(main_repo, &["add", "-A"]);
    git(main_repo, &["commit", "--quiet", "-m", "seed"]);

    let linked_dir = tempfile::tempdir().unwrap();
    let linked = linked_dir.path();
    // git worktree add requires a non-existent path. Drop the
    // tempdir's bookkeeping and let git create it.
    let linked_path = linked.join("wt");
    git(
        main_repo,
        &[
            "worktree",
            "add",
            linked_path.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );

    let v = run_open(&linked_path, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    assert_eq!(v["git"]["is_linked_worktree"], true);
    let git_dir = v["git"]["git_dir"].as_str().unwrap();
    let repo_root = v["repo_root"].as_str().unwrap();
    assert!(
        !git_dir.starts_with(repo_root),
        "linked-worktree git_dir should live outside repo_root: git_dir={git_dir} repo_root={repo_root}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_canonicalizes_to_target() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let link_parent = tempfile::tempdir().unwrap();
    let link = link_parent.path().join("link-to-repo");
    std::os::unix::fs::symlink(repo, &link).unwrap();

    let v = run_open(&link, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    let opened = v["opened_path"].as_str().unwrap();
    let canonical_target = dunce::canonicalize(repo)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(
        opened, canonical_target,
        "opened_path should be canonical target, not the symlink path"
    );
}

#[test]
fn fold_failure_pushes_warning_but_keeps_clank_initialized() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    // Point the branch ref at a non-existent object so `git log
    // HEAD` errors — git still recognizes this as a repo (so we
    // hit the ClankInitialized path) but the fold blows up
    // walking history.
    let head_ref = repo.join(".git/refs/heads/main");
    std::fs::write(&head_ref, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n").unwrap();

    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_init_needed");
    let warnings = v["warnings"].as_array().expect("warnings present");
    assert!(
        warnings.iter().any(|w| {
            let s = w.as_str().unwrap_or("");
            s.contains("fold failed")
        }),
        "expected a fold-failure warning, got: {warnings:?}"
    );
    assert_eq!(v["clank"]["active_plans"], 0);
}

#[test]
fn lex_clean_for_missing_relative_path() {
    let home = tempfile::tempdir().unwrap();
    // Relative path; should be lex-cleaned to an absolute one
    // under cwd. The inspector does NOT canonicalize missing
    // paths, so this exercises the lex-clean branch.
    let v = run_open_raw("./missing-relative-xyz", home.path());
    assert_eq!(v["state"], "path_missing");
    assert!(
        Path::new(v["opened_path"].as_str().unwrap()).is_absolute(),
        "opened_path should be absolute for missing relative input"
    );
}
