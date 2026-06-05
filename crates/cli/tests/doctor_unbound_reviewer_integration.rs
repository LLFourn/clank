//! Integration tests for `clank doctor`'s unbound-reviewer warning.

use std::path::Path;
use std::process::Command;

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

/// Build a skeleton with optional session.
fn skeleton(
    auto_mode: clank_core::vocab::AutoMode,
    session: Option<clank_core::agent_config::Session>,
) -> clank_core::agent_config::AgentConfig {
    clank_core::agent_config::AgentConfig {
        auto_mode,
        role: clank_core::vocab::Role::Reviewer, // skeleton role unused; declaration is source of truth
        wfw_timeout: None,
        session,
        launch: None,
    }
}

/// Build a bound session.
fn session(tool: clank_core::vocab::Tool, id: &str) -> clank_core::agent_config::Session {
    clank_core::agent_config::Session {
        id: clank_core::ids::SessionId::parse(id).unwrap(),
        tool,
        updated_at: "2026-06-04T12:00:00Z".to_string(),
    }
}

/// Write the per-machine skeleton via the existing typed
/// save_agent_config — no JSON literals.
fn write_skeleton(repo: &Path, label: &str, cfg: &clank_core::agent_config::AgentConfig) {
    clank::agent_store::save_agent_config(
        repo,
        &clank_core::ids::AgentLabel::parse(label).unwrap(),
        cfg,
    )
    .unwrap();
}

/// Register agents in the repo-scope declaration via typed struct.
fn register_agents(repo: &Path, agents: &[(&str, clank_core::vocab::Role)]) {
    let decls: Vec<clank::cli::config::DefaultAgent> = agents
        .iter()
        .map(|(label, role)| clank::cli::config::DefaultAgent {
            label: clank_core::ids::AgentLabel::parse(label).unwrap(),
            role: *role,
            tool: None,
            launch: None,
        })
        .collect();
    let file = clank::cli::config::RepoConfigFile {
        agents: Some(decls),
        ..Default::default()
    };
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
}

fn register_agent_with_launch(
    repo: &Path,
    label: &str,
    role: clank_core::vocab::Role,
    launch: clank_core::agent_config::LaunchConfig,
) {
    let file = clank::cli::config::RepoConfigFile {
        agents: Some(vec![clank::cli::config::DefaultAgent {
            label: clank_core::ids::AgentLabel::parse(label).unwrap(),
            role,
            tool: None,
            launch: Some(launch),
        }]),
        ..Default::default()
    };
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
}

fn run_doctor(repo: &Path) -> std::process::Output {
    Command::new(clank_bin())
        .arg("doctor")
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .expect("spawn clank doctor")
}

#[test]
fn doctor_warns_on_unbound_reviewer() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(repo, &[("ruthless", clank_core::vocab::Role::Reviewer)]);
    write_skeleton(
        repo,
        "ruthless",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("doctor --json should be valid JSON");
    let checks = parsed.as_array().expect("doctor --json should be array");
    let ruthless_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: ruthless"))
        .expect("expected an agent: ruthless check entry");
    assert_eq!(
        ruthless_check["status"], "warn",
        "unbound reviewer should be Warn; got {ruthless_check}"
    );
    let msg = ruthless_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("unbound"),
        "warning should mention 'unbound'; got `{msg}`"
    );
    assert!(
        msg.contains("clank as ruthless"),
        "warning should suggest the fix command; got `{msg}`"
    );
}

#[test]
fn doctor_does_not_warn_on_bound_reviewer() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(repo, &[("codex", clank_core::vocab::Role::Reviewer)]);
    write_skeleton(
        repo,
        "codex",
        &skeleton(
            clank_core::vocab::AutoMode::On,
            Some(session(
                clank_core::vocab::Tool::Codex,
                "11111111-1111-1111-1111-111111111111",
            )),
        ),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let codex_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: codex"))
        .expect("expected agent: codex check");
    assert_eq!(
        codex_check["status"], "ok",
        "bound reviewer should be Ok; got {codex_check}"
    );
}

#[test]
fn doctor_warns_on_unbound_master_symmetrically() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(repo, &[("lloyd", clank_core::vocab::Role::Master)]);
    write_skeleton(
        repo,
        "lloyd",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let lloyd_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: lloyd"))
        .expect("expected agent: lloyd check");
    assert_eq!(
        lloyd_check["status"], "warn",
        "unbound master should be Warn (symmetric); got {lloyd_check}"
    );
    let msg = lloyd_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("unbound"),
        "warning should mention 'unbound'; got `{msg}`"
    );
}

