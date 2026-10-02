use super::*;
use serde_json::json;
use std::collections::BTreeMap;

fn advertised(version: &str, events: &[&str]) -> Value {
    json!({"protocolVersion":1,"_meta":{"agentVersion":version,"accountSecret":"must-not-appear"},
        "agentCapabilities":{"_meta":{"x.ai/hooks":{"blockingEvents":events}}}})
}

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    provider: Provider,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let root = cwd.join("profile");
        std::fs::create_dir(&root).unwrap();
        let provider = Provider {
            program: "grok".into(),
            args: vec![],
            env: BTreeMap::new(),
            root,
        };
        Self {
            _temp: temp,
            cwd,
            provider,
        }
    }
    fn configure_fake(&mut self) {
        let script = self.cwd.join("fake.py");
        std::fs::write(&script,r#"
import os,sys,json,pathlib
root=pathlib.Path(os.environ['GROK_HOME'])
req=json.loads(sys.stdin.readline())
(root/'audit.json').write_text(json.dumps({'request':req,'argv':sys.argv[1:],'updater':os.environ.get('GROK_DISABLE_AUTOUPDATER'),'identity':{k:os.environ.get(k) for k in ['CLANK_AGENT','GROK_SESSION_ID','CLAUDE_CODE_SESSION_ID','CODEX_THREAD_ID']}}))
print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':{'protocolVersion':1,'_meta':{'agentVersion':'1.0.46','accountSecret':'must-not-appear'},'agentCapabilities':{'_meta':{'x.ai/hooks':{'blockingEvents':['pre_tool_use','stop']}}}}}),flush=True)
for line in sys.stdin: raise Exception('unexpected second request')
"#).unwrap();
        self.provider.program = "/usr/bin/python3".into();
        self.provider.args = vec![
            script.to_string_lossy().into_owned(),
            "profile-argument".into(),
        ];
        self.provider.env.insert(
            "GROK_HOME".into(),
            self.provider.root.to_string_lossy().into_owned(),
        );
        self.provider
            .env
            .insert("HOME".into(), self.cwd.to_string_lossy().into_owned());
    }
    fn install(&self, dry: bool) -> Result<()> {
        super::super::setup::install_grok_hooks(&hook_path(&self.provider), dry, &mut vec![])
    }
}

#[test]
fn capability_advertisement_is_neither_a_version_guess_nor_a_wake_proof() {
    let cap = Capabilities::decode(&advertised("1.0.46", &["pre_tool_use", "stop"]));
    assert!(cap.candidate);
    assert!(cap.description().contains("not verified by initialize"));
    assert!(!cap.description().contains("must-not-appear"));
    for version in ["1.0.45", "2.0.0", "garbage", "1.0.46-secret"] {
        assert!(!Capabilities::decode(&advertised(version, &["pre_tool_use", "stop"])).candidate);
    }
    for events in [
        &["stop"][..],
        &["pre_tool_use"][..],
        &["PreToolUse", "Stop"][..],
    ] {
        assert!(!Capabilities::decode(&advertised("1.0.46", events)).candidate);
    }
    assert!(!Capabilities::decode(&json!({"_meta":{"agentVersion":"1.0.46"}})).candidate);
}

#[test]
fn complete_install_is_idempotent_and_keeps_unrelated_hooks() {
    let f = Fixture::new();
    let path = hook_path(&f.provider);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let user = json!({"type":"command","command":"my-notifier","timeout":17});
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"other":{"keep":true},"hooks":{
            "PreToolUse":[{"matcher":"anything", "custom":7, "hooks":[
                {"type":"command","command":"clank stop-hook --tool claude"},user]}],
            "UserPromptSubmit":[{"hooks":[{"type":"command","command":"another"}]}]
        }}))
        .unwrap(),
    )
    .unwrap();
    f.install(false).unwrap();
    validate_hooks(&f.provider).unwrap();
    let installed = std::fs::read(&path).unwrap();
    let value = read_hooks(&path).unwrap();
    assert_eq!(value["other"]["keep"], true);
    assert_eq!(value["hooks"]["PreToolUse"][0]["hooks"], json!([user]));
    assert_eq!(value["hooks"]["PreToolUse"][0]["custom"], 7);
    assert_eq!(
        value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "another"
    );
    f.install(false).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), installed);
}

