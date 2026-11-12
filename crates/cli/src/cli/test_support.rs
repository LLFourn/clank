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