#[test]
fn doctor_warns_when_launch_command_missing_from_path() {
    // Phase C of agent-config-and-start: doctor surfaces a Warn
    // when an agent's launch.command isn't on $PATH. Catches
    // typos before the user tries `clank agent start <name>`.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    // Launch lives on the declaration per
    // agent-add-cli-and-repo-scope Phase 1.
    register_agent_with_launch(
        repo,
        "codex",
        clank_core::vocab::Role::Reviewer,
        clank_core::agent_config::LaunchConfig {
            command: Some("definitely-not-installed-anywhere".to_string()),
            args: Vec::new(),
            env: Default::default(),
        },
    );
    write_skeleton(
        repo,
        "codex",
        &skeleton(
            clank_core::vocab::AutoMode::On,
            Some(session(
                clank_core::vocab::Tool::Codex,
                "11111111-1111-1111-1111-111111111111",
            )),
        ),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let launch_warn = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: codex launch"))
        .expect("expected agent: codex launch check");
    assert_eq!(launch_warn["status"], "warn");
    let msg = launch_warn["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not found on $PATH"),
        "warning should mention $PATH; got: {msg}"
    );
    assert!(
        msg.contains("definitely-not-installed-anywhere"),
        "warning should name the offending command; got: {msg}"
    );
}

#[test]
fn doctor_warns_on_missing_skeleton() {
    // Phase 7: declaration says agent X is registered, but no
    // skeleton at .clank/agents/<X>/config.json. Doctor surfaces
    // a Warn naming the label + diagnostic mentioning
    // `clank init`.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(repo, &[("phantom", clank_core::vocab::Role::Reviewer)]);
    // No skeleton written.

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let phantom_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: phantom"))
        .expect("expected agent: phantom check");
    assert_eq!(
        phantom_check["status"], "warn",
        "missing skeleton must be Warn; got {phantom_check}"
    );
    let msg = phantom_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("in merged declaration"),
        "diagnostic should mention `in merged declaration`; got: {msg}"
    );
    assert!(
        msg.contains("missing"),
        "diagnostic should mention skeleton missing; got: {msg}"
    );
    assert!(
        msg.contains("clank init"),
        "diagnostic should mention `clank init` as the fix; got: {msg}"
    );
}

#[test]
fn doctor_warns_on_orphan_skeleton() {
    // Phase 7: per-agent dir exists but label NOT in the merged
    // declaration (neither repo-scope nor user-scope). Doctor
    // surfaces a Warn naming the label + diagnostic mentioning
    // `clank agent add` AND `rm -rf`.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    // Empty declaration (explicit empty override).
    let file = clank::cli::config::RepoConfigFile {
        agents: Some(Vec::new()),
        ..Default::default()
    };
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
    // Orphan skeleton.
    write_skeleton(
        repo,
        "orphan",
        &skeleton(
            clank_core::vocab::AutoMode::Off,
            Some(session(
                clank_core::vocab::Tool::Claude,
                "11111111-1111-1111-1111-111111111111",
            )),
        ),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let orphan_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: orphan"))
        .expect("expected agent: orphan check");
    assert_eq!(
        orphan_check["status"], "warn",
        "orphan skeleton must be Warn; got {orphan_check}"
    );
    let msg = orphan_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("not in the merged agent declaration"),
        "diagnostic should mention `not in the merged agent declaration`; got: {msg}"
    );
    assert!(
        msg.contains("clank agent add"),
        "diagnostic should mention `clank agent add` as a recovery option; got: {msg}"
    );
    assert!(
        msg.contains("rm -rf"),
        "diagnostic should mention `rm -rf` as a removal option; got: {msg}"
    );
}

#[test]
fn doctor_warn_for_unbound_does_not_introduce_new_fail() {
    // The unbound-reviewer warning must not escalate to Fail status
    // for the agent entry itself. (Doctor's overall exit code may
    // still be 1 due to pre-existing baseline Fail checks like
    // "no session detected" when run outside an agent; that's
    // unchanged by this plan.)
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(repo, &[("ruthless", clank_core::vocab::Role::Reviewer)]);
    write_skeleton(
        repo,
        "ruthless",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let ruthless_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: ruthless"))
        .expect("expected agent: ruthless check");
    assert_eq!(
        ruthless_check["status"], "warn",
        "unbound reviewer is Warn (not Fail); got {ruthless_check}"
    );
}
