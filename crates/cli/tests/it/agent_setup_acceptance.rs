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
            r#"import os,pathlib,sys
if '--version' in sys.argv:
 print('2.1.0 (Claude Code)'); sys.exit(0)
r=pathlib.Path(os.environ['EXPECTED_ROOT'])
for name in ['clank-master','clank-reviewer','clank-pr-review','clank-github']:
 assert (r/'skills'/name/'SKILL.md').is_file(), name
tool=os.environ['EXPECTED_TOOL']
required={'claude':'settings.json','codex':'hooks.json','opencode':'plugin/clank.js'}[tool]
assert (r/required).is_file(), required
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
