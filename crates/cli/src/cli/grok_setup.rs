//! Explicit Grok installation/probing. No helper is called from an idle hook.
//! ACP initialize advertises decision gates, not loaded hooks or model wake
//! behavior; report those facts separately instead of treating a version as
//! proof of a functioning background completion channel.

use anyhow::{Context, Result, ensure};
use clank_core::ids::AgentLabel;
use clank_core::{AutoMode, Tool};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::grok_launch::Provider;

pub(super) const COMMAND: &str = "clank stop-hook --tool grok";
pub(super) const EVENTS: [&str; 3] = ["SessionStart", "Stop", "SessionEnd"];
// Installation removes only Clank-owned entries for the retired adapter.
pub(super) const RETIRED_EVENTS: [&str; 6] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "StopCancelled",
    "StopFailure",
];

/// The HOME a Grok agent runs under. A custom launch HOME can have a
/// different global auto default from the monitor's own, so it is derived
/// from the roster, not from a protocol field.
pub(crate) fn launch_home(repo: &Path, label: &AgentLabel) -> Option<std::path::PathBuf> {
    profile(repo, label)
        .ok()
        .and_then(|provider| provider.env.get("HOME").map(std::path::PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
}

pub(super) fn profile(repo: &Path, label: &AgentLabel) -> Result<Provider> {
    let set = crate::agent_store::try_resolve_via_team(repo)?
        .context("this repo has no operating roster")?;
    let desc = super::agent::find_in_set(&set, label)
        .context("the selected Grok agent is not on this repo's roster")?;
    let cfg = crate::agent_store::load_agent_config(repo, label)?;
    let actual = match cfg.as_ref().and_then(|c| c.session.as_ref()) {
        Some(session) => session.tool,
        None => super::fork::load_fork_spec(repo, label)?.map_or(desc.tool, |spec| spec.tool),
    };
    ensure!(
        actual == Tool::Grok,
        "`{label}` is not configured to launch Grok"
    );
    Provider::resolve(desc.launch.as_ref(), repo)
}

pub(super) fn installation(args: &super::SetupArgs, home: &Path) -> Result<(Provider, PathBuf)> {
    if let Some(raw) = &args.grok_agent {
        let label = AgentLabel::parse(raw)?;
        let repo = super::resolve_repo(args.repo.as_deref())?;
        Ok((profile(&repo, &label)?, repo))
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_owned());
        Ok((Provider::resolve(None, &cwd)?, cwd))
    }
}

/// Preserve the default-home setup manifest and its upgrade provenance. Only
/// custom Grok homes need a projected inventory/manifest rooted there.
pub(super) fn asset_location(
    home: &Path,
    grok_home: &Path,
    asset: super::setup::OwnedAsset,
) -> (PathBuf, super::setup::OwnedAsset) {
    if !asset.rel.starts_with(".grok/") {
        return (home.to_owned(), asset);
    }
    super::setup::provider_asset_location(home, grok_home, Tool::Grok, asset)
}

#[derive(Debug)]
pub(super) struct Capabilities {
    pub version: Option<String>,
    pub stop: bool,
    pub candidate: bool,
    pub failure: Option<&'static str>,
}

impl Capabilities {
    fn decode(init: &Value) -> Self {
        let version = init
            .pointer("/_meta/agentVersion")
            .and_then(Value::as_str)
            .and_then(|v| v.split_whitespace().next())
            .filter(|v| v.len() <= 64)
            .map(str::to_owned);
        let events = init
            .pointer("/agentCapabilities/_meta/x.ai~1hooks/blockingEvents")
            .and_then(Value::as_array);
        let gate = |name: &str| {
            events.is_some_and(|events| events.iter().any(|v| v.as_str() == Some(name)))
        };
        let stop = gate("stop");
        let baseline = version
            .as_deref()
            .and_then(version_tuple)
            .is_some_and(|v| v.0 == 1 && v >= (1, 0, 46));
        let candidate = baseline && stop;
        Self {
            version,
            stop,
            candidate,
            failure: (!candidate).then_some(
                "requires Grok 1.x at least 1.0.46 and an advertised native Stop decision gate",
            ),
        }
    }
    pub(super) fn description(&self) -> String {
        // Whitelisted data only: initialize also contains account/model/config
        // metadata, and an error may contain secret profile arguments. Neither
        // the full response nor provider stderr belongs in setup/doctor output.
        format!("version={}, advertised Stop={}; {}",
            self.version.as_deref().filter(|v| version_tuple(v).is_some()).unwrap_or("unknown"),
            self.stop,
            self.failure.unwrap_or("candidate for the native adapter; loaded hook payloads and background-to-model continuation are not verified by initialize"))
    }
}

fn version_tuple(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split_whitespace().next()?.split('.');
    let tuple = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(tuple)
}

pub(super) async fn probe(provider: &Provider, repo: &Path) -> Capabilities {
    match super::grok_launch::initialize(provider, repo).await {
        Ok(init) => Capabilities::decode(&init),
        Err(_) => Capabilities {
            version: None,
            stop: false,
            candidate: false,
            failure: Some(
                "configured Grok ACP initialize failed or exceeded its bounded budget; upgrade/check that executable, or use auto off and an explicit manual wait",
            ),
        },
    }
}

pub(super) fn hook_path(provider: &Provider) -> PathBuf {
    provider.root.join("hooks/clank.json")
}

pub(super) fn read_hooks(path: &Path) -> Result<Value> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .with_context(|| format!("reading native Grok adapter {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "native Grok hook file exceeds 1 MiB"
    );
    serde_json::from_slice(&bytes).context("invalid native Grok hook JSON")
}

/// Check the complete expected file, not only one Stop entry. Presence means
/// configuration is installed; it cannot prove discovery/enablement at runtime.
pub(super) fn validate_hooks(provider: &Provider) -> Result<()> {
    let path = hook_path(provider);
    let value = read_hooks(&path)?;
    for event in EVENTS {
        let mut owned = Vec::new();
        for group in value
            .get("hooks")
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for entry in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if super::setup::hook_is_clank(entry) {
                    owned.push((group, entry));
                }
            }
        }
        ensure!(
            owned.len() == 1,
            "native Grok {event} needs exactly one Clank entry"
        );
        let (group, entry) = owned[0];
        ensure!(
            entry.get("type").and_then(Value::as_str) == Some("command")
                && entry.get("command").and_then(Value::as_str) == Some(COMMAND)
                && entry.get("timeout").and_then(Value::as_u64) == Some(5),
            "native Grok {event} command/timeout drifted"
        );
        let matcher = group.get("matcher").and_then(Value::as_str);
        ensure!(matcher.is_none(), "native Grok {event} matcher drifted");
    }
    for event in RETIRED_EVENTS {
        let owned = value
            .pointer(&format!("/hooks/{event}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .any(super::setup::hook_is_clank);
        ensure!(
            !owned,
            "retired native Grok {event} Clank hook remains installed"
        );
    }
    Ok(())
}

/// Real automatic launches must have the matching profile/home adapter and
/// advertised gates. Previews and explicit auto-off do not start helpers.
pub(super) async fn launch_ready(
    repo: &Path,
    label: &AgentLabel,
    provider: &Provider,
) -> Result<()> {
    let cfg = crate::agent_store::load_agent_config(repo, label)?;
    let home = provider
        .env
        .get("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if super::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref()) == AutoMode::Off {
        return Ok(());
    }
    let recovery = format!(
        "run `clank setup --grok-agent {label} --repo {}` for this profile, or use `clank auto off` for manual operation",
        shell_words::quote(&repo.to_string_lossy())
    );
    validate_hooks(provider).with_context(|| recovery.clone())?;
    let capabilities = probe(provider, repo).await;
    ensure!(
        capabilities.candidate,
        "Grok automatic handoff unavailable: {}; {recovery}",
        capabilities.description()
    );
    Ok(())
}

#[cfg(test)]
#[path = "grok_setup_tests.rs"]
mod tests;
