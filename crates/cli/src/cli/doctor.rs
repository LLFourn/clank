//! `clank doctor` — diagnose whether the clank integration is
//! correctly set up across all three scopes (repo, user-wide,
//! current session). Reads the same loaders the rest of the CLI
//! uses so it catches drift between on-disk state and what the
//! resolver / stop-hook will see.
//!
//! Exit code: 0 if all checks are OK/Warn; 1 if any are Fail.

use std::path::{Path, PathBuf};

use super::DoctorArgs;
use crate::agent_env::{
    detect_session_from_env, explicit_label_from_env, resolve_identity_from_env,
};
use crate::agent_store::{
    agent_config_path, agents_root, load_agent_config, load_all_agent_configs,
};
use crate::cli::teams_config::AgentDescription;
use clank_core::ids::AgentLabel;

/// Sentinel returned by [`run`] when one or more checks failed.
/// Routed to exit code 1 by `main::exit_code_for`. Defining it as
/// a typed error (rather than calling `std::process::exit`
/// directly) keeps the only direct-exit caller in the binary the
/// stop-hook adapter, which has its own per-tool exit protocol.
#[derive(Debug)]
pub struct DoctorFailed;

impl std::fmt::Display for DoctorFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("clank doctor: one or more checks failed")
    }
}

impl std::error::Error for DoctorFailed {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub section: &'static str,
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

impl CheckResult {
    fn ok(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Ok,
            message: message.into(),
        }
    }
    fn warn(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
        }
    }
    fn fail(section: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
        }
    }
}

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let mut results: Vec<CheckResult> = Vec::new();

    // Repo scope — skip if we're not in a clank-initialized repo.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let repo = super::resolve_repo(args.repo.as_deref()).ok();
    if let Some(repo) = repo.as_deref() {
        // Probed ONCE at the command boundary, so `repo_checks` stays
        // deterministic and spawns nothing. Reported even outside a
        // zellij session: before opening one is exactly when learning
        // your client cannot place panes is useful.
        results.extend(repo_checks(
            repo,
            home.as_deref(),
            Some(crate::cli::open_zellij::placement_capability()),
        ));
    } else {
        results.push(CheckResult::warn(
            "repo",
            "repo",
            "not inside a git repo (skipping repo checks)",
        ));
    }

    // User scope — independent of repo.
    results.extend(user_checks());

    // Session scope — only meaningful inside an agent.
    results.extend(session_checks(repo.as_deref()));

    render(&results, args.json)?;

    if results
        .iter()
        .any(|r| matches!(r.status, CheckStatus::Fail))
    {
        return Err(DoctorFailed.into());
    }
    Ok(())
}

