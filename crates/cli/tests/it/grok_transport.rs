//! Process-only contracts: state decisions live in direct unit tests.
use clank_core::agent_config::LaunchConfig;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

async fn exchange(mode: &str, followup: bool) -> (anyhow::Result<Value>, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut env = BTreeMap::from([
        ("GROK_HOME".into(), root.to_string_lossy().into_owned()),
        ("FIXTURE_RPC_MODE".into(), mode.into()),
    ]);
    for key in clank::agent_env::SESSION_IDENTITY_VARS {
        env.insert((*key).into(), "foreign".into());
    }
    let launch = LaunchConfig {
        command: Some(env!("CARGO_BIN_EXE_clank-test-fixture").into()),
        args: vec!["grok".into(), "--fixture-profile".into()],
        env,
    };
    let result = clank::cli::test_support::grok_exchange(
        &launch,
        &root,
        followup.then(|| ("fixture/echo", json!({"answer":42}))),
        Duration::from_secs(3),
    )
    .await;
    let audit: Vec<Value> = std::fs::read_to_string(root.join("audit.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let pid = audit[0]["pid"].as_i64().unwrap() as i32;
    assert_eq!(
        clank::signal::probe_process(pid),
        clank::signal::Probe::Gone,
        "helper survived {mode}"
    );
    (result, audit)
}

#[tokio::test]
async fn configured_transport_scrubs_identity_drains_stderr_and_reaps() {
    let (result, audit) = exchange("stderr", true).await;
    assert_eq!(result.unwrap(), json!({"answer":42}));
    assert_eq!(
        audit[0]["argv"],
        json!(["--fixture-profile", "agent", "--no-leader", "stdio"])
    );
    assert_eq!(audit[0]["updater"], "1");
    assert!(
        audit[0]["identity"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[1]["request"]["method"], "initialize");
    assert_eq!(audit[2]["request"]["method"], "fixture/echo");
    let (result, audit) = exchange("good", false).await;
    assert_eq!(result.unwrap()["protocolVersion"], 1);
    assert_eq!(audit.len(), 2, "initialize must not issue a second request");
}

#[tokio::test]
async fn invalid_protocol_and_output_limit_fail_and_reap() {
    for (mode, diagnostic) in [
        ("version", "unsupported Grok ACP protocol version"),
        ("unsupported", "method not found"),
        ("malformed", "malformed JSON"),
        ("wrong_id", "wrong request"),
        ("flood", "output exceeds"),
    ] {
        let (result, _) = exchange(mode, true).await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(diagnostic), "{mode}: {error}");
    }
}

#[tokio::test]
async fn silent_helper_is_ended_by_the_transport_deadline() {
    let (result, _) = exchange("hang", true).await;
    assert!(format!("{:#}", result.unwrap_err()).contains("timed out"));
}
