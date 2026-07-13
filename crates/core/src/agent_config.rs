//! Typed config schema for per-agent Clank state.
//!
//! Pure data + serde round-trip. No clock, no env, no filesystem
//! — the CLI is the only place that touches the world. This type
//! describes what's written to
//! `.clank/agents/<label>/config.json` (per-agent, per-machine;
//! gitignored).
//!
//! There is intentionally no repo-shared config type. Master is
//! a per-user preference: whoever wrote the plan wants their
//! `wait` to default to master's view; everyone else wants
//! reviewers. There's no useful repo-wide assertion of "alice is
//! THE master" — gate state is computed from the cumulative
//! participant set, not from role claims.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::vocab::{AutoMode, Tool};

/// Per-agent, per-machine state for one (label, repo). Lives at
/// `.clank/agents/<label>/config.json`. Gitignored.
///
/// State only: which session this label is bound to, whether
/// auto-mode is on, and the wait-for-work timeout. Identity
/// (role / tool / launch / initial_prompt) lives in the
/// team-based registration model — user-scope `agents` +
/// `teams` and repo-scope `team` — not here. Unknown keys in
/// existing on-disk skeletons (e.g. a leftover `role`/`tool`
/// from before the cutover) are ignored by serde.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// `Some` only when this session EXPLICITLY chose via `clank auto
    /// on|off`; `None` means "unset, inherit the user-global default"
    /// (`auto-mode-default-on`). The distinction matters: a fresh
    /// skeleton must be `None` so it can inherit, while an explicit
    /// `clank auto off` must STICK even under a global default — so
    /// the field is omitted from a fresh skeleton, never written as a
    /// silent `off`. Resolve via [`effective_auto_mode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<AutoMode>,
    /// Wait-for-work timeout as a duration string (`"30m"`,
    /// `"5m"`, `"45s"`). `None` means indefinite. Stringly typed
    /// so users editing JSON see the same form
    /// `clank wait --timeout` accepts; validated by the CLI's
    /// existing `parse_timeout` at use site, not load site (one
    /// source of truth for the format).
    ///
    /// Consumed only by the CODEX stop hook's in-hook wait. The claude
    /// hook never waits (claude-stop-hook-minimal-hint): it nudges the
    /// agent to arm its own backgrounded `clank wait`, which runs
    /// indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<String>,
    /// Session this agent label is currently bound to. Written by
    /// `clank as <label>` (or `clank init` phase 2) using the
    /// `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    /// Extra wake sources for this agent's `clank wait`
    /// (extra-wait-events): things beyond this repo's state — GitHub
    /// activity on OTHER repos, arbitrary command completions. Fixed
    /// at arm time; a config edit takes effect on the next re-arm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_events: Vec<WaitEventSource>,
}

/// One extra wake source (extra-wait-events). Tagged sum type so the
/// set is additive: new kinds parse-fail LOUD on old binaries (an
/// unknown `kind` is an error, not a silent drop) and new filters can
/// join a kind without breaking existing configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitEventSource {
    Github(GithubSource),
    Command(CommandSource),
}

/// Watch one GitHub repo's event feed. An agent lists as many of
/// these as it likes — nothing is fixed to a single repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSource {
    /// `owner/name`.
    pub repo: String,
    /// Which event sub-kinds wake the agent.
    pub events: Vec<GithubEventKind>,
    /// Poll cadence as a duration string (`"60s"`, `"5m"`), same form
    /// as `wait_timeout`; validated at use site. The EFFECTIVE
    /// interval is max(this, the server's `X-Poll-Interval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<String>,
    /// Whether events actored by the authenticated `gh` login wake
    /// this agent. Default FALSE: a controller agent's own PR comment
    /// must not wake itself in a loop (and its own pushes don't
    /// either — load-bearing for `branch_push`).
    #[serde(default)]
    pub include_own_actions: bool,
    /// `branch_push` scope: only pushes to these branches wake the
    /// agent (short names, e.g. `"main"`). Empty = every branch.
    /// Applies to `branch_push` only — the PR kinds are
    /// number-scoped already.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// How events arrive (github-http-client-and-realtime M2):
    /// `poll` (default) or `realtime` — the webhook-forward relay
    /// front-running the poll for latency, with the poll remaining
    /// the completeness backstop. Unknown values fail loud.
    #[serde(default)]
    pub delivery: Delivery,
}

/// Delivery mode for a github source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Poll,
    Realtime,
}