/// Repo-scope checks. `home` is explicit (not read from `$HOME`)
/// so in-process callers (tests) control team resolution. `pub`
/// for in-process assertion. Plan: dogfood-init-setup-in-tests
/// (Phase B).
pub fn repo_checks(
    repo: &Path,
    home: Option<&Path>,
    placement: Option<crate::cli::open_zellij::PlacementCapability>,
) -> Vec<CheckResult> {
    let mut out: Vec<CheckResult> = Vec::new();
    const SECTION: &str = "repo";

    // .clank/.gitignore presence.
    let gi = repo.join(".clank/.gitignore");
    match std::fs::read_to_string(&gi) {
        Ok(_) => out.push(CheckResult::ok(
            SECTION,
            ".clank/.gitignore",
            format!("present at {}", gi.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.push(CheckResult::warn(
            SECTION,
            ".clank/.gitignore",
            "missing — run `clank init`".to_string(),
        )),
        Err(e) => out.push(CheckResult::fail(
            SECTION,
            ".clank/.gitignore",
            format!("read failed: {e}"),
        )),
    }

    // The `.clank/.gitignore` allow-list keeps only plans/ + finished/
    // tracked; everything else under .clank/ is local.
    out.push(check_gitignore_probe(repo, ".clank/plans/x.md", true));
    out.push(check_gitignore_probe(repo, ".clank/finished/x", true));

    // The root `.gitignore` should carry NO `.clank/` rules — the
    // single-source `.clank/.gitignore` allow-list owns them. Leftovers are
    // redundant and a drift hazard.
    out.push(check_root_has_no_clank_rules(repo));

    // .claude/settings.local.json permissions.
    out.push(check_claude_perms(repo));

    // User zellij layout template, when configured: parse + marker
    // presence (`zellij-layout-config-around-agent-panes`). Checked
    // BEFORE the team-resolution early-returns so a broken template
    // surfaces at doctor time even on a teamless repo — never at
    // `open zellij` time.
    if let Some(template) = home
        .map(crate::cli::team::read_user_config)
        .transpose()
        .ok()
        .flatten()
        .and_then(|cfg| cfg.zellij.and_then(|z| z.layout))
    {
        match crate::cli::open_zellij::validate_template(&template) {
            Ok(()) => out.push(CheckResult::ok(
                SECTION,
                "zellij layout template",
                "parses and contains the `clank_agents` marker".to_string(),
            )),
            Err(e) => out.push(CheckResult::fail(
                SECTION,
                "zellij layout template",
                format!("{e:#}"),
            )),
        }
    }

    // Pane placement needs `zellij action stack-panes`; without it
    // reviewers are never stacked and the only symptom is a wrong
    // arrangement — which is how this reached us as "zellij is slow
    // and unreliable" (zellij-pane-placement-and-cost). The result is
    // INJECTED rather than probed here: probing inside `repo_checks`
    // made every doctor test spawn the real binary whenever tests ran
    // inside zellij, a hidden process dependency this repo's test
    // boundary forbids (codex on 0e1522b).
    if let Some(cap) = placement {
        use crate::cli::open_zellij::PlacementCapability as P;
        let name = "zellij pane placement";
        match cap {
            P::NoZellij => {}
            P::ClientSupports => out.push(CheckResult::ok(
                SECTION,
                name,
                "`stack-panes` available in the zellij client — reviewer panes \
                 can be placed (a session started by an OLDER binary may still \
                 reject it until restarted)"
                    .to_string(),
            )),
            P::ClientTooOld => out.push(CheckResult::warn(
                SECTION,
                name,
                "this zellij has no `stack-panes`: reviewer panes will not be \
                 stacked, and the symptom is panes beside the status pane \
                 instead of in the reviewer stack. Upgrade zellij."
                    .to_string(),
            )),
        }
    }

    // Per-agent checks: registration comes from the resolved team
    // set (`teams-based-agent-registration`). Join skeleton state,
    // and flag orphan skeletons (present on disk, not registered).
    let registered = match crate::agent_store::try_resolve_via_team_with(repo, home) {
        Ok(Some(set)) => set,
        Ok(None) => {
            out.push(CheckResult::warn(
                SECTION,
                "agents",
                "this repo has no agents configured; run `clank agent add <name>` + \
                 `clank agent promote <name>` (or `clank init --team <name>`) to register agents"
                    .to_string(),
            ));
            return out;
        }
        Err(e) => {
            out.push(CheckResult::fail(
                SECTION,
                "agents",
                format!("failed to resolve team registration: {e:#}"),
            ));
            return out;
        }
    };

    // (label, role-string, description) for master + reviewers.
    let mut members: Vec<(AgentLabel, &'static str, AgentDescription)> = Vec::new();
    members.push((
        registered.master.clone(),
        "master",
        registered.master_desc.clone(),
    ));
    for r in &registered.reviewers {
        use crate::cli::teams_config::RosterRole;
        let role_str = match r.role {
            RosterRole::Commit => "commit reviewer",
            RosterRole::Plan => "plan reviewer",
            RosterRole::Final => "final reviewer",
            RosterRole::Gate => "gate reviewer",
            RosterRole::Master => "master", // unreachable: master isn't a reviewer
        };
        members.push((r.label.clone(), role_str, r.desc.clone()));
    }

    for (label, role_str, desc) in &members {
        // The EFFECTIVE launch executable on $PATH? — explicit
        // `launch.command` or the tool's bare default. Launch
        // feasibility comes from the ROSTER description alone, so
        // this runs before any skeleton/session early-outs: a
        // freshly added agent with no skeleton yet is exactly the
        // state where a missing binary should be diagnosed (codex
        // 0c90514/00fc9b4).
        let program = effective_launch_program(desc);
        if which::which(&program).is_err() {
            let source = if desc.launch.as_ref().is_some_and(|l| l.command.is_some()) {
                "launch.command"
            } else {
                "tool default"
            };
            out.push(CheckResult::warn(
                SECTION,
                format!("agent: {} launch", label.as_str()),
                format!(
                    "`{program}` ({source}) not found on $PATH; \
                     `clank agent start {}` will fail at exec time",
                    label.as_str()
                ),
            ));
        }

        let skeleton = match load_agent_config(repo, label) {
            Ok(s) => s,
            Err(e) => {
                out.push(CheckResult::fail(
                    SECTION,
                    format!("agent: {}", label.as_str()),
                    format!("failed to read skeleton: {e:#}"),
                ));
                continue;
            }
        };

        let Some(cfg) = skeleton else {
            out.push(CheckResult::warn(
                SECTION,
                format!("agent: {}", label.as_str()),
                format!(
                    "registered {role_str} `{}` has no `.clank/agents/{}/config.json` yet; \
                     run `clank as {}` from inside the agent's session to bind",
                    label.as_str(),
                    label.as_str(),
                    label.as_str(),
                ),
            ));
            continue;
        };

        let session_desc = cfg
            .session
            .as_ref()
            .map(|s| {
                format!(
                    "{} bound to {} ({})",
                    s.tool.as_str(),
                    s.id.as_str(),
                    s.updated_at
                )
            })
            .unwrap_or_else(|| "unbound".to_string());
        // Report the EFFECTIVE auto mode with provenance, via the same
        // resolver `auto status` uses — the raw per-agent field alone
        // reads as "off" when a user-global default is actually driving
        // (grok-first-turn-orchestration).
        let (auto_mode, auto_source) =
            crate::cli::team::resolve_effective_auto_mode_with_source(Some(&cfg), home);
        let auto_desc = match auto_source {
            clank_core::agent_config::AutoModeSource::PerAgent => {
                format!("{} (per-agent)", auto_mode.as_str())
            }
            clank_core::agent_config::AutoModeSource::UserDefault => {
                format!("{} (user default; per-agent unset)", auto_mode.as_str())
            }
            clank_core::agent_config::AutoModeSource::Builtin => {
                format!("{} (builtin default; nothing set)", auto_mode.as_str())
            }
        };
        let base_msg = format!(
            "{}: auto_mode={auto_desc}, session={}",
            agent_config_path(repo, label).display(),
            session_desc,
        );
        let agent_check = if cfg.session.is_none() {
            CheckResult::warn(
                SECTION,
                format!("agent: {}", label.as_str()),
                format!(
                    "{base_msg} — registered {role_str} is unbound; \
                     run `clank as {}` from inside the agent's session to bind",
                    label.as_str()
                ),
            )
        } else {
            CheckResult::ok(SECTION, format!("agent: {}", label.as_str()), base_msg)
        };
        out.push(agent_check);
    }

    // Orphan-skeleton check: walk .clank/agents/ and flag any
    // directory whose label isn't in the registered set.
    if agents_root(repo).is_dir() {
        let registered_labels: std::collections::HashSet<&str> =
            members.iter().map(|(l, _, _)| l.as_str()).collect();
        match load_all_agent_configs(repo) {
            Ok(skeletons) => {
                for (skel_label, _) in &skeletons {
                    if !registered_labels.contains(skel_label.as_str()) {
                        out.push(CheckResult::warn(
                            SECTION,
                            format!("agent: {}", skel_label.as_str()),
                            format!(
                                "found `.clank/agents/{}/config.json` but `{}` is not in this repo's \
                                 agent roster (orphan state); add it via `clank agent add {}`, or `rm -rf .clank/agents/{}/` to remove it",
                                skel_label.as_str(),
                                skel_label.as_str(),
                                skel_label.as_str(),
                                skel_label.as_str(),
                            ),
                        ));
                    }
                }
            }
            Err(e) => out.push(CheckResult::warn(
                SECTION,
                "agents",
                format!("failed to scan skeleton dirs for orphans: {e:#}"),
            )),
        }
    }

    out
}

/// Probe whether git considers `rel` ignored. If `expect_tracked`,
/// "ignored" is a Warn (means the root gitignore lacks a needed
/// carve-out). The probed path doesn't need to exist on disk —
/// git matches patterns, not files.
/// Classify installed-vs-capable loop-mode drift
/// (claude-asyncrewake-work-loop). Pure; `None` = healthy.
pub(crate) fn claude_mode_drift(
    installed_async: bool,
    session_start_installed: bool,
    capable: bool,
) -> Option<String> {
    match (installed_async, capable) {
        (true, false) => Some(
            "the installed claude Stop hook is asyncrewake but this Claude Code \
             cannot honor asyncRewake — the park would run as a SYNCHRONOUS hook; \
             run `clank setup`"
                .to_string(),
        ),
        (false, true) => Some(
            "this Claude Code supports asyncRewake but the installed Stop hook is \
             the legacy background-arm loop; run `clank setup` to upgrade"
                .to_string(),
        ),
        (true, true) if !session_start_installed => Some(
            "asyncrewake Stop hook installed without its SessionStart companion \
             (generation minting + catch-up); run `clank setup`"
                .to_string(),
        ),
        _ => None,
    }
}

/// The mode check against the LIVE settings + binary. `None` when
/// the settings or the claude binary are absent (other checks cover
/// those).
fn check_claude_loop_mode(settings: &Path) -> Option<CheckResult> {
    const SECTION: &str = "user";
    let raw = std::fs::read_to_string(settings).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let entry_cmd = |event: &str, id: &str| -> Option<String> {
        v.get("hooks")?
            .get(event)?
            .as_array()?
            .iter()
            .flat_map(|w| {
                w.get("hooks")
                    .and_then(|h| h.as_array())
                    .into_iter()
                    .flatten()
            })
            .find(|h| h.get("id").and_then(|i| i.as_str()) == Some(id))
            .and_then(|h| h.get("command").and_then(|c| c.as_str()).map(String::from))
    };
    let stop_cmd = entry_cmd("Stop", "clank-stop-hook")?;
    let installed_async = stop_cmd.contains("--loop asyncrewake");
    let session_start_installed =
        entry_cmd("SessionStart", crate::cli::setup::SESSION_START_HOOK_ID).is_some();
    let capable = crate::cli::setup::probe_claude_asyncrewake()?;
    Some(
        match claude_mode_drift(installed_async, session_start_installed, capable) {
            Some(msg) => CheckResult::warn(SECTION, "claude loop mode", msg),
            None => CheckResult::ok(
                SECTION,
                "claude loop mode",
                if installed_async {
                    "asyncrewake (matches installed Claude Code)"
                } else {
                    "legacy background-arm (matches installed Claude Code)"
                },
            ),
        },
    )
}

/// The executable `clank agent start` will exec for a declaration:
/// explicit `launch.command`, else the tool's bare name. Pure —
/// the $PATH probe stays at the call site.
fn effective_launch_program(desc: &crate::cli::teams_config::AgentDescription) -> String {
    desc.launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| desc.tool.as_str().to_string())
}

fn check_gitignore_probe(repo: &Path, rel: &str, expect_tracked: bool) -> CheckResult {
    const SECTION: &str = "repo";
    let ignored_by = crate::git_io::check_ignore(repo, rel);
    if let (true, Some(line)) = (expect_tracked, ignored_by) {
        CheckResult::warn(
            SECTION,
            format!("gitignore probe: {rel}"),
            format!(
                "expected TRACKED but git reports ignored. Add the recommended \
                 carve-out to the root .gitignore. Source: {line}"
            ),
        )
    } else {
        CheckResult::ok(
            SECTION,
            format!("gitignore probe: {rel}"),
            format!("tracked (as expected)"),
        )
    }
}

/// The root `.gitignore` should own NO `.clank/` rules under the single-source
/// allow-list — leftovers are redundant and a drift hazard. Warn (don't fail).
fn check_root_has_no_clank_rules(repo: &Path) -> CheckResult {
    const SECTION: &str = "repo";
    const NAME: &str = "root .gitignore";
    let body = match std::fs::read_to_string(repo.join(".gitignore")) {
        Ok(b) => b,
        Err(_) => return CheckResult::ok(SECTION, NAME, "no `.clank/` rules".to_string()),
    };
    let offenders: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| l.strip_prefix('!').unwrap_or(l).starts_with(".clank/"))
        .map(str::to_string)
        .collect();
    if offenders.is_empty() {
        CheckResult::ok(SECTION, NAME, "no `.clank/` rules".to_string())
    } else {
        CheckResult::warn(
            SECTION,
            NAME,
            format!(
                "redundant `.clank/` rule(s): {} — remove them; `.clank/.gitignore` is the single source of truth",
                offenders.join(", ")
            ),
        )
    }
}

fn check_claude_perms(repo: &Path) -> CheckResult {
    const SECTION: &str = "repo";
    const NAME: &str = ".claude/settings.local.json";
    const REQUIRED: &[&str] = &[
        "Write(.clank/agents/**)",
        "Edit(.clank/agents/**)",
        "Read(.clank/agents/**)",
    ];
    let path = repo.join(".claude/settings.local.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::warn(
                SECTION,
                NAME,
                "missing — run `clank init` to write the agent edit-permission rules".to_string(),
            );
        }
        Err(e) => return CheckResult::fail(SECTION, NAME, format!("read failed: {e}")),
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return CheckResult::fail(SECTION, NAME, format!("parse failed: {e}")),
    };
    let allow = match v
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
    {
        Some(a) => a,
        None => {
            return CheckResult::warn(
                SECTION,
                NAME,
                "missing `permissions.allow` array — run `clank init`".to_string(),
            );
        }
    };
    let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
    let missing: Vec<&&str> = REQUIRED.iter().filter(|r| !entries.contains(r)).collect();
    if missing.is_empty() {
        CheckResult::ok(
            SECTION,
            NAME,
            "all three agent edit-permission rules present".to_string(),
        )
    } else {
        let names: Vec<String> = missing.iter().map(|s| (**s).to_string()).collect();
        CheckResult::warn(
            SECTION,
            NAME,
            format!(
                "missing rules: {} — run `clank init` to add",
                names.join(", ")
            ),
        )
    }
}

