//! Integration tests for `clank doctor`'s unbound-reviewer warning.

mod common;

use common::TestEnv;
use std::path::Path;

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn init_repo() -> TestEnv {
    TestEnv::init()
}

/// Build a skeleton with optional session. Skeletons are
/// STATE-ONLY under `teams-based-agent-registration`.
fn skeleton(
    auto_mode: clank_core::vocab::AutoMode,
    session: Option<clank_core::agent_config::Session>,
) -> clank_core::agent_config::AgentConfig {
    clank_core::agent_config::AgentConfig {
        auto_mode: Some(auto_mode),
        wfw_timeout: None,
        session,
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

/// Register agents through the REAL cores
/// (`dogfood-init-setup-in-tests`). The `Role::Master` entry is
/// the team master, else a synthetic `boss` master is added so
/// resolution succeeds; all others become commit reviewers.
fn register_agents(env: &TestEnv, agents: &[(&str, clank_core::vocab::Role)]) {
    use clank_core::vocab::Role;
    let master = agents
        .iter()
        .find(|(_, r)| *r == Role::Master)
        .map(|(l, _)| *l);
    let master_label = master.unwrap_or("boss");
    let reviewers: Vec<&str> = agents
        .iter()
        .filter(|(l, _)| Some(*l) != master)
        .map(|(l, _)| *l)
        .collect();
    env.register_team(master_label, &reviewers, &[]);
}

/// Register a single master agent carrying a launch profile — via
/// the real cores: add it to the repo roster (inline, with the
/// launch profile) then designate it master. `register_team` uses
/// a default desc, so this case is built explicitly.
fn register_agent_with_launch(
    env: &TestEnv,
    label: &str,
    _role: clank_core::vocab::Role,
    launch: clank_core::agent_config::LaunchConfig,
) {
    use clank::cli::teams_config::{AgentDescription, RosterRole};
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;
    let lbl = AgentLabel::parse(label).unwrap();
    clank::cli::agent::add_repo_roster_agent(
        env.repo(),
        &lbl,
        AgentDescription {
            tool: Tool::Claude,
            launch: Some(launch),
            initial_prompt: None,
        },
        RosterRole::Commit,
    )
    .unwrap();
    clank::cli::agent::set_repo_master(env.repo(), &lbl).unwrap();
}

/// Run doctor's repo-scope checks IN-PROCESS
/// (`dogfood-init-setup-in-tests` Phase B) and return the same
/// JSON array `clank doctor --json` emits — no binary spawn. The
/// agent checks these tests navigate live in the repo section.
fn run_doctor(env: &TestEnv) -> serde_json::Value {
    let results = clank::cli::doctor::repo_checks(env.repo(), Some(env.home()));
    clank::cli::doctor::checks_to_json(&results)
}

#[test]
fn doctor_warns_on_unbound_reviewer() {
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(&env, &[("ruthless", clank_core::vocab::Role::Reviewer)]);
    write_skeleton(
        repo,
        "ruthless",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let parsed = run_doctor(&env);
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
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(&env, &[("codex", clank_core::vocab::Role::Reviewer)]);
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

    let parsed = run_doctor(&env);
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
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(&env, &[("lloyd", clank_core::vocab::Role::Master)]);
    write_skeleton(
        repo,
        "lloyd",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let parsed = run_doctor(&env);
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
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    // Launch lives on the declaration per
    // agent-add-cli-and-repo-scope Phase 1.
    register_agent_with_launch(
        &env,
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

    let parsed = run_doctor(&env);
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
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(&env, &[("phantom", clank_core::vocab::Role::Reviewer)]);
    // No skeleton written.

    let parsed = run_doctor(&env);
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
        msg.contains("registered") && msg.contains("reviewer"),
        "diagnostic should name the registered role; got: {msg}"
    );
    assert!(
        msg.contains("config.json"),
        "diagnostic should mention the missing skeleton path; got: {msg}"
    );
    assert!(
        msg.contains("clank as"),
        "diagnostic should mention `clank as` as the bind fix; got: {msg}"
    );
}

#[test]
fn doctor_warns_on_orphan_skeleton() {
    // Phase 7: per-agent dir exists but label NOT in the merged
    // declaration (neither repo-scope nor user-scope). Doctor
    // surfaces a Warn naming the label + diagnostic mentioning
    // `clank agent add` AND `rm -rf`.
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    // Registered team is just a master (`boss`); the `orphan`
    // skeleton below is NOT in the registered set.
    register_agents(&env, &[("boss", clank_core::vocab::Role::Master)]);
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

    let parsed = run_doctor(&env);
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
        msg.contains("agent roster"),
        "diagnostic should mention the agent isn't in the agent roster; got: {msg}"
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
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    register_agents(&env, &[("ruthless", clank_core::vocab::Role::Reviewer)]);
    write_skeleton(
        repo,
        "ruthless",
        &skeleton(clank_core::vocab::AutoMode::Off, None),
    );

    let parsed = run_doctor(&env);
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

// ── zellij layout template validation (zellij-layout-config-around-agent-panes) ──

/// Write a user-scope config with a zellij layout template via the
/// typed schema (no raw JSON literals).
fn write_user_zellij_template(env: &TestEnv, template: &str) {
    use clank::cli::teams_config::ZellijSection;
    let mut cfg = clank::cli::team::read_user_config(env.home()).unwrap();
    cfg.zellij = Some(ZellijSection {
        layout: Some(template.to_string()),
    });
    let path = env.home().join(".clank/config.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
}

fn zellij_check(results: &[clank::cli::doctor::CheckResult]) -> Option<String> {
    let json = clank::cli::doctor::checks_to_json(results);
    json.as_array().and_then(|arr| {
        arr.iter()
            .find(|c| c["name"] == "zellij layout template")
            .map(|c| format!("{} {}", c["status"], c["message"]))
    })
}

#[test]
fn doctor_validates_broken_zellij_template_at_doctor_time() {
    let env = init_repo();
    write_user_zellij_template(&env, "layout { pane "); // invalid KDL
    let results = clank::cli::doctor::repo_checks(env.repo(), Some(env.home()));
    let line = zellij_check(&results).expect("zellij template check present");
    assert!(
        line.contains("fail") && line.contains("not valid KDL"),
        "broken template must FAIL at doctor time; got: {line}"
    );
}

#[test]
fn doctor_flags_template_missing_the_marker() {
    let env = init_repo();
    write_user_zellij_template(&env, "layout {\n    pane\n}\n");
    let results = clank::cli::doctor::repo_checks(env.repo(), Some(env.home()));
    let line = zellij_check(&results).expect("zellij template check present");
    assert!(
        line.contains("fail") && line.contains("clank_agents"),
        "marker-less template must FAIL naming the marker; got: {line}"
    );
}

#[test]
fn doctor_passes_valid_zellij_template() {
    let env = init_repo();
    write_user_zellij_template(&env, "layout {\n    clank_agents\n}\n");
    let results = clank::cli::doctor::repo_checks(env.repo(), Some(env.home()));
    let line = zellij_check(&results).expect("zellij template check present");
    assert!(line.contains("ok"), "valid template passes; got: {line}");
}

#[test]
fn doctor_skips_zellij_check_when_unconfigured() {
    let env = init_repo();
    let results = clank::cli::doctor::repo_checks(env.repo(), Some(env.home()));
    assert!(
        zellij_check(&results).is_none(),
        "no zellij config → no check emitted"
    );
}