/// The GitHub event sub-kinds `clank wait` can wake on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubEventKind {
    PrOpened,
    /// New commits landed on a PR (the events feed's `synchronize`
    /// action). Fires on the BASE repo, so fork PRs are covered.
    PrUpdated,
    PrMerged,
    /// All three comment classes: an issue comment on a PR, a
    /// submitted review (including empty-body approvals), and an
    /// inline diff comment.
    PrComment,
    IssueOpened,
    IssueClosed,
    /// An issue comment on a NON-PR issue.
    IssueComment,
    /// A push to a BRANCH (`refs/heads/*` only — tag pushes are not
    /// branch pushes and never match). Scope with
    /// [`GithubSource::branches`].
    BranchPush,
}

/// Spawn a command when the wait arms; its COMPLETION is the wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSource {
    /// Labels the wake item.
    pub name: String,
    /// Argv — program + args, NO implicit shell (write
    /// `["sh", "-c", "…"]` to get one). Runs in its own process
    /// group; the group is killed when the wait exits without it.
    pub command: Vec<String>,
}

/// Resolve a session's effective auto-mode from the per-agent
/// setting and the user-global default (`auto-mode-default-on`).
/// An explicit per-agent choice wins (so `clank auto off` sticks);
/// otherwise the user-global default applies; otherwise `Off`. This
/// is THE single resolver — every consumer (stop hook, agent start,
/// `clank auto status`) routes through it so the surfaces can't
/// disagree.
pub fn effective_auto_mode(
    per_agent: Option<AutoMode>,
    user_default: Option<AutoMode>,
) -> AutoMode {
    effective_auto_mode_with_source(per_agent, user_default).0
}

/// Where an effective auto-mode came from — so surfaces that show the
/// value (doctor) can say WHY without recreating the precedence
/// (grok-first-turn-orchestration: doctor printed the raw per-agent
/// field, an agent read `unset` as off, and misdiagnosed a wake gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoModeSource {
    PerAgent,
    UserDefault,
    /// Neither set: the built-in `Off`.
    Builtin,
}

/// The single mode+source resolver every consumer projects from.
pub fn effective_auto_mode_with_source(
    per_agent: Option<AutoMode>,
    user_default: Option<AutoMode>,
) -> (AutoMode, AutoModeSource) {
    match (per_agent, user_default) {
        (Some(m), _) => (m, AutoModeSource::PerAgent),
        (None, Some(m)) => (m, AutoModeSource::UserDefault),
        (None, None) => (AutoMode::Off, AutoModeSource::Builtin),
    }
}

/// General-purpose launch profile: executable + args + env.
///
/// Two consumers in the codebase today (per OQ2 of
/// `clank-diff-editor`: reuse this struct rather than fork):
///
/// 1. **`clank agent start <name>`** (per-agent launch). Args are
///    spliced BEFORE the session-restore suffix (claude's
///    `--resume <id>` or codex's `resume <id> --cd <repo>`) so
///    they attach to the top-level tool. For codex specifically:
///    flags AFTER the `resume` subcommand attach to `resume`, not
///    to the `codex` binary; putting `launch.args` first preserves
///    the typical use case (`codex --profile deep resume <id>`).
///    For claude (flat flags) position is cosmetic, but the same
///    rule keeps the mental model consistent.
///
/// 2. **`clank diff` editor launch** (`Config.diff.editor`).
///    `args` may include template variables like `{range}`,
///    `{commits}`, `{patch_file}` that are substituted at compose
///    time. Args do not have a "session-restore suffix" concept;
///    args are passed verbatim (post-substitution) to the editor.
///
/// If future consumers need fields beyond `command/args/env`
/// (e.g. multi-pane layout descriptors), split into a sibling
/// struct rather than overloading this one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchConfig {
    /// Override the executable. `None` falls back to the
    /// session-tool's bare name (`claude` / `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Args passed to the executable BEFORE the session-restore
    /// suffix. Example: `["--profile", "deep"]` for a codex agent
    /// produces `codex --profile deep resume <id> --cd <repo>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Env vars merged onto the calling process env. On key
    /// collision, this map wins ("config wins over inherited
    /// environment").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// One session's binding to an agent label. Stored under