fn user_checks() -> Vec<CheckResult> {
    const SECTION: &str = "user";
    let mut out = Vec::<CheckResult>::new();

    let home = match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) => h,
        None => {
            out.push(CheckResult::fail(
                SECTION,
                "home",
                "HOME env var unset — can't locate ~/.claude or ~/.codex".to_string(),
            ));
            return out;
        }
    };

    // Every user-scope asset, from THE inventory setup installs from
    // (setup/doctor parity by construction — codex 0c90514). This
    // includes the opencode plugin: a missing or drifted plugin means
    // opencode agents never bind and never receive work, silently
    // (the wire is exit-0/empty by design).
    let claude_async = crate::cli::setup::probe_claude_asyncrewake() == Some(true);
    for asset in crate::cli::setup::user_asset_inventory(claude_async) {
        // Stale canonical mode reads differently from arbitrary
        // drift: plain `clank setup` migrates it, no --force needed.
        let path = home.join(&asset.rel);
        let display = format!("~/{}", asset.rel);
        if let Ok(existing) = std::fs::read_to_string(&path)
            && existing != asset.expected
            && asset.canonical_alternates.contains(&existing)
        {
            out.push(CheckResult::warn(
                SECTION,
                display.clone(),
                format!(
                    "{display} is the canonical skill from the OTHER claude loop \
                     mode; run `clank setup` (no --force needed) to migrate"
                ),
            ));
            continue;
        }
        out.push(check_skill_file(&path, &asset.expected, &display));
    }
    // The pre-split `clank` skill must be gone — left in place it
    // shadows the role skills with stale, role-jamming guidance.
    for (_, tool_dir) in crate::cli::setup::TOOL_SKILL_DIRS {
        let obsolete = home.join(format!("{tool_dir}/skills/clank"));
        if obsolete.join("SKILL.md").exists() {
            out.push(CheckResult::warn(
                SECTION,
                "skills",
                format!(
                    "~/{tool_dir}/skills/clank is the obsolete pre-split skill; \
                     run `clank setup --force` to remove it (now clank-master / clank-reviewer)"
                ),
            ));
        }
    }
    // claude-asyncrewake-work-loop: the installed entry's delivery
    // mode is a durable setup-time decision — flag drift in BOTH
    // directions (a stale async entry on a downgraded claude is a
    // SYNCHRONOUS day-long hook; a legacy entry on a capable claude
    // wastes the whole point).
    if let Some(check) = check_claude_loop_mode(&home.join(".claude/settings.json")) {
        out.push(check);
    }
    // Hook entries.
    out.push(check_hook_entry(
        &home.join(".claude/settings.json"),
        "claude",
        "~/.claude/settings.json",
    ));
    out.push(check_hook_entry(
        &home.join(".codex/hooks.json"),
        "codex",
        "~/.codex/hooks.json",
    ));
    out.push(check_codex_rule(
        &home.join(".codex/rules/default.rules"),
        "~/.codex/rules/default.rules",
    ));

    out
}

