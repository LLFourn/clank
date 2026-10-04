//! Real launch ordering with isolated homes and executables, no providers/accounts.
use crate::common::TestEnv;
use clank::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::{Tool, agent_config::LaunchConfig, ids::AgentLabel};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct Fixture {
    env: TestEnv,
    root: PathBuf,
    marker: PathBuf,
    label: AgentLabel,
}
impl Fixture {
    fn new(tool: Tool, config_key: &str) -> Self {
        let env = TestEnv::init();
        let root = env.home().join("selected-profile");
        let marker = env.home().join("launched");
        let script = env.home().join("provider.py");
        std::fs::write(
            &script,
            r#"import os,pathlib,sys,json
if '--version' in sys.argv:
 print('2.1.0 (Claude Code)'); sys.exit(0)
r=pathlib.Path(os.environ['EXPECTED_ROOT'])
for name in ['clank-master','clank-reviewer','clank-pr-review','clank-github']:
 assert (r/'skills'/name/'SKILL.md').is_file(), name
tool=os.environ['EXPECTED_TOOL']
required={'claude':'settings.json','codex':'hooks.json','opencode':'plugin/clank.js'}[tool]
assert (r/required).is_file(), required
if tool == 'claude':
 assert pathlib.Path.cwd() == pathlib.Path(os.environ['EXPECTED_REPO']).resolve()
 profile=os.environ.get('CLAUDE_CONFIG_DIR') or os.environ['HOME']
 config=json.loads((pathlib.Path(profile)/'.claude.json').read_text())
 assert config['projects'][os.environ['EXPECTED_TRUST_ROOT']]['hasTrustDialogAccepted'] is True, config
if tool == 'codex':
 config=(r/'config.toml').read_text()
 assert 'trust_level = "trusted"' in config, config
 assert not (pathlib.Path(os.environ['HOME'])/'.codex/config.toml').exists()
pathlib.Path(os.environ['MARKER']).write_text(tool)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).unwrap();
        let label = AgentLabel::parse("agent").unwrap();
        let launch = LaunchConfig {
            command: Some("/usr/bin/python3".into()),
            args: vec![script.to_string_lossy().into()],
            env: BTreeMap::from([
                ("HOME".into(), env.home().to_string_lossy().into()),
                (config_key.into(), root.to_string_lossy().into()),
                ("EXPECTED_ROOT".into(), root.to_string_lossy().into()),
                ("EXPECTED_TOOL".into(), tool.as_str().into()),
                ("MARKER".into(), marker.to_string_lossy().into()),
                ("EXPECTED_REPO".into(), env.repo().to_string_lossy().into()),
                (
                    "EXPECTED_TRUST_ROOT".into(),
                    dunce::canonicalize(env.repo())
                        .unwrap()
                        .to_string_lossy()
                        .into(),
                ),
            ]),
        };
        clank::cli::agent::add_repo_roster_agent(
            env.repo(),
            &label,
            AgentDescription {
                tool,
                launch: Some(launch),
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        Self {
            env,
            root,
            marker,
            label,
        }
    }
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_clank"));
        command
            .env_clear()
            .env("HOME", self.env.home())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(self.env.repo())
            .args(["agent", "start", self.label.as_str(), "--repo"])
            .arg(self.env.repo());
        command
    }
}

#[test]
fn real_launch_repairs_all_non_grok_providers_before_execution_and_preview_is_read_only() {
    for (tool, key) in [
        (Tool::Claude, "CLAUDE_CONFIG_DIR"),
        (Tool::Codex, "CODEX_HOME"),
        (Tool::OpenCode, "OPENCODE_CONFIG_DIR"),
    ] {
        let f = Fixture::new(tool, key);
        let preview = f.command().arg("--print").output().unwrap();
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert!(!f.root.exists());
        assert!(!f.marker.exists());
        let started = f.command().output().unwrap();
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        assert_eq!(std::fs::read_to_string(&f.marker).unwrap(), tool.as_str());
        assert!(
            String::from_utf8_lossy(&started.stderr).contains("installing missing integration")
        );
        let healthy = f.command().output().unwrap();
        assert!(healthy.status.success());
        assert!(
            !String::from_utf8_lossy(&healthy.stderr).contains("installing missing integration")
        );
    }
}

#[test]
fn failed_repair_keeps_non_grok_fork_intent_and_does_not_execute_provider() {
    let f = Fixture::new(Tool::Codex, "CODEX_HOME");
    std::fs::create_dir_all(&f.root).unwrap();
    std::fs::write(f.root.join("hooks.json"), "not JSON").unwrap();
    let path = clank::cli::fork::fork_spec_path(f.env.repo(), &f.label);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let spec = serde_json::to_vec(&clank::cli::fork::ForkSpec {
        tool: Tool::Codex,
        from_session: Some("ancestor".into()),
        prompt: "orient".into(),
        grok: None,
    })
    .unwrap();
    std::fs::write(&path, &spec).unwrap();
    let result = f.command().output().unwrap();
    assert!(!result.status.success());
    assert_eq!(std::fs::read(path).unwrap(), spec);
    assert_eq!(
        std::fs::read_to_string(f.root.join("hooks.json")).unwrap(),
        "not JSON"
    );
    assert!(!f.marker.exists());
}

#[test]
fn claude_launch_from_foreign_cwd_prepares_selected_profile_and_resume() {
    let f = Fixture::new(Tool::Claude, "CLAUDE_CONFIG_DIR");
    // This is the previously broken --repo boundary: setup and exec must both
    // run in the selected repo rather than the caller's unrelated directory.
    let launched = f.command().current_dir(f.env.home()).output().unwrap();
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    assert!(!f.env.home().join(".claude.json").exists());
    let path = f.root.join(".claude.json");
    let before = std::fs::read(&path).unwrap();
    clank::agent_store::bind_session_to_agent(
        f.env.repo(),
        &f.label,
        Tool::Claude,
        &clank_core::ids::SessionId::parse("00000000-0000-0000-0000-000000000001").unwrap(),
    )
    .unwrap();
    let resumed = f.command().current_dir(f.env.home()).output().unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn claude_trust_failure_preserves_fork_intent_and_configuration() {
    let f = Fixture::new(Tool::Claude, "CLAUDE_CONFIG_DIR");
    std::fs::create_dir_all(&f.root).unwrap();
    let config = f.root.join(".claude.json");
    std::fs::write(&config, "{broken}").unwrap();
    let path = clank::cli::fork::fork_spec_path(f.env.repo(), &f.label);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let spec = serde_json::to_vec(&clank::cli::fork::ForkSpec {
        tool: Tool::Claude,
        from_session: Some("ancestor".into()),
        prompt: "orient".into(),
        grok: None,
    })
    .unwrap();
    std::fs::write(&path, &spec).unwrap();
    let preview = f.command().arg("--print").output().unwrap();
    assert!(preview.status.success());
    assert!(!f.root.join("settings.json").exists());
    let launched = f.command().output().unwrap();
    assert!(!launched.status.success());
    assert!(
        String::from_utf8_lossy(&launched.stderr).contains("cannot prepare Claude workspace trust")
    );
    assert_eq!(std::fs::read(&path).unwrap(), spec);
    assert_eq!(std::fs::read_to_string(config).unwrap(), "{broken}");
    assert!(!f.marker.exists());
    // Repairing the configuration makes that same intent usable.
    std::fs::write(f.root.join(".claude.json"), "{}").unwrap();
    let retried = f.command().output().unwrap();
    assert!(
        retried.status.success(),
        "{}",
        String::from_utf8_lossy(&retried.stderr)
    );
    assert!(!path.exists());
}

#[test]
fn claude_relative_and_default_profiles_match_setup_trust_and_execution() {
    for profile in [None, Some(""), Some("relative-profile")] {
        let f = Fixture::new(Tool::Claude, "CLAUDE_CONFIG_DIR");
        let mut config = clank::agent_store::load_repo_config_required(f.env.repo()).unwrap();
        let launch = config
            .agents
            .get_mut(&f.label)
            .unwrap()
            .launch
            .as_mut()
            .unwrap();
        let expected = if let Some(profile) = profile.filter(|value| !value.is_empty()) {
            f.env.repo().join(profile)
        } else {
            f.env.home().join(".claude")
        };
        if let Some(profile) = profile {
            launch
                .env
                .insert("CLAUDE_CONFIG_DIR".into(), profile.into());
        } else {
            launch.env.remove("CLAUDE_CONFIG_DIR");
        }
        launch
            .env
            .insert("EXPECTED_ROOT".into(), expected.to_string_lossy().into());
        std::fs::write(
            f.env.repo().join(".clank/config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
        let preview = f
            .command()
            .current_dir(f.env.home())
            .arg("--print")
            .output()
            .unwrap();
        assert!(preview.status.success());
        assert!(!expected.exists());
        let result = f.command().current_dir(f.env.home()).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if profile == Some("relative-profile") {
            assert!(expected.join(".claude.json").exists());
            assert!(!f.env.home().join("relative-profile").exists());
        } else {
            assert!(f.env.home().join(".claude.json").exists());
        }
    }
}