#[test]
fn dry_run_creates_nothing_and_malformed_event_cannot_publish_partial_adapter() {
    let f = Fixture::new();
    let path = hook_path(&f.provider);
    f.install(true).unwrap();
    assert!(!path.parent().unwrap().exists());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = br#"{"hooks":{"PermissionDenied":"malformed"},"keep":true}"#;
    std::fs::write(&path, original).unwrap();
    assert!(f.install(false).is_err());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn validator_catches_missing_post_guard_and_incorrect_terminal_matcher() {
    let f = Fixture::new();
    f.install(false).unwrap();
    let path = hook_path(&f.provider);
    let original = read_hooks(&path).unwrap();
    for event in ["PostToolUse", "PreToolUse", "SessionStart", "StopCancelled"] {
        let mut broken = original.clone();
        broken["hooks"].as_object_mut().unwrap().remove(event);
        std::fs::write(&path, serde_json::to_vec(&broken).unwrap()).unwrap();
        assert!(
            validate_hooks(&f.provider)
                .unwrap_err()
                .to_string()
                .contains(event)
        );
    }
    let mut broken = original;
    broken["hooks"]["PostToolUse"][0]["matcher"] = Value::String(".*".into());
    std::fs::write(path, serde_json::to_vec(&broken).unwrap()).unwrap();
    assert!(
        validate_hooks(&f.provider)
            .unwrap_err()
            .to_string()
            .contains("matcher")
    );
}

#[test]
fn custom_home_projects_every_grok_asset_and_keeps_default_manifest_provenance() {
    let f = Fixture::new();
    let home = f.cwd.join("user");
    let mut grok_assets = 0;
    for asset in super::super::setup::user_asset_inventory(false) {
        let original = asset.rel.clone();
        let (root, projected) = asset_location(&home, &f.provider.root, asset);
        if original.starts_with(".grok/") {
            grok_assets += 1;
            assert_eq!(root, f.provider.root);
            assert_eq!(projected.rel, original.trim_start_matches(".grok/"));
        } else {
            assert_eq!(root, home);
            assert_eq!(projected.rel, original);
        }
    }
    assert_eq!(grok_assets, 4);
    let asset = super::super::setup::user_asset_inventory(false)
        .into_iter()
        .find(|a| a.rel.starts_with(".grok/"))
        .unwrap();
    let original = asset.rel.clone();
    let (root, asset) = asset_location(&home, &home.join(".grok"), asset);
    assert_eq!(root, home);
    assert_eq!(asset.rel, original);
}

#[test]
fn selected_profile_uses_actual_tool_and_custom_home_without_binding_mutation() {
    use crate::cli::teams_config::{RepoConfigFile, RosterAgent, RosterRole};
    use clank_core::agent_config::LaunchConfig;
    use clank_core::ids::SessionId;
    let f = Fixture::new();
    let label = AgentLabel::parse("grok").unwrap();
    std::fs::create_dir(f.cwd.join(".clank")).unwrap();
    let mut roster = RepoConfigFile::default();
    roster.agents.insert(
        label.clone(),
        RosterAgent {
            tool: Tool::Claude,
            role: RosterRole::Master,
            initial_prompt: None,
            launch: Some(LaunchConfig {
                command: Some("configured-grok".into()),
                args: vec!["--configured".into()],
                env: BTreeMap::from([(
                    "GROK_HOME".into(),
                    f.provider.root.to_string_lossy().into_owned(),
                )]),
            }),
        },
    );
    std::fs::write(
        f.cwd.join(".clank/config.json"),
        serde_json::to_vec(&roster).unwrap(),
    )
    .unwrap();
    crate::agent_store::bind_session_to_agent(
        &f.cwd,
        &label,
        Tool::Grok,
        &SessionId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
    )
    .unwrap();
    let config = crate::agent_store::agent_config_path(&f.cwd, &label);
    let before = std::fs::read(&config).unwrap();
    let selected = profile(&f.cwd, &label).unwrap();
    assert_eq!(selected.program, "configured-grok");
    assert_eq!(selected.args, ["--configured"]);
    assert_eq!(selected.root, f.provider.root);
    assert_eq!(std::fs::read(config).unwrap(), before);
}

#[tokio::test]
async fn initialize_probe_uses_configured_executable_and_only_one_scrubbed_request() {
    let mut f = Fixture::new();
    f.configure_fake();
    for key in crate::agent_env::SESSION_IDENTITY_VARS {
        f.provider.env.insert((*key).into(), "foreign".into());
    }
    let cap = probe(&f.provider, &f.cwd).await;
    assert!(cap.candidate, "{}", cap.description());
    let audit: Value =
        serde_json::from_slice(&std::fs::read(f.provider.root.join("audit.json")).unwrap())
            .unwrap();
    assert_eq!(audit["request"]["method"], "initialize");
    assert_eq!(
        audit["argv"],
        json!(["profile-argument", "agent", "--no-leader", "stdio"])
    );
    assert_eq!(audit["updater"], "1");
    assert!(
        audit["identity"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    assert!(!cap.description().contains("must-not-appear"));
}

#[tokio::test]
async fn auto_off_launch_does_not_probe_or_require_hooks() {
    let f = Fixture::new();
    let label = AgentLabel::parse("grok").unwrap();
    crate::agent_store::set_auto_mode(&f.cwd, &label, AutoMode::Off).unwrap();
    launch_ready(&f.cwd, &label, &f.provider).await.unwrap();
    assert!(!hook_path(&f.provider).exists());
}

#[test]
fn diagnostic_keeps_config_advertisement_and_reaction_distinct() {
    use super::super::doctor::{CheckStatus, grok_report};
    let f = Fixture::new();
    let cap = Capabilities::decode(&advertised("1.0.46", &["pre_tool_use", "stop"]));
    assert_eq!(
        grok_report("grok", &f.provider, true, &cap)[0].status,
        CheckStatus::Fail
    );
    assert_eq!(
        grok_report("default", &f.provider, false, &cap)[0].status,
        CheckStatus::Warn
    );
    f.install(false).unwrap();
    let report = grok_report("grok", &f.provider, true, &cap);
    assert_eq!(report[0].status, CheckStatus::Ok);
    assert_eq!(report[1].status, CheckStatus::Ok);
    assert_eq!(report[2].status, CheckStatus::Warn);
    assert!(report[2].message.contains("model reaction"));
}

#[tokio::test]
async fn real_auto_launch_gate_requires_complete_adapter_then_advertisement_without_binding() {
    let mut f = Fixture::new();
    f.configure_fake();
    let label = AgentLabel::parse("grok").unwrap();
    crate::agent_store::set_auto_mode(&f.cwd, &label, AutoMode::On).unwrap();
    let config = crate::agent_store::agent_config_path(&f.cwd, &label);
    let before = std::fs::read(&config).unwrap();
    assert!(launch_ready(&f.cwd, &label, &f.provider).await.is_err());
    assert!(
        !f.provider.root.join("audit.json").exists(),
        "missing configuration must not start a helper"
    );
    f.install(false).unwrap();
    launch_ready(&f.cwd, &label, &f.provider).await.unwrap();
    assert_eq!(std::fs::read(config).unwrap(), before);
    assert!(f.provider.root.join("audit.json").exists());
    f.provider.program = "/does-not-exist/secret-profile-option".into();
    let error = launch_ready(&f.cwd, &label, &f.provider)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("clank setup --grok-agent grok"));
    assert!(!error.contains("secret-profile-option"));
}

#[tokio::test]
#[ignore = "requires CLANK_GROK_TEST_BIN and a credential-free env -i test process"]
async fn installed_initialize_advertises_the_native_baseline_without_starting_a_session() {
    let unexpected: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key != "PATH" && key != "CLANK_GROK_TEST_BIN" && key != "__CF_USER_TEXT_ENCODING"
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "run in env -i; unexpected keys: {unexpected:?}"
    );
    let mut f = Fixture::new();
    f.provider.program = std::env::var("CLANK_GROK_TEST_BIN").unwrap();
    f.provider.env = BTreeMap::from([
        ("HOME".into(), f.cwd.to_string_lossy().into_owned()),
        (
            "GROK_HOME".into(),
            f.provider.root.to_string_lossy().into_owned(),
        ),
        ("TERM".into(), "dumb".into()),
    ]);
    let cap = probe(&f.provider, &f.cwd).await;
    assert!(cap.candidate, "{}", cap.description());
    assert!(cap.description().contains("unverified") || cap.description().contains("not verified"));
    assert!(!f.cwd.join(".clank").exists());
    // Grok initializes session-cache directories while collecting initialize
    // metadata. Their existence is not evidence of a created provider session.
    fn has_summary(path: &Path) -> bool {
        std::fs::read_dir(path)
            .ok()
            .into_iter()
            .flatten()
            .any(|entry| {
                let path = entry.unwrap().path();
                path.file_name().is_some_and(|name| name == "summary.json")
                    || (path.is_dir() && has_summary(&path))
            })
    }
    assert!(!has_summary(&f.provider.root.join("sessions")));
    eprintln!("isolated initialize: {}", cap.description());
}