/// Check that codex's command-rules file contains a bare `clank`
/// allow rule. Warn if missing — without it, codex sessions
/// prompt the user to continue each `clank <subcommand>`
/// invocation it sees for the first time.
fn check_codex_rule(path: &Path, display: &str) -> CheckResult {
    const SECTION: &str = "user";
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::warn(
                SECTION,
                display,
                "missing — run `clank setup` to add the `clank` allow rule \
                 (without it, codex prompts to continue every new clank subcommand)"
                    .to_string(),
            );
        }
        Err(e) => return CheckResult::fail(SECTION, display, format!("read failed: {e}")),
    };
    for line in body.lines() {
        if !line.contains(r#"pattern=["clank"]"#) {
            continue;
        }
        if line.contains(r#"decision="allow""#) {
            return CheckResult::ok(SECTION, display, "`clank` allow rule present");
        }
        if line.contains(r#"decision="deny""#) {
            return CheckResult::warn(
                SECTION,
                display,
                format!(
                    "`clank` is DENIED at line `{}` — remove or change it then run `clank setup`",
                    line.trim()
                ),
            );
        }
    }
    CheckResult::warn(
        SECTION,
        display,
        "`clank` allow rule missing — run `clank setup` to add it".to_string(),
    )
}

fn check_skill_file(path: &Path, expected: &str, display: &str) -> CheckResult {
    const SECTION: &str = "user";
    match std::fs::read_to_string(path) {
        Ok(s) if s == expected => CheckResult::ok(SECTION, display, "matches embedded content"),
        Ok(_) => CheckResult::warn(
            SECTION,
            display,
            "drifted from embedded content — run `clank setup --force` to refresh".to_string(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            CheckResult::warn(SECTION, display, "missing — run `clank setup`".to_string())
        }
        Err(e) => CheckResult::fail(SECTION, display, format!("read failed: {e}")),
    }
}

fn check_hook_entry(path: &Path, tool: &str, display: &str) -> CheckResult {
    const SECTION: &str = "user";
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::warn(SECTION, display, "missing — run `clank setup`".to_string());
        }
        Err(e) => return CheckResult::fail(SECTION, display, format!("read failed: {e}")),
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return CheckResult::fail(SECTION, display, format!("parse failed: {e}")),
    };
    let stop = match v
        .get("hooks")
        .and_then(|h| h.get("Stop"))
        .and_then(|s| s.as_array())
    {
        Some(s) => s,
        None => {
            return CheckResult::warn(
                SECTION,
                display,
                "no Stop hooks configured — run `clank setup`".to_string(),
            );
        }
    };
    let has_clank = stop.iter().any(|wrapper| {
        let Some(inner) = wrapper.get("hooks").and_then(|h| h.as_array()) else {
            return false;
        };
        inner.iter().any(|h| {
            h.get("id").and_then(|v| v.as_str()) == Some("clank-stop-hook")
                || h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|cmd| cmd.starts_with("clank stop-hook"))
        })
    });
    if has_clank {
        CheckResult::ok(SECTION, display, format!("{tool} Stop hook installed"))
    } else {
        CheckResult::warn(
            SECTION,
            display,
            format!("no clank Stop hook in {tool} config — run `clank setup`"),
        )
    }
}

