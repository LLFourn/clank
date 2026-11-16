//! Narrow access to native transport boundaries for the integration harness.
//! Compiled only with the test-runtime feature; absent from installed builds.
use clank_core::agent_config::LaunchConfig;
use serde_json::Value;
use std::{path::Path, time::Duration};

pub async fn grok_exchange(
    launch: &LaunchConfig,
    repo: &Path,
    followup: Option<(&str, Value)>,
    budget: Duration,
) -> anyhow::Result<Value> {
    let provider = super::grok_launch::Provider::resolve(Some(launch), repo)?;
    super::grok_launch::acp_rpc(&provider, repo, followup, budget).await
}

/// The runtime root this process uses, to hand to the children it spawns
/// as `CLANK_TEST_RUNTIME` so they share its namespace.
pub fn runtime_root() -> std::path::PathBuf {
    super::monitor::test_runtime_root().expect("a test build always isolates its runtime")
}

/// This uid's runtime namespace under [`runtime_root`].
pub fn runtime_dir() -> std::path::PathBuf {
    super::monitor::runtime_dir(rustix::process::getuid().as_raw())
}

/// The launch gate a real automatic Grok launch passes through.
pub async fn grok_launch_ready(
    launch: &LaunchConfig,
    repo: &Path,
    label: &clank_core::ids::AgentLabel,
) -> anyhow::Result<()> {
    let provider = super::grok_launch::Provider::resolve(Some(launch), repo)?;
    super::grok_setup::launch_ready(repo, label, &provider).await
}

/// `clank setup --grok-agent`'s hook install for this launch's profile.
pub fn install_grok_hooks(launch: &LaunchConfig, repo: &Path) -> anyhow::Result<()> {
    let provider = super::grok_launch::Provider::resolve(Some(launch), repo)?;
    super::setup::install_grok_hooks(&super::grok_setup::hook_path(&provider), false, &mut vec![])
}