/// [`AgentConfig::session`]; the resolver matches by `(id, tool)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub tool: Tool,
    /// RFC3339 / ISO-8601 timestamp of the last bind, in UTC
    /// (e.g. `"2026-05-23T06:24:47Z"`). Stringly typed so core
    /// stays free of `chrono`/`time` deps; the CLI formats with
    /// whatever clock source it has. UTC over local-offset
    /// because the `time` crate's `local-offset` codepath has
    /// known soundness issues in multi-threaded programs.
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    #[test]
    fn agent_config_round_trips_defaults() {
        let cfg = AgentConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.auto_mode, None, "fresh skeleton is unset, not Off");
        assert!(back.wait_timeout.is_none());
        assert!(back.session.is_none());
    }

    #[test]
    fn agent_config_round_trips_populated() {
        let cfg = AgentConfig {
            auto_mode: Some(AutoMode::On),
            wait_timeout: Some("30m".into()),
            session: Some(Session {
                id: session_id("742f6a04-f174-409a-ab01-419a16c5f372"),
                tool: Tool::Claude,
                updated_at: "2026-05-23T16:24:47+10:00".into(),
            }),
            wait_events: Vec::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn agent_config_ignores_unknown_legacy_fields() {
        // On-disk skeletons from before the team-based cutover
        // still carry `role`/`tool`/`launch`/`initial_prompt`.
        // serde must ignore them (no `deny_unknown_fields`).
        let json = r#"{
            "auto_mode": "off",
            "role": "master",
            "tool": "claude",
            "launch": { "command": "claude", "args": ["--skill", "ruthless"] },
            "initial_prompt": "hi"
        }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.auto_mode, Some(AutoMode::Off), "explicit off survives");
        assert!(cfg.session.is_none());
        // Reserialization drops the unknown keys entirely.
        let back = serde_json::to_string(&cfg).unwrap();
        assert!(!back.contains("role"));
        assert!(!back.contains("tool"));
        assert!(!back.contains("launch"));
    }

    #[test]
    fn launch_config_default_is_empty() {
        let lc = LaunchConfig::default();
        assert!(lc.command.is_none());
        assert!(lc.args.is_empty());
        assert!(lc.env.is_empty());
        // Round-trips as `{}`.
        let json = serde_json::to_string(&lc).unwrap();
        assert_eq!(json, "{}", "default LaunchConfig should emit `{{}}`");
    }

    #[test]
    fn agent_config_accepts_missing_optional_fields() {
        let json = r#"{ "auto_mode": "wait" }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.auto_mode, Some(AutoMode::On));
        assert!(cfg.wait_timeout.is_none());
        assert!(cfg.session.is_none());
    }

    #[test]
    fn agent_config_omits_none_session() {
        let cfg = AgentConfig {
            auto_mode: Some(AutoMode::On),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("session"), "expected no session key: {json}");
        assert!(
            !json.contains("wait_timeout"),
            "expected no wait_timeout key: {json}"
        );
    }

    #[test]
    fn github_source_round_trips_new_kinds_and_branches() {
        // github-wake-pr-updates-and-pushes: the additive sub-kinds +
        // the branches filter round-trip…
        let src = WaitEventSource::Github(GithubSource {
            repo: "o/r".into(),
            events: vec![GithubEventKind::PrUpdated, GithubEventKind::BranchPush],
            poll_interval: None,
            include_own_actions: false,
            branches: vec!["main".into()],
            delivery: Delivery::Realtime,
        });
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("pr_updated") && json.contains("branch_push"));
        let back: WaitEventSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
        // …and an OLD config (absent branches, original kinds) parses
        // unchanged with the empty-filter default.
        // delivery absent = poll; unknown value fails loud.
        assert!(
            serde_json::from_str::<WaitEventSource>(
                r#"{"kind":"github","repo":"o/r","events":[],"delivery":"carrier-pigeon"}"#
            )
            .is_err()
        );
        let old = r#"{"kind":"github","repo":"o/r","events":["pr_opened","issue_comment"]}"#;
        let parsed: WaitEventSource = serde_json::from_str(old).unwrap();
        let WaitEventSource::Github(g) = parsed else {
            panic!("github kind");
        };
        assert!(g.branches.is_empty(), "absent branches = every branch");
        assert_eq!(g.delivery, Delivery::Poll, "absent delivery = poll");
        assert_eq!(
            g.events,
            vec![GithubEventKind::PrOpened, GithubEventKind::IssueComment]
        );
    }

    #[test]
    fn effective_auto_mode_layers_explicit_over_default() {
        use AutoMode::{Off, On};
        // Explicit per-agent wins (so `clank auto off` sticks even
        // under a global default-on).
        assert_eq!(effective_auto_mode(Some(Off), Some(On)), Off);
        assert_eq!(effective_auto_mode(Some(On), Some(Off)), On);
        // Unset per-agent inherits the user-global default.
        assert_eq!(effective_auto_mode(None, Some(On)), On);
        assert_eq!(effective_auto_mode(None, Some(Off)), Off);
        // Nothing set anywhere → Off (today's behavior).
        assert_eq!(effective_auto_mode(None, None), Off);
    }
}