fn session_checks(repo: Option<&Path>) -> Vec<CheckResult> {
    const SECTION: &str = "session";
    let mut out = Vec::<CheckResult>::new();

    // Read CLANK_AGENT FIRST. The pure resolver's precedence
    // says the explicit override wins before any session env is
    // touched; doctor must mirror that or it'll report failures
    // for states `clank wait` / `clank auto` happily accept.
    let explicit = match explicit_label_from_env() {
        Ok(e) => e,
        Err(e) => {
            out.push(CheckResult::warn(
                SECTION,
                "CLANK_AGENT",
                format!("invalid override: {e}"),
            ));
            None
        }
    };
    if let Some(label) = &explicit {
        out.push(CheckResult::ok(
            SECTION,
            "CLANK_AGENT",
            format!("explicit override active: `{}`", label.as_str()),
        ));
    }

    // Now report session env. Session-env errors are ALWAYS Warn
    // (never Fail) so they can't block the identity check below
    // when CLANK_AGENT's override would still resolve. The
    // resolver itself is the authoritative success/failure
    // signal for whether downstream commands will work.
    let detected: Option<(clank_core::Tool, clank_core::ids::SessionId)> =
        match detect_session_from_env() {
            Ok(d) => {
                match &d {
                    Some((tool, session_id)) => out.push(CheckResult::ok(
                        SECTION,
                        "env",
                        format!(
                            "running inside {} (session {})",
                            tool.as_str(),
                            session_id.as_str()
                        ),
                    )),
                    None => out.push(CheckResult::ok(
                        SECTION,
                        "env",
                        "not running inside an agent (no CLAUDE_CODE_SESSION_ID / \
                         CODEX_THREAD_ID)"
                            .to_string(),
                    )),
                }
                d
            }
            Err(e) => {
                out.push(CheckResult::warn(
                    SECTION,
                    "env",
                    format!("session env unparseable: {e}"),
                ));
                None
            }
        };

    let Some(repo) = repo else {
        out.push(CheckResult::warn(
            SECTION,
            "identity",
            "not in a clank repo — can't resolve identity".to_string(),
        ));
        return out;
    };

    // Identity resolution: go through the SAME resolver `clank
    // wait` / `clank auto` use, so doctor never disagrees with
    // what those commands would do. Critically, this honors the
    // CLANK_AGENT > session-binding precedence — an explicit
    // override succeeds even without a matching session config.
    let resolved = match resolve_identity_from_env(repo) {
        Ok(label) => label,
        Err(e) => {
            out.push(CheckResult::fail(SECTION, "identity", format!("{e:#}")));
            return out;
        }
    };
    let source = describe_identity_source(repo, &resolved, &detected, &explicit);
    out.push(CheckResult::ok(SECTION, "identity", source));

    // Inferred role from the roster resolver. The shared best-effort
    // formatter distinguishes a missing master from this identity having
    // been removed from an otherwise valid roster.
    let role = crate::agent_store::role_status_text(repo, &resolved);
    out.push(CheckResult::ok(
        SECTION,
        "role",
        format!("inferred role: {role}"),
    ));

    out
}

