//! Integration tests for `clank agent add/remove/promote` (Phase 4
//! of `agent-add-cli-and-repo-scope`).
//!
//! Tests construct config files via the typed `RepoConfigFile` /
//! `UserConfigFile` serde structs to dogfood the new write surface
//! and so schema changes show up as compile errors instead of
//! silent JSON drift.

use std::path::Path;
use std::process::Command;

use clank::cli::config::{DefaultAgent, RepoConfigFile, UserConfigFile};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Role;

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

/// Repo + isolated HOME (for `--global` tests).
struct Env {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path();
        git(path, &["init", "--quiet", "--initial-branch=main"]);
        git(path, &["config", "user.email", "test@test"]);
        git(path, &["config", "user.name", "test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        Self {
            repo,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn repo(&self) -> &Path {
        self.repo.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Spawn `clank agent <subcommand> ...` with HOME and
    /// CLANK_AGENT vars unset so identity resolution doesn't
    /// pollute the test.
    fn agent(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(clank_bin());
        cmd.arg("agent")
            .args(args)
            .arg("--repo")
            .arg(self.repo())
            .env("HOME", self.home())
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CLANK_AGENT");
        cmd.output().expect("spawn clank agent")
    }
}

fn read_repo_config(repo: &Path) -> RepoConfigFile {
    let path = repo.join(".clank/config.json");
    let body = std::fs::read_to_string(&path).expect("repo config exists");
    serde_json::from_str(&body).expect("repo config parses as RepoConfigFile")
}

fn read_user_config(home: &Path) -> UserConfigFile {
    let path = home.join(".clank/config.json");
    let body = std::fs::read_to_string(&path).expect("user config exists");
    serde_json::from_str(&body).expect("user config parses as UserConfigFile")
}

fn agent_decl(label: &str, role: Role) -> DefaultAgent {
    DefaultAgent {
        label: AgentLabel::parse(label).unwrap(),
        role,
        tool: None,
        launch: None,
        initial_prompt: None,
    }
}

fn write_repo_config(repo: &Path, file: &RepoConfigFile) {
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(file).unwrap(),
    )
    .unwrap();
}

fn write_user_config(home: &Path, file: &UserConfigFile) {
    std::fs::create_dir_all(home.join(".clank")).unwrap();
    std::fs::write(
        home.join(".clank/config.json"),
        serde_json::to_string_pretty(file).unwrap(),
    )
    .unwrap();
}

// ── Phase 4 acceptance: clank agent add ──────────────────────────

#[test]
fn clank_agent_add_writes_list_entry_and_skeleton() {
    let env = Env::new();
    let out = env.agent(&[
        "add",
        "codex",
        "--tool",
        "codex",
        "--launch-cmd",
        "codex",
        "--launch-arg",
        "--profile",
        "--launch-arg",
        "deep",
    ]);
    assert!(
        out.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Declaration: full DefaultAgent (label + role + tool + launch).
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.expect("agents key present");
    assert_eq!(agents.len(), 1);
    let entry = &agents[0];
    assert_eq!(entry.label.as_str(), "codex");
    assert_eq!(entry.role, Role::Reviewer);
    assert_eq!(entry.tool, Some(clank_core::vocab::Tool::Codex));
    let launch = entry.launch.as_ref().expect("launch present");
    assert_eq!(launch.command.as_deref(), Some("codex"));
    assert_eq!(
        launch.args,
        vec!["--profile".to_string(), "deep".to_string()]
    );
    // Skeleton: per-machine defaults (no role, no launch).
    let skeleton_path = env.repo().join(".clank/agents/codex/config.json");
    assert!(skeleton_path.exists(), "skeleton must be created");
}

#[test]
fn clank_agent_add_no_launch_flags_leaves_launch_none() {
    // Phase 6 default: declaration entry has launch: None when no
    // --launch-* flag is passed.
    let env = Env::new();
    let out = env.agent(&["add", "alice", "--tool", "claude"]);
    assert!(out.status.success(), "add failed");
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    assert!(
        agents[0].launch.is_none(),
        "no --launch-* flags → declaration.launch must be None; got {:?}",
        agents[0].launch
    );
    // Same default for initial_prompt — no --initial-prompt → None.
    assert!(
        agents[0].initial_prompt.is_none(),
        "no --initial-prompt → declaration.initial_prompt must be None; got {:?}",
        agents[0].initial_prompt
    );
}

#[test]
fn clank_agent_add_initial_prompt_persists_to_config() {
    // Acceptance #3 round-trip: `clank agent add --initial-prompt
    // "..."` must write the field to the declaration. Without this
    // test, a bug in the agent_add write path (e.g., serde skipping
    // the field on serialization) wouldn't be caught — the
    // resolver/compose tests use typed write_repo_agents shortcuts,
    // not the real `clank agent add` CLI surface.
    let env = Env::new();
    let out = env.agent(&[
        "add",
        "alice",
        "--tool",
        "claude",
        "--initial-prompt",
        "custom prompt",
    ]);
    assert!(
        out.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    assert_eq!(
        agents[0].initial_prompt.as_deref(),
        Some("custom prompt"),
        "--initial-prompt must round-trip to declaration; got {:?}",
        agents[0].initial_prompt,
    );
}

#[test]
fn clank_agent_add_empty_initial_prompt_persists_as_empty_string() {
    // `Some("")` is the explicit-disable gesture. It round-trips
    // as Some("") at the config layer; resolve_initial_prompt
    // is what collapses it to None at start-time. Locking in the
    // round-trip semantic separately from the resolver semantic.
    let env = Env::new();
    let out = env.agent(&["add", "alice", "--tool", "claude", "--initial-prompt", ""]);
    assert!(
        out.status.success(),
        "add with empty --initial-prompt failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    assert_eq!(
        agents[0].initial_prompt.as_deref(),
        Some(""),
        "empty --initial-prompt must round-trip as Some(\"\"); got {:?}",
        agents[0].initial_prompt,
    );
}

#[test]
fn clank_agent_add_global_writes_to_user_scope() {
    let env = Env::new();
    let out = env.agent(&["add", "lloyd", "--global", "--role", "master"]);
    assert!(
        out.status.success(),
        "global add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // User-scope has the entry.
    let user = read_user_config(env.home());
    let agents = user.default_agents.expect("default_agents present");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].label.as_str(), "lloyd");
    assert_eq!(agents[0].role, Role::Master);
    // Repo-scope does NOT have a config.json (no per-agent
    // skeleton created at user-scope either).
    assert!(
        !env.repo().join(".clank/config.json").exists(),
        "global add must NOT write repo-scope config.json"
    );
    assert!(
        !env.repo().join(".clank/agents/lloyd").exists(),
        "global add must NOT create per-agent skeleton"
    );
}

#[test]
fn clank_agent_add_refuses_duplicate_at_same_scope() {
    let env = Env::new();
    env.agent(&["add", "codex", "--tool", "codex"]);
    let out = env.agent(&["add", "codex", "--tool", "codex"]);
    assert!(
        !out.status.success(),
        "duplicate add must fail; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already") && stderr.contains("codex"),
        "diagnostic should mention duplicate; got: {stderr}"
    );
    // Filesystem unchanged: still one entry.
    let repo = read_repo_config(env.repo());
    assert_eq!(repo.agents.unwrap().len(), 1);
}

#[test]
fn clank_agent_add_repo_scope_shadows_user_scope_with_notice() {
    // ALLOW shadowing (REPLACE semantics naturally permit it),
    // emit a stderr notice for transparency.
    let env = Env::new();
    write_user_config(
        env.home(),
        &UserConfigFile {
            default_agents: Some(vec![agent_decl("codex", Role::Reviewer)]),
            ..Default::default()
        },
    );
    let out = env.agent(&["add", "codex", "--tool", "codex"]);
    assert!(
        out.status.success(),
        "repo-scope shadow add must succeed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("shadows user-scope") || stderr.contains("shadows"),
        "stderr must include shadow notice; got: {stderr}"
    );
    // Repo-scope now has codex.
    let repo = read_repo_config(env.repo());
    assert_eq!(repo.agents.unwrap()[0].label.as_str(), "codex");
}

#[test]
fn clank_agent_add_global_refuses_when_repo_scope_has_label() {
    // The asymmetric policy: --global add must REFUSE when the
    // label is already in repo-scope (cross-direction collision).
    let env = Env::new();
    write_repo_config(
        env.repo(),
        &RepoConfigFile {
            agents: Some(vec![agent_decl("codex", Role::Reviewer)]),
            ..Default::default()
        },
    );
    let out = env.agent(&["add", "codex", "--global", "--tool", "codex"]);
    assert!(
        !out.status.success(),
        "global add must REFUSE when repo-scope has label; \
         stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // User-scope unchanged.
    assert!(
        !env.home().join(".clank/config.json").exists(),
        "user-scope must not be written on refusal"
    );
}

// ── Phase 4 acceptance: clank agent remove ───────────────────────

#[test]
fn clank_agent_remove_preserves_per_agent_directory() {
    // Per plan Phase 4: remove drops the declaration entry but
    // preserves the per-agent dir (feedback history stays).
    // Critically: load_expected_reviewers no longer returns the
    // removed label.
    let env = Env::new();
    env.agent(&["add", "codex", "--tool", "codex"]);
    // Simulate accumulated feedback history.
    let fb_path = env.repo().join(".clank/agents/codex/feedback/abc1234.md");
    std::fs::create_dir_all(fb_path.parent().unwrap()).unwrap();
    std::fs::write(&fb_path, "APPROVE\n").unwrap();

    let out = env.agent(&["remove", "codex"]);
    assert!(
        out.status.success(),
        "remove failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    // Declaration entry gone.
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap_or_default();
    assert!(
        agents.is_empty() || agents.iter().all(|e| e.label.as_str() != "codex"),
        "remove must drop declaration entry"
    );
    // Feedback history survives.
    assert!(
        fb_path.exists(),
        "feedback history must be preserved on remove"
    );
    // Per-agent skeleton dir also preserved.
    assert!(
        env.repo().join(".clank/agents/codex").exists(),
        "per-agent dir must be preserved on remove"
    );
}

// ── clank agent promote (plan: agent-promote-replaces-set-role) ──

#[test]
fn clank_agent_remove_drops_role_from_resolve_even_when_skeleton_preserved() {
    // Codex caught on ebc5d38: `resolve_role` had a second
    // skeleton fallback that defeated `clank agent remove`.
    // After remove, the skeleton is intentionally preserved (for
    // feedback history) — but `resolve_role` was reading the
    // skeleton's role field, treating a removed agent as still
    // registered.
    //
    // Repro from codex's review:
    //   - explicit empty repo declaration (`agents: []`).
    //   - preserved skeleton with role=master + auto_mode=off.
    //   - `auto status` reports role=master.
    //
    // Fix: declaration is THE source of truth; no second
    // skeleton fallback in `resolve_role`. After remove, the
    // agent gets the DEFAULT role (Reviewers) from
    // `unwrap_or_default`.
    let env = Env::new();
    // Explicit empty repo declaration.
    write_repo_config(
        env.repo(),
        &RepoConfigFile {
            agents: Some(Vec::new()),
            ..Default::default()
        },
    );
    // Preserved skeleton claiming role=master.
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode: clank_core::vocab::AutoMode::Off,
        role: Role::Master,
        wfw_timeout: None,
        session: None,
        launch: None,
    };
    clank::agent_store::save_agent_config(env.repo(), &AgentLabel::parse("codex").unwrap(), &cfg)
        .unwrap();

    // Run `clank auto status --json` for the removed agent.
    let mut cmd = Command::new(clank_bin());
    let out = cmd
        .args(["auto", "status", "--json"])
        .arg("--repo")
        .arg(env.repo())
        .env("HOME", env.home())
        .env("CLANK_AGENT", "codex")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .expect("spawn clank auto status");
    assert!(
        out.status.success(),
        "auto status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(
        parsed["role"], "reviewer",
        "explicit empty declaration must drop the removed agent's role to the default (reviewer); \
         skeleton fallback in resolve_role defeated `clank agent remove`. Got: {parsed}"
    );
}

#[test]
fn clank_agent_promote_flips_reviewer_to_master() {
    // promote edits the DECLARATION entry. Skeleton is untouched.
    // Rename of `clank_agent_set_role_flips_role_in_place` per
    // plan agent-promote-replaces-set-role.
    let env = Env::new();
    env.agent(&["add", "codex", "--tool", "codex"]); // reviewer by default
    let before_skeleton =
        std::fs::read_to_string(env.repo().join(".clank/agents/codex/config.json")).unwrap();

    let out = env.agent(&["promote", "codex"]);
    assert!(
        out.status.success(),
        "promote failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = read_repo_config(env.repo());
    let entry = &repo.agents.unwrap()[0];
    assert_eq!(entry.label.as_str(), "codex");
    assert_eq!(entry.role, Role::Master);
    let after_skeleton =
        std::fs::read_to_string(env.repo().join(".clank/agents/codex/config.json")).unwrap();
    assert_eq!(
        before_skeleton, after_skeleton,
        "skeleton must NOT be modified by promote"
    );
}

#[test]
fn clank_agent_promote_swaps_old_master_to_reviewer() {
    // Pre-state: master=alice, reviewer=bob. promote bob.
    // Post-state: reviewer=alice, master=bob.
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude", "--role", "master"]);
    env.agent(&["add", "bob", "--tool", "codex"]);

    let out = env.agent(&["promote", "bob"]);
    assert!(
        out.status.success(),
        "promote failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("promoted") && stderr.contains("bob") && stderr.contains("alice"),
        "stderr must announce promoted=bob + demoted=alice; got: {stderr}"
    );

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let alice = agents.iter().find(|e| e.label.as_str() == "alice").unwrap();
    let bob = agents.iter().find(|e| e.label.as_str() == "bob").unwrap();
    assert_eq!(alice.role, Role::Reviewer);
    assert_eq!(bob.role, Role::Master);
}

#[test]
fn clank_agent_promote_no_op_when_already_only_master() {
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude", "--role", "master"]);
    env.agent(&["add", "bob", "--tool", "codex"]);

    // mtime baseline of the declaration file.
    let cfg_path = env.repo().join(".clank/config.json");
    let before_mtime = std::fs::metadata(&cfg_path).unwrap().modified().unwrap();
    // Sleep enough that a write would produce a different mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let out = env.agent(&["promote", "alice"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already the only master"),
        "no-op must announce it; got: {stderr}"
    );
    let after_mtime = std::fs::metadata(&cfg_path).unwrap().modified().unwrap();
    assert_eq!(
        before_mtime, after_mtime,
        "no-op promote must not rewrite the declaration"
    );
}

#[test]
fn clank_agent_promote_from_zero_masters() {
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude"]); // reviewer
    env.agent(&["add", "bob", "--tool", "codex"]); // reviewer

    let out = env.agent(&["promote", "alice"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("promoted") && stderr.contains("alice"));
    assert!(
        !stderr.contains("demoted"),
        "zero prior masters means no demotion suffix; got: {stderr}"
    );

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let alice = agents.iter().find(|e| e.label.as_str() == "alice").unwrap();
    let bob = agents.iter().find(|e| e.label.as_str() == "bob").unwrap();
    assert_eq!(alice.role, Role::Master);
    assert_eq!(bob.role, Role::Reviewer);
}

#[test]
fn clank_agent_promote_repairs_pre_existing_multi_master() {
    // Codex d1c1de5 catch: pre-existing multi-master configs
    // (writable by today's buggy `set-role` / `add --role
    // master`) must be repaired by promote. Post-state must
    // have exactly one master regardless of how many existed
    // before.
    let env = Env::new();
    // Hand-write the buggy state — 3 masters + 1 reviewer.
    write_repo_config(
        env.repo(),
        &RepoConfigFile {
            agents: Some(vec![
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("alice").unwrap(),
                    role: Role::Master,
                    tool: Some(clank_core::vocab::Tool::Claude),
                    launch: None,
                    initial_prompt: None,
                },
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("bob").unwrap(),
                    role: Role::Master,
                    tool: Some(clank_core::vocab::Tool::Codex),
                    launch: None,
                    initial_prompt: None,
                },
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("carol").unwrap(),
                    role: Role::Master,
                    tool: Some(clank_core::vocab::Tool::Claude),
                    launch: None,
                    initial_prompt: None,
                },
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("dave").unwrap(),
                    role: Role::Reviewer,
                    tool: Some(clank_core::vocab::Tool::Codex),
                    launch: None,
                    initial_prompt: None,
                },
            ]),
            ..Default::default()
        },
    );

    let out = env.agent(&["promote", "dave"]);
    assert!(
        out.status.success(),
        "promote-repair failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // All three prior masters must appear in the demoted list.
    for prev in ["alice", "bob", "carol"] {
        assert!(
            stderr.contains(prev),
            "demoted list must include {prev}; got: {stderr}"
        );
    }
    assert!(stderr.contains("promoted") && stderr.contains("dave"));

    // Post-state must have exactly one master (dave), all
    // others reviewers.
    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let masters: Vec<_> = agents.iter().filter(|e| e.role == Role::Master).collect();
    assert_eq!(
        masters.len(),
        1,
        "post-state must have exactly one master; got {} ({:?})",
        masters.len(),
        masters.iter().map(|e| e.label.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(masters[0].label.as_str(), "dave");
}

#[test]
fn clank_agent_promote_errors_when_label_absent() {
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude"]);

    let out = env.agent(&["promote", "phantom"]);
    assert!(!out.status.success(), "promote of missing label must error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("phantom") && stderr.contains("not in repo-scope"));
}

// ── add --role master enforces the same invariant ────────────────

#[test]
fn clank_agent_add_role_master_with_no_existing_master() {
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude"]); // reviewer

    let out = env.agent(&["add", "bob", "--tool", "codex", "--role", "master"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("registered") && stderr.contains("bob"));
    assert!(
        !stderr.contains("demoted"),
        "no existing master → no demoted suffix; got: {stderr}"
    );

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let alice = agents.iter().find(|e| e.label.as_str() == "alice").unwrap();
    let bob = agents.iter().find(|e| e.label.as_str() == "bob").unwrap();
    assert_eq!(alice.role, Role::Reviewer);
    assert_eq!(bob.role, Role::Master);
}

#[test]
fn clank_agent_add_role_master_demotes_existing_master() {
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude", "--role", "master"]);

    let out = env.agent(&["add", "bob", "--tool", "codex", "--role", "master"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("demoted") && stderr.contains("alice"),
        "add of second master must demote first; got: {stderr}"
    );

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let alice = agents.iter().find(|e| e.label.as_str() == "alice").unwrap();
    let bob = agents.iter().find(|e| e.label.as_str() == "bob").unwrap();
    assert_eq!(alice.role, Role::Reviewer);
    assert_eq!(bob.role, Role::Master);
}

#[test]
fn clank_agent_add_role_master_repairs_pre_existing_multi_master() {
    // Codex a71bd8e catch (the architectural inconsistency
    // resolution): `add --role master` enforces the same
    // unique-master invariant as promote. Pre-existing multi-
    // master configs are repaired on the next master-add.
    let env = Env::new();
    write_repo_config(
        env.repo(),
        &RepoConfigFile {
            agents: Some(vec![
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("alice").unwrap(),
                    role: Role::Master,
                    tool: Some(clank_core::vocab::Tool::Claude),
                    launch: None,
                    initial_prompt: None,
                },
                clank::cli::config::DefaultAgent {
                    label: AgentLabel::parse("bob").unwrap(),
                    role: Role::Master,
                    tool: Some(clank_core::vocab::Tool::Codex),
                    launch: None,
                    initial_prompt: None,
                },
            ]),
            ..Default::default()
        },
    );

    let out = env.agent(&["add", "carol", "--tool", "claude", "--role", "master"]);
    assert!(
        out.status.success(),
        "add-repair failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for prev in ["alice", "bob"] {
        assert!(
            stderr.contains(prev),
            "demoted list must include {prev}; got: {stderr}"
        );
    }

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let masters: Vec<_> = agents.iter().filter(|e| e.role == Role::Master).collect();
    assert_eq!(
        masters.len(),
        1,
        "post-state must have exactly one master after add-repair"
    );
    assert_eq!(masters[0].label.as_str(), "carol");
}

#[test]
fn clank_agent_add_role_reviewer_does_not_demote_master() {
    // Regression guard: the unique-master helper only fires
    // when the NEW entry's role is master.
    let env = Env::new();
    env.agent(&["add", "alice", "--tool", "claude", "--role", "master"]);

    let out = env.agent(&["add", "bob", "--tool", "codex"]); // reviewer
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("demoted"),
        "add --role reviewer must NOT demote anyone; got: {stderr}"
    );

    let repo = read_repo_config(env.repo());
    let agents = repo.agents.unwrap();
    let alice = agents.iter().find(|e| e.label.as_str() == "alice").unwrap();
    let bob = agents.iter().find(|e| e.label.as_str() == "bob").unwrap();
    assert_eq!(alice.role, Role::Master);
    assert_eq!(bob.role, Role::Reviewer);
}