/// Describe WHERE the resolver got the label: explicit
/// CLANK_AGENT override vs which agent's session binding
/// matched. Pure diagnostic — the resolved label itself is the
/// authoritative answer.
fn describe_identity_source(
    repo: &Path,
    resolved: &clank_core::ids::AgentLabel,
    detected: &Option<(clank_core::vocab::Tool, clank_core::ids::SessionId)>,
    explicit: &Option<clank_core::ids::AgentLabel>,
) -> String {
    if let Some(label) = explicit
        && label == resolved
    {
        return format!(
            "resolved to `{}` via CLANK_AGENT override",
            resolved.as_str()
        );
    }
    if let Some((tool, sid)) = detected {
        let agents = load_all_agent_configs(repo).unwrap_or_default();
        let bound = agents.iter().find(|(label, cfg)| {
            label == resolved
                && cfg
                    .session
                    .as_ref()
                    .is_some_and(|s| &s.id == sid && s.tool == *tool)
        });
        if let Some((label, cfg)) = bound {
            let updated_at = cfg
                .session
                .as_ref()
                .map(|s| s.updated_at.clone())
                .unwrap_or_default();
            return format!(
                "resolved to `{}` via session binding (last bound {})",
                label.as_str(),
                updated_at
            );
        }
    }
    format!("resolved to `{}` (source unknown)", resolved.as_str())
}

/// `clank doctor --json` row (one per check). Borrows from a
/// [`CheckResult`]; `status` is the lowercase wire string via
/// `CheckStatus`'s rename_all (typed-json-not-json-macro).
#[derive(serde::Serialize)]
struct CheckJson<'a> {
    section: &'a str,
    name: &'a str,
    status: CheckStatus,
    message: &'a str,
}

/// Serialize checks to the `--json` wire array (one object per
/// check). `pub` so in-process callers (tests) assert on the
/// exact shape `clank doctor --json` emits without a spawn.
/// Plan: dogfood-init-setup-in-tests (Phase B).
pub fn checks_to_json(results: &[CheckResult]) -> serde_json::Value {
    let payload: Vec<CheckJson> = results
        .iter()
        .map(|r| CheckJson {
            section: r.section,
            name: &r.name,
            status: r.status,
            message: &r.message,
        })
        .collect();
    serde_json::to_value(payload).expect("serialize doctor checks")
}

fn render(results: &[CheckResult], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(&checks_to_json(results))?);
        return Ok(());
    }

    let mut current_section: Option<&str> = None;
    for r in results {
        if Some(r.section) != current_section {
            println!("\n[{}]", r.section);
            current_section = Some(r.section);
        }
        let tag = match r.status {
            CheckStatus::Ok => "OK   ",
            CheckStatus::Warn => "WARN ",
            CheckStatus::Fail => "FAIL ",
        };
        println!("  {tag}{}: {}", r.name, r.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_mode_drift_matrix() {
        // Both drift directions warn; matched modes are healthy; a
        // missing SessionStart companion under async is drift too.
        assert!(super::claude_mode_drift(true, true, true).is_none());
        assert!(super::claude_mode_drift(false, false, false).is_none());
        let stale_async = super::claude_mode_drift(true, true, false).unwrap();
        assert!(stale_async.contains("SYNCHRONOUS"), "{stale_async}");
        let stale_legacy = super::claude_mode_drift(false, false, true).unwrap();
        assert!(stale_legacy.contains("upgrade"), "{stale_legacy}");
        let no_companion = super::claude_mode_drift(true, false, true).unwrap();
        assert!(no_companion.contains("SessionStart"), "{no_companion}");
    }

    #[test]
    fn effective_launch_program_prefers_command_then_tool_default() {
        // The $PATH probe checks THIS resolution — an opencode agent
        // with no launch block must resolve to the bare `opencode`
        // (previously the default case was never checked at all).
        use crate::cli::teams_config::AgentDescription;
        use clank_core::vocab::Tool;
        let desc = AgentDescription {
            tool: Tool::OpenCode,
            launch: None,
            initial_prompt: None,
        };
        assert_eq!(super::effective_launch_program(&desc), "opencode");
        let desc = AgentDescription {
            tool: Tool::OpenCode,
            launch: Some(clank_core::agent_config::LaunchConfig {
                command: Some("my-opencode-wrapper".into()),
                ..Default::default()
            }),
            initial_prompt: None,
        };
        assert_eq!(
            super::effective_launch_program(&desc),
            "my-opencode-wrapper"
        );
    }

    use super::*;

    fn init_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let s = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(s.success());
        dir
    }

    #[test]
    fn checks_to_json_matches_prior_shape() {
        // typed-json-not-json-macro: checks_to_json must serialize to
        // the same keys+values the old `json!` produced — the
        // `clank doctor --json` array the integration test parses.
        // `json!` here expresses the expected value; key order is
        // irrelevant (`to_value` equality is order-independent).
        let results = vec![
            CheckResult::ok("repo", "a".to_string(), "all good".to_string()),
            CheckResult::warn("user", "b".to_string(), "heads up".to_string()),
            CheckResult::fail("session", "c".to_string(), "broken".to_string()),
        ];
        assert_eq!(
            checks_to_json(&results),
            serde_json::json!([
                {"section": "repo", "name": "a", "status": "ok", "message": "all good"},
                {"section": "user", "name": "b", "status": "warn", "message": "heads up"},
                {"section": "session", "name": "c", "status": "fail", "message": "broken"},
            ])
        );
    }

    #[test]
    fn placement_capability_is_reported_without_probing() {
        // The row is driven by an INJECTED value, so the check is
        // deterministic and `repo_checks` spawns nothing — and it is
        // reported outside a zellij session too, which is when
        // learning your client cannot place panes is most useful.
        use crate::cli::open_zellij::PlacementCapability as P;
        let dir = tempfile::tempdir().unwrap();
        let row = |cap: Option<P>| {
            repo_checks(dir.path(), None, cap)
                .into_iter()
                .find(|r| r.name == "zellij pane placement")
        };
        assert!(row(None).is_none(), "unknown capability says nothing");
        assert!(
            row(Some(P::NoZellij)).is_none(),
            "no zellij at all is not a problem to report"
        );
        assert_eq!(
            row(Some(P::ClientTooOld)).map(|r| r.status),
            Some(CheckStatus::Warn),
            "a client without stack-panes must warn: the only other symptom \
             is panes in the wrong place"
        );
        assert_eq!(
            row(Some(P::ClientSupports)).map(|r| r.status),
            Some(CheckStatus::Ok)
        );
    }

    #[test]
    fn repo_checks_warn_on_missing_gitignore() {
        let dir = init_git_repo();
        let results = repo_checks(dir.path(), None, None);
        let gi = results
            .iter()
            .find(|r| r.name == ".clank/.gitignore")
            .expect("gitignore check ran");
        assert_eq!(gi.status, CheckStatus::Warn);
        assert!(gi.message.contains("clank init"));
    }

    #[test]
    fn repo_checks_flag_missing_binary_for_a_skeletonless_agent() {
        // The exact fresh state the check exists for (codex 00fc9b4):
        // rostered via `clank agent add`, never bound, no
        // .clank/agents/<label>/ skeleton — launch feasibility comes
        // from the roster description alone and must be diagnosed
        // before the missing-skeleton early-out.
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        use clank_core::vocab::Tool;
        let dir = init_git_repo();
        crate::cli::agent::add_repo_roster_agent(
            dir.path(),
            &clank_core::ids::AgentLabel::parse("kimi").unwrap(),
            AgentDescription {
                tool: Tool::OpenCode,
                launch: Some(clank_core::agent_config::LaunchConfig {
                    command: Some("definitely-missing-opencode-binary".into()),
                    ..Default::default()
                }),
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        let results = repo_checks(dir.path(), None, None);
        let launch = results
            .iter()
            .find(|r| r.name == "agent: kimi launch")
            .expect("launch check ran despite the missing skeleton");
        assert_eq!(launch.status, CheckStatus::Warn);
        assert!(
            launch
                .message
                .contains("definitely-missing-opencode-binary")
        );
        // And the skeleton-less state still gets its own warning —
        // the launch check runs IN ADDITION, not instead.
        let agent = results
            .iter()
            .find(|r| r.name == "agent: kimi")
            .expect("agent check ran");
        assert_eq!(agent.status, CheckStatus::Warn);
    }

    #[test]
    fn repo_checks_warn_on_missing_claude_perms() {
        let dir = init_git_repo();
        let results = repo_checks(dir.path(), None, None);
        let perm = results
            .iter()
            .find(|r| r.name == ".claude/settings.local.json")
            .expect("perms check ran");
        assert_eq!(perm.status, CheckStatus::Warn);
        assert!(perm.message.contains("clank init"));
    }

    #[test]
    fn check_skill_file_ok_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SKILL.md");
        std::fs::write(&p, "expected").unwrap();
        let r = check_skill_file(&p, "expected", "test");
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn check_skill_file_warn_on_drift() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SKILL.md");
        std::fs::write(&p, "drifted").unwrap();
        let r = check_skill_file(&p, "expected", "test");
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("--force"));
    }

    #[test]
    fn check_skill_file_warn_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = check_skill_file(&dir.path().join("nope.md"), "x", "test");
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.message.contains("clank setup"));
    }

    #[test]
    fn check_hook_entry_ok_when_id_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[
                {"hooks":[{"id":"clank-stop-hook","type":"command","command":"clank stop-hook --tool claude"}]}
            ]}}"#,
        )
        .unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Ok);
    }

    #[test]
    fn check_hook_entry_warn_when_missing_clank_hook() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[
                {"hooks":[{"type":"command","command":"/other/hook"}]}
            ]}}"#,
        )
        .unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn check_hook_entry_warn_when_no_stop_array() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, r#"{"hooks":{}}"#).unwrap();
        let r = check_hook_entry(&p, "claude", "test");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    /// Helper: run a closure with a focused env state and
    /// restore everything afterward.
    ///
    /// Tests run in parallel by default and the process env is
    /// global, so we hold a static mutex across the entire
    /// save→set→run→restore window. The lock spans the closure
    /// execution itself — releasing the lock around just
    /// set/restore would let a concurrent test observe our
    /// mid-run env. Codex flagged this on c1414f2.
    ///
    /// SAFETY (`set_var`/`remove_var`): holding `ENV_LOCK`
    /// serializes every caller of `with_env` against every
    /// other caller. Tests that read env outside `with_env`
    /// would still be racy in principle; nothing in this
    /// module does that.
    fn with_env<F: FnOnce()>(set: &[(&str, &str)], clear: &[&str], f: F) {
        use std::sync::{Mutex, OnceLock};
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = ENV_LOCK.get_or_init(|| Mutex::new(()));
        // Poisoned-lock recovery: if a prior test panicked
        // inside this critical section we still restored the
        // env via catch_unwind below, so the poison flag
        // carries no real risk.
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

        let saved: Vec<(String, Option<std::ffi::OsString>)> = set
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var_os(k)))
            .chain(clear.iter().map(|k| (k.to_string(), std::env::var_os(k))))
            .collect();
        unsafe {
            for k in clear {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    #[test]
    fn session_checks_clank_agent_override_resolves_without_session_binding() {
        // Regression: codex 3808a0d called out that doctor was
        // failing identity when CLANK_AGENT was set but no agent
        // config had a matching session binding. The pure
        // resolver succeeds in that case (override > session
        // lookup), and doctor must mirror that — otherwise it
        // disagrees with `clank wait` / `clank auto` which both
        // happily run with just the override.
        let dir = init_git_repo();
        let repo = dir.path().to_path_buf();

        with_env(
            &[("CLANK_AGENT", "alice")],
            &["CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID"],
            || {
                let results = session_checks(Some(&repo));
                let identity = results
                    .iter()
                    .find(|r| r.name == "identity")
                    .expect("identity check ran");
                assert_eq!(
                    identity.status,
                    CheckStatus::Ok,
                    "expected OK for CLANK_AGENT override; got {:?}: {}",
                    identity.status,
                    identity.message
                );
                assert!(
                    identity.message.contains("alice") && identity.message.contains("CLANK_AGENT"),
                    "identity message should attribute to override: {}",
                    identity.message
                );
            },
        );
    }

    #[test]
    fn session_checks_clank_agent_override_wins_over_both_session_envs() {
        // Regression: codex 7108d87 — env detection was bailing
        // on the both-set case BEFORE the resolver got a chance.
        // With CLANK_AGENT set, resolver succeeds via override
        // even when both session env vars are set (leaked from
        // a parent shell). Env should Warn, identity should OK.
        let dir = init_git_repo();
        let repo = dir.path().to_path_buf();

        with_env(
            &[
                ("CLANK_AGENT", "alice"),
                (
                    "CLAUDE_CODE_SESSION_ID",
                    "742f6a04-f174-409a-ab01-419a16c5f372",
                ),
                ("CODEX_THREAD_ID", "019e5385-ed97-7603-8561-dd9024328ff9"),
            ],
            &[],
            || {
                let results = session_checks(Some(&repo));
                let env = results
                    .iter()
                    .find(|r| r.name == "env")
                    .expect("env check ran");
                assert_eq!(
                    env.status,
                    CheckStatus::Warn,
                    "both-set env should Warn (not Fail) so identity can resolve via override: {}",
                    env.message
                );
                let identity = results
                    .iter()
                    .find(|r| r.name == "identity")
                    .expect("identity check ran");
                assert_eq!(
                    identity.status,
                    CheckStatus::Ok,
                    "identity must resolve via override despite env warn; got {:?}: {}",
                    identity.status,
                    identity.message
                );
                assert!(identity.message.contains("alice"));
            },
        );
    }

    #[test]
    fn session_checks_clank_agent_override_wins_over_malformed_session_env() {
        // Same precedence: an unparseable session id env
        // shouldn't block identity when CLANK_AGENT is set.
        let dir = init_git_repo();
        let repo = dir.path().to_path_buf();

        with_env(
            &[
                ("CLANK_AGENT", "alice"),
                ("CLAUDE_CODE_SESSION_ID", "not/a/valid/session"),
            ],
            &["CODEX_THREAD_ID"],
            || {
                let results = session_checks(Some(&repo));
                let env = results
                    .iter()
                    .find(|r| r.name == "env")
                    .expect("env check ran");
                assert_eq!(
                    env.status,
                    CheckStatus::Warn,
                    "malformed session id should Warn, not Fail: {}",
                    env.message
                );
                let identity = results
                    .iter()
                    .find(|r| r.name == "identity")
                    .expect("identity check ran");
                assert_eq!(
                    identity.status,
                    CheckStatus::Ok,
                    "identity must resolve via override despite malformed env: {}",
                    identity.message
                );
                assert!(identity.message.contains("alice"));
            },
        );
    }
}
