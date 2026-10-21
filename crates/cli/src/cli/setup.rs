//! `clank setup` — install user-scope clank assets into
//! `~/.claude/`, `~/.codex/`, `~/.grok/`, and `~/.config/opencode/`.
//!
//! Writes skill files (refuse-if-drifted; D8) and merges clank's
//! hook entries into each agent's user-wide hook config, owning
//! them by the command they run (see [`hook_is_clank`]).
//!
//! Per the plan (D1) clank ships as a CLI that owns these
//! files — no separate plugin distribution. Re-running `clank
//! setup` after upgrading the binary refreshes everything.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::vocab::{Role, Tool};

use super::SetupArgs;

// The `clank` skill is split by ROLE, not by tool: one tool hosts
// multiple roles (claude runs both the master and a reviewer), so a
// per-tool skill can't be role-specific. Each role's SKILL.md is
// composed from a shared core + a role body (+ a claude-only slash
// command) with the few tool-specific bits substituted, so there is
// ONE source per role. See [`compose_skill`].
const SKILL_SHARED_CORE: &str = include_str!("setup_assets/skill_shared_core.md");
const SKILL_MASTER_BODY: &str = include_str!("setup_assets/skill_master.md");
const SKILL_REVIEWER_BODY: &str = include_str!("setup_assets/skill_reviewer.md");
const SKILL_SLASH_COMMAND: &str = include_str!("setup_assets/skill_slash_command.md");

/// The github event-inbox skill (github-offline-catchup). Tool-neutral:
/// the events CLI and the react-then-ack loop are identical everywhere,
/// so one body installs to every skill dir.
pub const GITHUB_SKILL_BODY: &str = include_str!("setup_assets/github_events_skill.md");

/// The opencode plugin: session BINDING via `shell.env` + the
/// `session.idle` work loop (opencode-agent-tool M2). Content
/// invariants pinned by tests below; installed by `clank setup`.
pub const OPENCODE_PLUGIN: &str = include_str!("setup_assets/opencode_plugin.js");

/// The two role skills, by their `~/.<tool>/skills/<name>/` dir name.
const MASTER_SKILL: &str = "clank-master";
const REVIEWER_SKILL: &str = "clank-reviewer";
/// The pre-split single skill, removed on setup.
const OBSOLETE_SKILL: &str = "clank";

/// Files an earlier setup installed that this build no longer ships,
/// relative to the provider root.
const RETIRED_ASSETS: &[&str] = &["skills/clank-pr-review/SKILL.md"];

/// `description:` frontmatter — the PRIMARY lever for role selection.
/// Lead with the role guard ("Use ONLY when … Do NOT use …") and name
/// the other skill, so an agent never loads the wrong role's skill (and
/// runs commands it must never touch).
const MASTER_DESC: &str = "Clank multi-agent workflow, MASTER role. Use ONLY when you are the master in a clank repo (implement complete plans, promote/finish plans, manage the roster). Do NOT use as a reviewer or on standby — use clank-reviewer instead.";
const REVIEWER_DESC: &str = "Clank multi-agent workflow, REVIEWER and STANDBY roles. Use ONLY when you are a reviewer or on standby in a clank repo. Do NOT use as the master — use clank-master instead.";

/// Claude's Stop hook owns its wait and returns the wake as a
/// continuation. The agent ends its turn; it does not arm a task.
/// One tool's work-delivery teaching: how work ARRIVES, and the one
/// thing the agent must never do about it.
///
/// Two fields rather than one blob because the second is an invariant
/// every loop owes and the first is mechanism. A loop written without
/// a prohibition does not compile. A loop with its prohibition buried
/// in prose only fails a substring search someone has to remember to
/// keep in step — which is exactly what happened: `80f01612` rewrote
/// claude's rule correctly and changed its capitalization in the same
/// line, and the test had been asserting the capitalization.
pub(crate) struct WorkLoop {
    arrival: &'static str,
    /// The WORDING differs per tool because the mechanism does. Where
    /// the hook or the plugin owns the wait the rule is "do not arm
    /// one"; where the agent owns it (grok) the rule is the opposite
    /// half — arm exactly one, in the background, and do not spin.
    /// Only the subject is common, so only the subject is asserted.
    pub(crate) prohibition: &'static str,
}

const WORK_LOOP_CLAUDE: WorkLoop = WorkLoop {
    arrival: "\
- **Act on Stop-hook work immediately, then end your turn.** The Stop
  hook watches for clank work and wakes you with the items. Run
  `clank status` if you need more than the hint carries (short SHAs
  resolve wherever a `<sha>` is wanted).
  Each item is a one-line hint: kind, plan, short sha.",
    prohibition: "\
- **NEVER poll.** Do not start a background `clank wait` or re-run
  `clank status` while waiting. End your turn; the Stop hook owns the
  wait and hands you new work.",
};

/// Claude under the ASYNCREWAKE loop (claude-asyncrewake-work-loop):
/// the Stop hook parks the watcher itself and work arrives as a
/// system-reminder wake — the agent must never arm anything. The
/// legacy text below stays for machines whose Claude Code predates
/// asyncRewake (setup decides per machine).
const WORK_LOOP_CLAUDE_ASYNC: WorkLoop = WorkLoop {
    arrival: "\
- **Work arrives on its own.** When you end a turn, clank parks a
  watcher inside the Stop hook; when work exists you are WOKEN with
  the items as a system reminder (\"Clank wait returned work…\").
  Act on them IMMEDIATELY. Run `clank status` if you need more than
  the hint carries (short SHAs resolve wherever a `<sha>` is wanted).
  Each item is a one-line hint: kind, plan, short sha.",
    prohibition: "\
- **NEVER run `clank wait` yourself**, foreground or background —
  the hook owns the wait and presents pending work. Do not re-run `clank status` waiting for
  state to change. End your turn; work finds you.",
};

const WORK_LOOP_CODEX: WorkLoop = WorkLoop {
    arrival: "\
- **Act on Stop-hook work IMMEDIATELY, then YIELD.** When the Stop hook
  hands you work, do it now. codex surfaces this as a
  `Stop hook (blocked) feedback:` message. Run `clank status` if you
  need more than the hint carries (short SHAs resolve wherever a `<sha>`
  is wanted).
  Each item is a one-line hint: kind, plan, short sha.",
    prohibition: "\
- **NEVER poll.** Do not loop on `clank wait` or re-run `clank status`
  waiting for state to change. STOP — the Stop hook re-invokes you when
  there is work. You WILL be woken; do not spin.",
};

/// Native Stop asks for one ordinary provider-owned background wait.
const WORK_LOOP_GROK: WorkLoop = WorkLoop {
    arrival: "\
- **Use one background wait.** On first start/resume, end your
  turn so the native Stop hook can check running tasks. If it
  requests arming, run `clank wait` with `run_terminal_command`
  and `block_until_ms: 0`, with no positive timeout.
  Keep exactly one such task. Its completion wakes you: act on the
  printed items immediately, then re-arm `clank wait` and end your turn.
  The monitor validates your bound session and rejects duplicate waits.
  A failed wait is not proof that work was handled.
  Run `clank doctor` to diagnose setup; actual completion notifications
  must be enabled in Grok. Each item names its kind, plan and short sha.",
    prohibition: "\
- **NEVER poll.** Do not use a foreground wait, a positive task timeout,
  or repeated status/task checks. Keep one background `clank wait`;
  cancel an existing task before replacing it. Use `clank auto off` for
  manual operation. Waiting never acknowledges work.",
};

/// opencode is plugin-driven (opencode-agent-tool M1/M2): the clank
/// plugin long-polls `clank stop-hook` on session.idle and INJECTS
/// work as a new prompt — from the agent's seat, work simply
/// arrives. No arming, no reminder; the skill must forbid
/// self-arming (a parked wait duplicates the plugin's deliveries
/// and litters processes).
const WORK_LOOP_OPENCODE: WorkLoop = WorkLoop {
    arrival: "\
- **Work arrives on its own.** When you end a turn, clank's opencode
  plugin watches for work and injects it as a new prompt (\"Clank wait
  returned work…\"). Act on the items IMMEDIATELY. Run `clank status`
  if you need more than the hint carries (short SHAs resolve wherever
  a `<sha>` is wanted).
  Each item is a one-line hint: kind, plan, short sha.",
    prohibition: "\
- **NEVER run `clank wait` yourself**, foreground or background — the
  plugin already holds this session's one wait; a second duplicates
  deliveries. Do not re-run `clank status` waiting for state to
  change. End your turn; work finds you.",
};

/// The loop a (tool, mode) pair teaches. Exhaustive over `Tool`, so a
/// tool added to the vocabulary must state its own discipline here
/// rather than inheriting someone else's.
pub(crate) fn work_loop(tool: Tool, claude_async: bool) -> WorkLoop {
    match tool {
        Tool::Claude if claude_async => WORK_LOOP_CLAUDE_ASYNC,
        Tool::Claude => WORK_LOOP_CLAUDE,
        Tool::Codex => WORK_LOOP_CODEX,
        Tool::Grok => WORK_LOOP_GROK,
        Tool::OpenCode => WORK_LOOP_OPENCODE,
    }
}

/// Compose a role's `SKILL.md` for a tool from the shared single-source
/// fragments: frontmatter (role-guarded description) + a role-guard
/// line + shared core + the role body, plus the claude-only `/clank`
/// slash command. Tool differences (`{{SHELL}}`, the per-tool
/// `{{WORK_LOOP}}` work-delivery teaching) are substituted, never
/// duplicated. Pure — unit-tested without touching the filesystem.
pub fn compose_skill(role: Role, tool: Tool) -> String {
    compose_skill_with(role, tool, false)
}

/// `claude_async` selects claude's work-loop teaching
/// (claude-asyncrewake-work-loop): setup passes the SAME probed mode
/// it encodes in the hook entry, and doctor verifies with the same
/// value — the skill on disk always matches the installed loop.
pub fn compose_skill_with(role: Role, tool: Tool, claude_async: bool) -> String {
    // The guard belongs to the SKILL, not to one role: the reviewer
    // skill serves standby agents too, and a guard naming only
    // `reviewer` tells a standby agent to switch to the master's.
    let (name, description, body, guard) = match role {
        Role::Master => (
            MASTER_SKILL,
            MASTER_DESC,
            SKILL_MASTER_BODY,
            format!(
                "> ROLE GUARD: this skill is for the **master** role. If your role \
                 in this repo is not master, stop and use `{REVIEWER_SKILL}` instead — \
                 your role is what `clank status` and your work items report.\n\n"
            ),
        ),
        Role::Reviewer | Role::Standby => (
            REVIEWER_SKILL,
            REVIEWER_DESC,
            SKILL_REVIEWER_BODY,
            format!(
                "> ROLE GUARD: this skill is for the **reviewer** and **standby** \
                 roles. If your role in this repo is neither, stop and use \
                 `{MASTER_SKILL}` instead — your role is what `clank status` and your \
                 work items report. A standby agent writes no verdicts and handles \
                 every other item as usual.\n\n"
            ),
        ),
    };
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {name}\n"));
    out.push_str(&format!("description: {description}\n"));
    out.push_str("---\n\n");
    out.push_str(&guard);
    out.push_str(SKILL_SHARED_CORE);
    out.push_str(body);
    if tool == Tool::Claude {
        out.push_str(SKILL_SLASH_COMMAND);
    }

    let shell = match tool {
        Tool::Claude => "Bash",
        Tool::Codex => "shell",
        Tool::Grok => "run_terminal_command",
        Tool::OpenCode => "bash",
    };
    let teaching = work_loop(tool, claude_async);
    out.replace("{{SHELL}}", shell).replace(
        "{{WORK_LOOP}}",
        &format!("{}\n{}", teaching.arrival, teaching.prohibition),
    )
}

/// Per-tool hook timeout we write into the agent's hook config.
///
/// The LARGEST value that does not WRAP: 24.9 days.
///
/// This field is in SECONDS. The runner converts it to MILLISECONDS
/// for a timer whose delay is a signed 32-bit int, capped at
/// 2147483647 ms; past that it wraps and fires on the next tick. So
/// `i32::MAX` LOOKED safely inside a 32-bit range while being a
/// thousandfold past it — 2147483647 s is 2147483647000 ms — and
/// every parked hook was cancelled within a second of starting.
/// Asking for a ceiling that never fires produced one that always
/// did.
///
/// The timeout cannot be escaped: `asyncRewake` is the only hook mode
/// that wakes on exit 2, and it is always subject to this field
/// (`async: true` is exempt but has no wake). Nor can the field be
/// dropped — absent it each runner applies its own default, and
/// claude documents 600s for a `command` hook, which would cut a
/// legitimate park far shorter. It is load-bearing, so it has to be
/// right.
///
/// Not the old 86400 either: that ceiling was genuinely reached,
/// silencing this repo's codex reviewer for 22 hours with a review
/// waiting. Raising it was correct; the VALUE was wrong.
///
/// 24.9 days is still a cliff, just a rare one — an agent idle past it
/// is left with no park. Removing the cliff needs the poll to re-arm
/// rather than expire, which is its own plan.
pub(crate) const HOOK_TIMEOUT_SECS: u64 = 2_147_483;

/// The per-tool user-scope skill dirs. Grok dedupes its claude-compat
/// scan native-first (grok-first-class P3), so native copies win
/// there; opencode's compat scan wins its dedupe instead, so `clank
/// agent start` launches opencode with that scan disabled
/// (OPENCODE_DISABLE_CLAUDE_CODE_SKILLS=1) and the native copies are
/// what its agents see — every tool ends up on the same per-tool
/// skill model.
pub const TOOL_SKILL_DIRS: [(Tool, &str); 4] = [
    (Tool::Claude, ".claude"),
    (Tool::Codex, ".codex"),
    (Tool::Grok, ".grok"),
    (Tool::OpenCode, ".config/opencode"),
];

/// Every user-scope file clank owns under `$HOME`, as
/// (home-relative path, expected content): the role skills composed
/// per (role, tool), the tool-neutral github skill for
/// every tool dir, and the opencode plugin (binding + work loop;
/// plugins only load from the GLOBAL dir — project-local
/// .opencode/plugin is not scanned, M2 findings). ONE inventory,
/// consumed by both `clank setup` (install) and `clank doctor`
/// (verify), so the two can never diverge (codex 0c90514).
/// One owned user-scope asset: the expected content plus any OTHER
/// canonical content clank itself generated for a different mode —
/// a file exactly matching an alternate is a MODE MIGRATION, not
/// user drift, and installs without --force (codex 1483318: doctor
/// tells the user to run plain `clank setup` on mode drift, so it
/// must actually work).
pub struct OwnedAsset {
    pub rel: String,
    pub expected: String,
    pub canonical_alternates: Vec<String>,
}

pub fn user_asset_inventory(claude_async: bool) -> Vec<OwnedAsset> {
    TOOL_SKILL_DIRS
        .into_iter()
        .flat_map(|(tool, _)| provider_asset_inventory(tool, claude_async))
        .collect()
}

pub(super) fn provider_asset_inventory(tool: Tool, claude_async: bool) -> Vec<OwnedAsset> {
    let tool_dir = TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1;
    let plain = |rel: String, expected: String| OwnedAsset {
        rel,
        expected,
        canonical_alternates: Vec::new(),
    };
    let mut out = Vec::new();
    for (role, skill) in [
        (Role::Master, MASTER_SKILL),
        (Role::Reviewer, REVIEWER_SKILL),
    ] {
        let canonical_alternates = if tool == Tool::Claude {
            vec![compose_skill_with(role, tool, !claude_async)]
        } else {
            Vec::new()
        };
        out.push(OwnedAsset {
            rel: format!("{tool_dir}/skills/{skill}/SKILL.md"),
            expected: compose_skill_with(role, tool, claude_async),
            canonical_alternates,
        });
    }
    out.push(plain(
        format!("{tool_dir}/skills/clank-github/SKILL.md"),
        GITHUB_SKILL_BODY.into(),
    ));
    if tool == Tool::OpenCode {
        out.push(plain(
            format!("{tool_dir}/plugin/clank.js"),
            OPENCODE_PLUGIN.into(),
        ));
    }
    out
}

pub(super) fn resolve_provider_root(
    home: &Path,
    tool: Tool,
    cwd: &Path,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> PathBuf {
    let override_key = match tool {
        Tool::Claude => "CLAUDE_CONFIG_DIR",
        Tool::Codex => "CODEX_HOME",
        Tool::Grok => "GROK_HOME",
        Tool::OpenCode => "OPENCODE_CONFIG_DIR",
    };
    let path = env(override_key)
        .filter(|value| tool != Tool::Claude || !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if tool == Tool::OpenCode {
                env("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"))
                    .join("opencode")
            } else {
                home.join(TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1)
            }
        });
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Read-only path selection shared by launch repair and doctor.
pub(super) fn provider_launch_paths(
    repo: &Path,
    tool: Tool,
    launch: Option<&clank_core::agent_config::LaunchConfig>,
    ambient: &dyn crate::agent_env::Ambient,
) -> anyhow::Result<(PathBuf, PathBuf, Vec<PathBuf>)> {
    let env = |name: &str| {
        launch
            .and_then(|l| l.env.get(name))
            .map(std::ffi::OsString::from)
            .or_else(|| ambient.var_os(name))
    };
    let cwd = if matches!(tool, Tool::Grok | Tool::Claude) {
        repo.to_owned()
    } else {
        ambient
            .cwd()
            .context("provider working directory is unset")?
    };
    let home = env("HOME")
        .map(PathBuf::from)
        .context("provider HOME is unset; cannot locate Clank integration")?;
    let home = if home.is_absolute() {
        home
    } else {
        cwd.join(home)
    };
    let root = resolve_provider_root(&home, tool, &cwd, env);
    let mut roots = vec![root];
    if tool == Tool::OpenCode {
        let global = resolve_provider_root(&home, tool, &cwd, |key| {
            if key == "OPENCODE_CONFIG_DIR" {
                None
            } else {
                env(key)
            }
        });
        match std::fs::symlink_metadata(global.join("plugin/clank.js")) {
            Ok(_) => {
                let same = dunce::canonicalize(&roots[0])
                    .ok()
                    .zip(dunce::canonicalize(&global).ok())
                    .is_some_and(|(selected, global)| selected == global);
                if !same && roots[0] != global {
                    roots.insert(0, global);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((home, cwd, roots))
}

/// Repair only at a real launch boundary. This never starts a provider or
/// consumes a fork intent; failure leaves the caller's launch untouched.
pub(super) async fn ensure_launch_setup(
    repo: &Path,
    tool: Tool,
    launch: Option<&clank_core::agent_config::LaunchConfig>,
) -> anyhow::Result<()> {
    let (home, cwd, roots) = provider_launch_paths(repo, tool, launch, &crate::agent_env::Process)?;
    let root = roots.last().context("provider root is absent")?;
    let claude_async = if tool == Tool::Claude {
        let path = root.join("settings.json");
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let entries = clank_hook_entries(&value, "Stop");
                if entries.is_empty() {
                    probe_launch_claude(launch, &cwd).await?
                } else {
                    entries
                        .iter()
                        .filter_map(|entry| clank_hook_args(entry))
                        .any(|args| {
                            args.windows(2)
                                .any(|pair| pair == ["--loop", "asyncrewake"])
                        })
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                probe_launch_claude(launch, &cwd).await?
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        false
    };
    // Inspect every setup-owned loaded root before repairing any: an old global
    // plugin must join the same duplicate guard, and drift must stay untouched.
    let repairs = roots
        .into_iter()
        .map(|root| inspect_provider(&home, tool, &root, claude_async).map(|ready| (root, ready)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (root, ready) in repairs {
        if ready {
            continue;
        }
        eprintln!(
            "clank: installing missing integration for {} in {}",
            tool.as_str(),
            root.display()
        );
        install_provider(
            &home,
            tool,
            &root,
            claude_async,
            false,
            false,
            &mut Vec::new(),
        )
        .with_context(|| {
            format!(
                "repairing {} integration in {}",
                tool.as_str(),
                root.display()
            )
        })?;
        anyhow::ensure!(
            inspect_provider(&home, tool, &root, claude_async)?,
            "{} integration remains incomplete after setup in {}",
            tool.as_str(),
            root.display()
        );
    }

    Ok(())
}

pub(super) async fn probe_launch_claude(
    launch: Option<&clank_core::agent_config::LaunchConfig>,
    cwd: &Path,
) -> anyhow::Result<bool> {
    let mut command = tokio::process::Command::new(
        launch
            .and_then(|l| l.command.as_deref())
            .unwrap_or("claude"),
    );
    if let Some(launch) = launch {
        command.args(&launch.args).envs(&launch.env);
    }
    command.arg("--version").current_dir(cwd).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
        .await
        .context("selected Claude version probe timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "selected Claude version probe failed"
    );
    Ok(claude_asyncrewake_capable(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub async fn run(args: SetupArgs) -> anyhow::Result<()> {
    let home = home_dir()?;
    let mut summary = Vec::<String>::new();

    // ONE probe drives the skills AND the hook entries — the
    // installed teaching and the installed loop cannot disagree.
    let claude_async = probe_claude_asyncrewake() == Some(true);
    let (grok, grok_repo) = super::grok_setup::installation(&args, &home)?;
    for (tool, _) in TOOL_SKILL_DIRS {
        let root = if tool == Tool::Grok {
            grok.root.clone()
        } else {
            resolve_provider_root(&home, tool, &std::env::current_dir()?, |key| {
                std::env::var_os(key)
            })
        };
        install_provider(
            &home,
            tool,
            &root,
            claude_async,
            args.force,
            args.dry_run,
            &mut summary,
        )?;
    }
    // Native initialize may create provider cache directories. A dry run must
    // not invoke it in the user's home; real setup probes once, never at idle.
    if args.dry_run {
        summary.push("  Grok: capability probe skipped during dry-run; real setup/launch/doctor checks the configured executable".into());
    } else {
        let capabilities = super::grok_setup::probe(&grok, &grok_repo).await;
        summary.push(format!("  Grok: {}", capabilities.description()));
    }
    // Setup may seed product behavior defaults, but never personal/team
    // composition templates. `finish.autosquash` is a behavior default:
    // absent → true; an explicit true/false is preserved.
    if let Some(line) = seed_autosquash_default(&home.join(".clank/config.json"), args.dry_run)? {
        summary.push(line);
    }

    if summary.is_empty() {
        println!("clank setup: nothing to do (all assets already in place)");
    } else {
        for line in summary {
            println!("{line}");
        }
    }
    if args.dry_run {
        println!("(dry-run — no changes written)");
    }
    Ok(())
}

/// One provider's installation, shared by explicit setup and launch repair.
/// Probes are deliberately outside this filesystem transaction.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_provider(
    home: &Path,
    tool: Tool,
    provider_root: &Path,
    claude_async: bool,
    force: bool,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    let prefix = TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1;
    let domain = provider_domain(home, provider_root, tool);
    let default = domain == home;
    let _locks = if dry_run {
        Vec::new()
    } else {
        // Protect both shared manifest and actual provider files. A profile
        // alias must not acquire a different lock for the same hook files.
        let mut roots = vec![domain.to_owned(), provider_root.to_owned()];
        for root in &roots {
            std::fs::create_dir_all(root)?;
        }
        roots = roots
            .into_iter()
            .map(dunce::canonicalize)
            .collect::<Result<Vec<_>, _>>()?;
        roots.sort();
        roots.dedup();
        roots
            .iter()
            .map(|root| installation_lock(root))
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let mut manifest = load_manifest(domain);
    for mut asset in provider_asset_inventory(tool, claude_async) {
        if !default {
            asset.rel = asset
                .rel
                .strip_prefix(&format!("{prefix}/"))
                .unwrap()
                .to_owned();
        }
        install_skill(
            domain,
            &asset.rel,
            &asset.expected,
            &asset.canonical_alternates,
            force,
            dry_run,
            &mut manifest,
            summary,
        )?;
    }
    for retired in owned_retired_assets(home, tool, provider_root, &manifest)? {
        if !dry_run {
            remove_retired_asset(domain, &mut manifest, &retired)?;
        }
        summary.push(format!(
            "  remove {} (no longer shipped)",
            retired.path.display()
        ));
    }
    remove_obsolete_skill(
        &provider_root.join(format!("skills/{OBSOLETE_SKILL}")),
        dry_run,
        summary,
    )?;
    match tool {
        Tool::Claude => install_claude_hooks(
            &provider_root.join("settings.json"),
            claude_async,
            dry_run,
            summary,
        )?,
        Tool::Codex => {
            install_codex_hooks(&provider_root.join("hooks.json"), dry_run, summary)?;
            install_codex_rule(&provider_root.join("rules/default.rules"), dry_run, summary)?;
        }
        Tool::Grok => {
            install_grok_hooks(&provider_root.join("hooks/clank.json"), dry_run, summary)?
        }
        Tool::OpenCode => {}
    }
    Ok(())
}

pub(super) fn provider_domain<'a>(home: &'a Path, root: &'a Path, tool: Tool) -> &'a Path {
    let prefix = TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1;
    let default = home.join(prefix);
    if root == default
        || dunce::canonicalize(root)
            .ok()
            .is_some_and(|actual| dunce::canonicalize(default).ok().as_ref() == Some(&actual))
    {
        home
    } else {
        root
    }
}

pub(super) fn provider_asset_location(
    home: &Path,
    root: &Path,
    tool: Tool,
    mut asset: OwnedAsset,
) -> (PathBuf, OwnedAsset) {
    let domain = provider_domain(home, root, tool);
    if domain != home {
        let prefix = TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1;
        asset.rel = asset
            .rel
            .strip_prefix(&format!("{prefix}/"))
            .expect("selected provider asset")
            .to_owned();
    }
    (domain.to_owned(), asset)
}

/// Read-only readiness. Missing/known old setup means repair; invalid or
/// user-drifted assets are errors, never silently reclassified as missing.
pub(super) fn inspect_provider(
    home: &Path,
    tool: Tool,
    root: &Path,
    claude_async: bool,
) -> anyhow::Result<bool> {
    let domain = provider_domain(home, root, tool);
    let manifest = load_manifest(domain);
    let mut ready = true;
    for asset in provider_asset_inventory(tool, claude_async) {
        let (domain, asset) = provider_asset_location(home, root, tool, asset);
        let path = domain.join(&asset.rel);
        match std::fs::read_to_string(&path) {
            Ok(content) => match classify_asset_state(
                &content,
                &asset.expected,
                &asset.canonical_alternates,
                manifest.get(&asset.rel).map(String::as_str),
            ) {
                AssetState::Current => {}
                AssetState::ModeAlternate | AssetState::OldSetup => ready = false,
                AssetState::Drift => anyhow::bail!(
                    "{} contains local changes; automatic setup will not overwrite them",
                    path.display()
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ready = false,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading setup asset {}", path.display()));
            }
        }
    }
    if !owned_retired_assets(home, tool, root, &manifest)?.is_empty() {
        ready = false;
    }
    match std::fs::metadata(root.join(format!("skills/{OBSOLETE_SKILL}/SKILL.md"))) {
        Ok(_) => ready = false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let (file, kinds): (&str, Vec<Box<dyn HookKind>>) = match tool {
        Tool::Claude => (
            "settings.json",
            vec![
                Box::new(ClaudeHook {
                    async_mode: claude_async,
                }),
                Box::new(SessionStartHook),
                Box::new(ClaudeQuestionHook("PreToolUse")),
                Box::new(ClaudeQuestionHook("PermissionRequest")),
                Box::new(ClaudeQuestionHook("PostToolUse")),
                Box::new(ClaudeQuestionHook("PostToolUseFailure")),
            ],
        ),
        Tool::Codex => (
            "hooks.json",
            vec![Box::new(CodexHook), Box::new(CodexSessionStartHook)],
        ),
        Tool::Grok => (
            "hooks/clank.json",
            super::grok_setup::EVENTS
                .into_iter()
                .map(|event| Box::new(GrokHook { event }) as Box<dyn HookKind>)
                .collect(),
        ),
        Tool::OpenCode => return Ok(ready),
    };
    let path = root.join(file);
    let mut value: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .with_context(|| format!("parsing setup hooks {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error.into()),
    };
    let before = value.clone();
    if tool == Tool::Grok {
        retire_grok_hooks(&mut value)?;
    }
    for kind in kinds {
        merge_hook_value(&mut value, &path, kind.as_ref())?;
    }
    ready &= before == value;
    if tool == Tool::Codex {
        let path = root.join("rules/default.rules");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        ready &= codex_rule_ready(&path, content.as_deref())?;
    }
    Ok(ready)
}

/// The manifest domain is also the lock domain: default providers share the
/// user manifest; custom homes have their own. Never unlink a locked inode.
pub(super) fn installation_lock(domain: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = domain.join(".clank");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("setup-install.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening setup lock {}", path.display()))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "setup lock is not a regular file"
    );
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("locking setup installation");
        }
    }
}

/// Seed `finish.autosquash=true` into `user_config` on first setup. Returns
/// the summary line when it seeds (or would, under `dry_run`); `None` when the
/// key is already set (explicit true/false is preserved) or on a dry-run of an
/// already-set key. Split out for testing without `$HOME` mutation.
fn seed_autosquash_default(user_config: &Path, dry_run: bool) -> anyhow::Result<Option<String>> {
    let key = &["finish", "autosquash"][..];
    // A destructive default must disclose itself at the surface, not just in a
    // code comment (ruthless 432a82e): say plainly that finish now rewrites the
    // current branch in place, the published-history risk, and the opt-outs.
    let notice = |verb: &str| {
        format!(
            "{verb} finish.autosquash=true (default): `clank finish` now collapses each plan \
             by REWRITING the current branch in place — on an already-pushed branch this \
             rewrites published history. Disable with `clank config finish.autosquash false`, \
             or skip one finish with `--no-squash`."
        )
    };
    if dry_run {
        Ok(
            (!crate::cli::config::json_path_present(user_config, key))
                .then(|| notice("would seed")),
        )
    } else if crate::cli::config::set_key_if_absent(user_config, key, "true", "bool")? {
        Ok(Some(notice("seeded")))
    } else {
        Ok(None)
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME env var is unset; cannot locate user config dirs"))
}

/// Provenance for setup-owned files: each installed relative path
/// mapped to the blake3 of the content setup last wrote there. This
/// is what makes "old setup" a defined state — without it an older
/// setup-written version is indistinguishable from user drift, and
/// every asset upgrade reads as foreign (setup-overwrites-its-own-old-versions).
pub(crate) type SetupManifest = std::collections::BTreeMap<String, String>;

fn manifest_path(home: &Path) -> PathBuf {
    home.join(".clank/setup-manifest.json")
}

/// Missing OR CORRUPT reads as empty: an unknown file then follows
/// the refuse path, which is the safe direction — never a silent
/// overwrite on a guess.
pub(crate) fn load_manifest(home: &Path) -> SetupManifest {
    std::fs::read_to_string(manifest_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_manifest(home: &Path, manifest: &SetupManifest) -> anyhow::Result<()> {
    let path = manifest_path(home);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    tmp.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(&path).map_err(|e| anyhow::anyhow!(e.error))?;
    Ok(())
}

pub(crate) fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

/// What a setup-owned file on disk IS, classified once so setup's
/// install ladder and doctor's report can never diverge on what
/// "old setup" means — the same parity rule the asset inventory
/// carries (codex 0c90514), now over provenance too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetState {
    Current,
    ModeAlternate,
    /// Matches the manifest's recorded hash: a setup-written OLDER
    /// version. Setup overwrites without `--force`.
    OldSetup,
    /// Nothing recognises it: user drift. Setup refuses without
    /// `--force`.
    Drift,
}

pub(crate) fn classify_asset_state(
    existing: &str,
    expected: &str,
    canonical_alternates: &[String],
    recorded_hash: Option<&str>,
) -> AssetState {
    if existing == expected {
        AssetState::Current
    } else if canonical_alternates.iter().any(|a| a == existing) {
        AssetState::ModeAlternate
    } else if recorded_hash.is_some_and(|h| h == content_hash(existing)) {
        AssetState::OldSetup
    } else {
        AssetState::Drift
    }
}

/// Write a skill/command file the binary owns outright. Behavior:
/// - missing → write
/// - matches → no-op (record up-to-date)
/// - canonical alternate → mode migration
/// - matches the manifest's recorded hash → a known setup-written
///   OLDER version: upgrade WITHOUT `--force`
/// - drifted (no provenance) → refuse unless `--force`
/// Every successful write records its hash, so the NEXT upgrade is
/// silent. Pre-manifest files get one honest `--force` bridge:
/// arbitrary existing content is never blessed into the manifest.
#[allow(clippy::too_many_arguments)]
fn install_skill(
    home: &Path,
    rel: &str,
    expected: &str,
    canonical_alternates: &[String],
    force: bool,
    dry_run: bool,
    manifest: &mut SetupManifest,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    let path = home.join(rel);
    match std::fs::read_to_string(&path) {
        Ok(existing) => {
            let state = classify_asset_state(
                &existing,
                expected,
                canonical_alternates,
                manifest.get(rel).map(String::as_str),
            );
            match state {
                AssetState::Current => {
                    record_setup_write(home, manifest, rel, expected, dry_run)?;
                    summary.push(format!("  ok    {}", path.display()));
                }
                // OURS in a different mode — migrate freely.
                AssetState::ModeAlternate => {
                    if !dry_run {
                        publish_asset(&path, expected.as_bytes())
                            .with_context(|| format!("writing `{}`", path.display()))?;
                    }
                    record_setup_write(home, manifest, rel, expected, dry_run)?;
                    summary.push(format!("  mode  {}", path.display()));
                }
                // OURS from an older version — plain setup upgrades,
                // and the new hash goes on record for the next one.
                AssetState::OldSetup if !force => {
                    if !dry_run {
                        publish_asset(&path, expected.as_bytes())
                            .with_context(|| format!("writing `{}`", path.display()))?;
                    }
                    record_setup_write(home, manifest, rel, expected, dry_run)?;
                    summary.push(format!("  upgrade {}", path.display()));
                }
                _ if force => {
                    if !dry_run {
                        publish_asset(&path, expected.as_bytes())
                            .with_context(|| format!("writing `{}`", path.display()))?;
                    }
                    record_setup_write(home, manifest, rel, expected, dry_run)?;
                    summary.push(format!("  force {}", path.display()));
                }
                AssetState::Drift => {
                    anyhow::bail!(
                        "{} exists with different content; pass --force to overwrite.",
                        path.display()
                    );
                }
                AssetState::OldSetup => unreachable!("OldSetup without force upgrades above"),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !dry_run {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating `{}`", parent.display()))?;
                }
                publish_asset(&path, expected.as_bytes())
                    .with_context(|| format!("writing `{}`", path.display()))?;
            }
            record_setup_write(home, manifest, rel, expected, dry_run)?;
            summary.push(format!("  write {}", path.display()));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Atomic publication keeps a killed setup from leaving a partial asset.
pub(super) fn publish_asset(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    // Match the old write-through behavior for user-config symlinks rather
    // than replacing the link itself with an unrelated regular file.
    let target = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => dunce::canonicalize(path)?,
        Ok(_) => path.to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
        Err(error) => return Err(error.into()),
    };
    let path = target.as_path();
    let parent = path.parent().context("setup asset has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    match std::fs::metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.permissions().readonly(),
                "setup asset {} is read-only",
                path.display()
            );
            temp.as_file().set_permissions(metadata.permissions())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    temp.write_all(content)?;
    temp.as_file_mut().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publishing setup asset {}", path.display()))?;
    Ok(())
}

/// One recorded write: the hash of every content setup lands goes
/// into the manifest — `ok` included, so a matching-but-unrecorded
/// file becomes provenance for the next upgrade.
fn record_setup_write(
    home: &Path,
    manifest: &mut SetupManifest,
    rel: &str,
    content: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    if !dry_run {
        manifest.insert(rel.to_string(), content_hash(content));
        save_manifest(home, manifest)?;
    }
    Ok(())
}

/// A retired asset whose bytes are still the ones setup recorded
/// writing. Only these are setup's to delete: an edited copy, or one
/// the manifest never recorded, is the user's.
#[derive(Debug)]
struct RetiredAsset {
    path: PathBuf,
    rel: String,
}

fn owned_retired_assets(
    home: &Path,
    tool: Tool,
    root: &Path,
    manifest: &SetupManifest,
) -> anyhow::Result<Vec<RetiredAsset>> {
    let prefix = TOOL_SKILL_DIRS.iter().find(|(t, _)| *t == tool).unwrap().1;
    let domain = provider_domain(home, root, tool);
    let mut owned = Vec::new();
    for retired in RETIRED_ASSETS {
        let rel = if domain == home {
            format!("{prefix}/{retired}")
        } else {
            (*retired).to_owned()
        };
        let path = domain.join(&rel);
        let content = match std::fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading retired asset {}", path.display()));
            }
        };
        let recorded = manifest.get(&rel).map(String::as_str);
        if recorded == Some(blake3::hash(&content).to_hex().as_str()) {
            owned.push(RetiredAsset { path, rel });
        }
    }
    Ok(owned)
}

fn remove_retired_asset(
    domain: &Path,
    manifest: &mut SetupManifest,
    retired: &RetiredAsset,
) -> anyhow::Result<()> {
    std::fs::remove_file(&retired.path)
        .with_context(|| format!("removing retired asset {}", retired.path.display()))?;
    manifest.remove(&retired.rel);
    save_manifest(domain, manifest)?;
    let dir = retired
        .path
        .parent()
        .context("retired asset has no parent")?;
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("removing retired skill dir {}", dir.display()))
        }
    }
}

/// Remove a clank-owned skill dir that the binary no longer ships
/// (the pre-split single `clank` skill). The dir is clank-owned, so we
/// remove it outright when its `SKILL.md` is present; a no-op when
/// already gone. Records the action in the summary.
fn remove_obsolete_skill(
    dir: &Path,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    if dir.join("SKILL.md").exists() {
        if !dry_run {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("removing obsolete skill `{}`", dir.display()))?;
        }
        summary.push(format!(
            "  remove {} (split into clank-master / clank-reviewer)",
            dir.display()
        ));
    }
    Ok(())
}

/// The line we ensure is present in codex's command-rules file.
/// Bare prefix `["clank"]` whitelists every `clank <subcommand>`
/// without enumerating each one — the gate state machine itself
/// is what enforces "did this agent have the right to do that."
///
/// Format is codex's `prefix_rule(pattern=[...], decision="allow|deny")`
/// DSL.
const CODEX_RULE_LINE: &str = r#"prefix_rule(pattern=["clank"], decision="allow")"#;

/// Substring used to identify ANY rule whose pattern is the
/// single-element list `["clank"]`. The closing `]` immediately
/// after `"clank"` distinguishes the bare pattern from longer
/// patterns like `["clank", "init"]` — those contain `, "init"]`
/// between the `"` and the `]` so this substring is absent.
const CODEX_BARE_CLANK_PATTERN: &str = r#"pattern=["clank"]"#;

/// Minimal structural validation of a `prefix_rule(` line. The
/// file is the user's; we don't try to be a full DSL parser.
/// What we DO catch: lines that announce themselves as
/// `prefix_rule(` calls but are missing the closing `)`, missing
/// the `pattern=[...]` argument, or missing an
/// `decision="allow|deny"` argument. Each of these would leave
/// codex with a broken rules file after our append.
fn validate_prefix_rule_line(path: &Path, lineno: usize, trimmed: &str) -> anyhow::Result<()> {
    let malformed = |reason: &str| -> anyhow::Error {
        anyhow::anyhow!(
            "{}:{}: line `{}` is malformed ({}); fix or remove it, then re-run `clank setup`",
            path.display(),
            lineno,
            trimmed,
            reason
        )
    };
    // Must close the call. Strip a trailing comment if codex
    // ever adds support; for now we trust line.trim().
    if !trimmed.ends_with(')') {
        return Err(malformed("missing closing `)`"));
    }
    if !trimmed.contains("pattern=[") {
        return Err(malformed("missing `pattern=[...]` argument"));
    }
    if !trimmed.contains(']') {
        return Err(malformed("`pattern=[...]` is missing its closing `]`"));
    }
    let has_allow = trimmed.contains(r#"decision="allow""#);
    let has_deny = trimmed.contains(r#"decision="deny""#);
    if !has_allow && !has_deny {
        return Err(malformed(
            r#"missing `decision="allow"` or `decision="deny"` argument"#,
        ));
    }
    Ok(())
}

fn codex_rule_ready(path: &Path, content: Option<&str>) -> anyhow::Result<bool> {
    let Some(content) = content else {
        return Ok(false);
    };
    let mut existing_clank_allow_seen = false;
    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = line.trim();
        if !trimmed.starts_with("prefix_rule(") {
            continue;
        }
        // Any `prefix_rule(` line must be well-formed. The
        // user's rules file is their space; if it's already
        // broken, appending our line wouldn't fix it AND would
        // leave us as the most recent edit on a broken file.
        // Surface the diagnostic; let the user fix it manually.
        validate_prefix_rule_line(path, lineno, trimmed)?;

        if !trimmed.contains(CODEX_BARE_CLANK_PATTERN) {
            continue;
        }
        if trimmed.contains(r#"decision="deny""#) {
            anyhow::bail!(
                "{}:{}: line `{}` denies the `clank` pattern; \
                     remove or change it before re-running `clank setup`",
                path.display(),
                lineno,
                trimmed,
            );
        }
        if trimmed.contains(r#"decision="allow""#) {
            existing_clank_allow_seen = true;
        }
    }
    Ok(existing_clank_allow_seen)
}

/// Ensure the codex command-rules file contains a bare `clank`
/// allow rule.
///
/// Idempotent: if a matching `decision="allow"` rule already
/// exists, no-op.
///
/// Fail-closed: if a matching `decision="deny"` rule exists, the
/// user has explicitly denied the pattern and we error out
/// naming the offending line. Don't silently override.
///
/// Honors `--dry-run`: prints the intended action without
/// touching the filesystem.
fn install_codex_rule(path: &Path, dry_run: bool, summary: &mut Vec<String>) -> anyhow::Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", path.display())));
        }
    };

    if codex_rule_ready(path, existing.as_deref())? {
        summary.push(format!("  ok    {}", path.display()));
        return Ok(());
    }

    if !dry_run {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating `{}`", parent.display()))?;
        }
        let mut new_content = existing.unwrap_or_default();
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(CODEX_RULE_LINE);
        new_content.push('\n');
        publish_asset(path, new_content.as_bytes())
            .with_context(|| format!("writing `{}`", path.display()))?;
    }
    summary.push(format!(
        "  write {} (append `clank` allow rule)",
        path.display()
    ));
    Ok(())
}

/// One clank hook entry, merged into the user's `settings.json` or
/// `hooks.json` (typed-json-not-json-macro). Only the fragment WE
/// contribute is typed; the surrounding read-modify-write stays
/// `Value`-level since the file is user-owned.
#[derive(serde::Serialize)]
struct HookEntry<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    command: &'a str,
    timeout: u64,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    status_message: Option<&'a str>,
    /// claude-asyncrewake-work-loop: the async park entry. Absent
    /// (not `false`) for every legacy/other-tool entry so their JSON
    /// is byte-stable.
    #[serde(rename = "asyncRewake", skip_serializing_if = "std::ops::Not::not")]
    async_rewake: bool,
}

#[derive(serde::Serialize)]
struct HookWrapper<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<&'a str>,
    hooks: [HookEntry<'a>; 1],
}

/// Tool-specific knowledge for the hook merger.
trait HookKind {
    fn owns(&self, entry: &serde_json::Value) -> bool {
        hook_is_clank(entry)
    }
    /// What goes into the `command` field of the JSON entry.
    fn command(&self) -> &'static str;
    /// Tool name for the diagnostic summary.
    fn tool_name(&self) -> &'static str;
    /// Whether this tool's hook entry supports `statusMessage`
    /// (visible while the hook runs).
    fn status_message(&self) -> Option<&'static str> {
        None
    }
    /// Which hook event array the entry merges into.
    fn event(&self) -> &'static str {
        "Stop"
    }
    /// claude-asyncrewake-work-loop: park-mode entry.
    fn async_rewake(&self) -> bool {
        false
    }
    fn timeout(&self) -> u64 {
        HOOK_TIMEOUT_SECS
    }
    fn matcher(&self) -> Option<&'static str> {
        None
    }
}

/// The claude Stop entry. The delivery mode is ONE durable
/// setup-time decision (claude-asyncrewake-work-loop): the command
/// carries `--loop asyncrewake` iff the installed Claude Code can
/// honor `asyncRewake`, and the hook obeys its own argv — the
/// static entry and the runtime behavior cannot drift.
struct ClaudeHook {
    async_mode: bool,
}
impl HookKind for ClaudeHook {
    fn command(&self) -> &'static str {
        if self.async_mode {
            "clank stop-hook --tool claude --loop asyncrewake"
        } else {
            "clank stop-hook --tool claude"
        }
    }
    fn tool_name(&self) -> &'static str {
        "claude"
    }
    fn status_message(&self) -> Option<&'static str> {
        // Visible while the park holds (asyncRewake mode only —
        // legacy claude doesn't show statusMessage).
        self.async_mode.then_some("Clank: watching for work")
    }
    fn async_rewake(&self) -> bool {
        self.async_mode
    }
}

/// Every entry claude's `settings.json` gets: the Stop hook in the
/// delivery mode the probe decided, and the SessionStart companion
/// in EVERY mode — it binds the session clank launched
/// (a-session-binds-itself-at-start), which no delivery mode can do
/// without; it used to ride with asyncrewake alone, and a legacy
/// claude then had its bind prompt taken away with nothing writing
/// the id (codex on acdfb0e).
fn install_claude_hooks(
    path: &Path,
    async_mode: bool,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    merge_hook_into_settings(path, ClaudeHook { async_mode }, dry_run, summary)?;
    merge_hook_into_settings(path, SessionStartHook, dry_run, summary)?;
    for event in [
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolUseFailure",
    ] {
        merge_hook_into_settings(path, ClaudeQuestionHook(event), dry_run, summary)?;
    }
    Ok(())
}
struct ClaudeQuestionHook(&'static str);
impl HookKind for ClaudeQuestionHook {
    fn owns(&self, entry: &serde_json::Value) -> bool {
        entry.get("command").and_then(|v|v.as_str()).and_then(|s|shell_words::split(s).ok()).is_some_and(|words|matches!(words.as_slice(),[program,command,..] if Path::new(program).file_name()==Some(std::ffi::OsStr::new("clank")) && command=="question-hook"))
    }
    fn command(&self) -> &'static str {
        "clank question-hook"
    }
    fn tool_name(&self) -> &'static str {
        "claude"
    }
    fn event(&self) -> &'static str {
        self.0
    }
    fn matcher(&self) -> Option<&'static str> {
        Some("AskUserQuestion")
    }
}

/// SessionStart companion: binds the launched session to its label,
/// delivers catch-up context. Fast — it
/// must never hold a session start hostage.
struct SessionStartHook;
impl HookKind for SessionStartHook {
    fn command(&self) -> &'static str {
        "clank stop-hook --tool claude --session-start"
    }
    fn tool_name(&self) -> &'static str {
        "claude"
    }
    fn event(&self) -> &'static str {
        "SessionStart"
    }
    fn timeout(&self) -> u64 {
        15
    }
}

/// Pure capability gate: 2.1.223 is the M0-verified floor for
/// `asyncRewake`. Parses the leading x.y.z of `claude --version`.
pub(crate) fn claude_asyncrewake_capable(version_output: &str) -> bool {
    let mut nums = version_output
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let v = (
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
        nums.next().unwrap_or(0),
    );
    v >= (2, 1, 223)
}

/// Probe the installed Claude Code. `None` = no binary / unreadable
/// version — setup stays on the legacy loop (conservative).
pub(crate) fn probe_claude_asyncrewake() -> Option<bool> {
    let out = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| claude_asyncrewake_capable(&String::from_utf8_lossy(&out.stdout)))
}

struct CodexHook;
impl HookKind for CodexHook {
    fn command(&self) -> &'static str {
        "clank stop-hook --tool codex"
    }
    fn tool_name(&self) -> &'static str {
        "codex"
    }
    fn status_message(&self) -> Option<&'static str> {
        Some("Clank: checking for pending review work")
    }
}

/// Every entry codex's `hooks.json` gets: the Stop hook that drives
/// the work loop and the SessionStart companion that binds the
/// session. One function, so `run` cannot install one without the
/// other.
fn install_codex_hooks(
    path: &Path,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    merge_hook_into_settings(path, CodexHook, dry_run, summary)?;
    merge_hook_into_settings(path, CodexSessionStartHook, dry_run, summary)
}

/// Codex's SessionStart companion (a-session-binds-itself-at-start):
/// binds the session clank launched to its label, and rebinds when
/// `/clear` hands the process a new id — the same wire as claude's,
/// which codex 0.153 emits with the same fields. Unconditional, unlike
/// claude's: it does not depend on a delivery mode.
struct CodexSessionStartHook;
impl HookKind for CodexSessionStartHook {
    fn command(&self) -> &'static str {
        "clank stop-hook --tool codex --session-start"
    }
    fn tool_name(&self) -> &'static str {
        "codex"
    }
    fn event(&self) -> &'static str {
        "SessionStart"
    }
    fn timeout(&self) -> u64 {
        15
    }
}

/// Merge `kind`'s entry into the user's hook-config JSON: every
/// clank entry under its event is taken out of its wrapper and one
/// fresh entry is appended. A wrapper keeps its other entries, fields
/// and position, and goes only when clank's removal empties it — a
/// wrapper can hold a user's hook beside ours.
fn merge_hook_into_settings(
    path: &Path,
    kind: impl HookKind,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut value: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("parsing `{}` as JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => return Err(e.into()),
    };

    let removed = merge_hook_value(&mut value, path, &kind)?;
    let event = kind.event();

    if !dry_run {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating `{}`", parent.display()))?;
        }
        let serialized = serde_json::to_string_pretty(&value)?;
        publish_asset(path, format!("{serialized}\n").as_bytes())
            .with_context(|| format!("writing `{}`", path.display()))?;
    }

    let action = if removed > 0 { "merge" } else { "add  " };
    summary.push(format!(
        "  {action} {path} ({tool} {event} hook)",
        path = path.display(),
        tool = kind.tool_name(),
    ));
    Ok(())
}

/// Apply one owned fragment in memory, shared by legacy and native files.
fn merge_hook_value(
    value: &mut serde_json::Value,
    path: &Path,
    kind: &(impl HookKind + ?Sized),
) -> anyhow::Result<usize> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` is not a JSON object"))?;
    let event = kind.event();
    let wrappers = hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let wrappers = wrappers
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks.{event}` is not a JSON array"))?;

    let mut removed = 0;
    wrappers.retain_mut(|wrapper| {
        let Some(inner) = wrapper.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return true;
        };
        let before = inner.len();
        inner.retain(|h| !kind.owns(h));
        let taken = before - inner.len();
        removed += taken;
        taken == 0 || !inner.is_empty()
    });

    let entry = HookEntry {
        kind: "command",
        command: kind.command(),
        timeout: kind.timeout(),
        status_message: kind.status_message(),
        async_rewake: kind.async_rewake(),
    };
    wrappers.push(serde_json::to_value(HookWrapper {
        matcher: kind.matcher(),
        hooks: [entry],
    })?);

    Ok(removed)
}

struct GrokHook {
    event: &'static str,
}
impl HookKind for GrokHook {
    fn command(&self) -> &'static str {
        super::grok_setup::COMMAND
    }
    fn tool_name(&self) -> &'static str {
        "grok"
    }
    fn event(&self) -> &'static str {
        self.event
    }
    fn timeout(&self) -> u64 {
        5
    }
}

/// Publish the complete native adapter atomically; a partial set of guards is
/// never a successful installation. Unrelated hooks and fields are preserved.
pub(super) fn install_grok_hooks(
    path: &Path,
    dry_run: bool,
    summary: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut value = match super::grok_setup::read_hooks(path) {
        Ok(v) => v,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => return Err(e),
    };
    let before = value.clone();
    retire_grok_hooks(&mut value)?;
    for event in super::grok_setup::EVENTS {
        merge_hook_value(&mut value, path, &GrokHook { event })?;
    }
    if value == before {
        summary.push(format!("  ok    {} (native Grok adapter)", path.display()));
        return Ok(());
    }
    if !dry_run {
        use std::io::Write;
        let parent = path
            .parent()
            .context("native Grok hook path needs a parent")?;
        std::fs::create_dir_all(parent)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        writeln!(tmp, "{}", serde_json::to_string_pretty(&value)?)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(path).map_err(|e| anyhow::anyhow!(e.error))?;
    }
    summary.push(format!(
        "  write {} (complete native Grok adapter)",
        path.display()
    ));
    Ok(())
}

/// One upgrade transformation, used by readiness and installation alike.
fn retire_grok_hooks(value: &mut serde_json::Value) -> anyhow::Result<()> {
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        for event in super::grok_setup::RETIRED_EVENTS {
            if let Some(groups) = hooks.get_mut(event) {
                let groups = groups
                    .as_array_mut()
                    .context("retired Grok hook groups must be arrays")?;
                groups.retain_mut(|group| {
                    let Some(entries) = group
                        .get_mut("hooks")
                        .and_then(serde_json::Value::as_array_mut)
                    else {
                        return true;
                    };
                    let before = entries.len();
                    entries.retain(|entry| !hook_is_clank(entry));
                    before == entries.len() || !entries.is_empty()
                });
                if groups.is_empty() {
                    hooks.remove(event);
                }
            }
        }
    }
    Ok(())
}

/// Whether one inner hook entry is clank's; see [`clank_hook_args`].
pub(crate) fn hook_is_clank(entry: &serde_json::Value) -> bool {
    clank_hook_args(entry).is_some() || ClaudeQuestionHook("PreToolUse").owns(entry)
}

/// The words an inner hook entry's `command` passes to
/// `clank stop-hook`, or `None` when the entry is not clank's. The
/// command is split as a shell would split it, so the binary may be
/// named by bare name or by path, quoted or not; a command whose
/// quoting doesn't parse is not ours. A compound command
/// (`clank stop-hook …; /my/notify`) is owned whole, so setup replaces
/// it tail and all.
///
/// Keyed on the command because it is the one field a host cannot
/// drop. A host may rewrite its config against its own schema and
/// lose fields it doesn't know: claude's `settings.json` turned up
/// with every clank `id` marker gone, stranding the entries so setup
/// piled up duplicates (a-hook-is-known-by-what-it-runs). Every entry
/// Stop entries run `clank stop-hook`; the question adapter has its own
/// ownership predicate. This function returns Stop adapter argv only.
pub(crate) fn clank_hook_args(entry: &serde_json::Value) -> Option<Vec<String>> {
    let command = entry.get("command")?.as_str()?;
    let words = shell_words::split(command).ok()?;
    match words.as_slice() {
        [program, subcommand, args @ ..]
            if Path::new(program).file_name() == Some(std::ffi::OsStr::new("clank"))
                && subcommand == "stop-hook" =>
        {
            Some(args.to_vec())
        }
        _ => None,
    }
}

/// Every clank-owned inner entry under `event` in a parsed hook
/// config, in file order.
pub(crate) fn clank_hook_entries<'a>(
    settings: &'a serde_json::Value,
    event: &str,
) -> Vec<&'a serde_json::Value> {
    settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|e| e.as_array())
        .into_iter()
        .flatten()
        .filter_map(|wrapper| wrapper.get("hooks").and_then(|h| h.as_array()))
        .flatten()
        .filter(|h| hook_is_clank(h))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_inspection_is_read_only_and_shares_installer_hook_and_rule_definitions() {
        for (tool, prefix) in TOOL_SKILL_DIRS {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = home.join(prefix);
            assert!(!inspect_provider(&home, tool, &root, false).unwrap());
            assert!(!home.exists());
            install_provider(&home, tool, &root, false, false, false, &mut vec![]).unwrap();
            let path = manifest_path(&home);
            let before = std::fs::metadata(&path).unwrap().modified().unwrap();
            assert!(inspect_provider(&home, tool, &root, false).unwrap());
            assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), before);
            let first = root.join("skills/clank-master/SKILL.md");
            std::fs::remove_file(&first).unwrap();
            assert!(!inspect_provider(&home, tool, &root, false).unwrap());
            std::fs::write(&first, "local changes").unwrap();
            assert!(inspect_provider(&home, tool, &root, false).is_err());
        }
    }

    #[test]
    fn inspection_refuses_invalid_hooks_and_explicit_codex_rule_denial_before_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".codex");
        install_provider(
            temp.path(),
            Tool::Codex,
            &root,
            false,
            false,
            false,
            &mut vec![],
        )
        .unwrap();
        std::fs::write(
            root.join("rules/default.rules"),
            "prefix_rule(pattern=[\"clank\"], decision=\"deny\")\n",
        )
        .unwrap();
        assert!(inspect_provider(temp.path(), Tool::Codex, &root, false).is_err());
        std::fs::write(root.join("hooks.json"), "broken").unwrap();
        assert!(inspect_provider(temp.path(), Tool::Codex, &root, false).is_err());
    }

    #[test]
    fn profile_roots_follow_selected_provider_overrides_and_relative_launch_cwd() {
        let home = Path::new("/fixture/home");
        let cwd = Path::new("/fixture/cwd");
        for (tool, key) in [
            (Tool::Claude, "CLAUDE_CONFIG_DIR"),
            (Tool::Codex, "CODEX_HOME"),
            (Tool::Grok, "GROK_HOME"),
            (Tool::OpenCode, "OPENCODE_CONFIG_DIR"),
        ] {
            assert_eq!(
                resolve_provider_root(home, tool, cwd, |name| (name == key)
                    .then(|| "profile".into())),
                cwd.join("profile")
            );
        }
        assert_eq!(
            resolve_provider_root(home, Tool::OpenCode, cwd, |name| (name
                == "XDG_CONFIG_HOME")
                .then(|| "/fixture/xdg".into())),
            Path::new("/fixture/xdg/opencode")
        );
    }

    #[test]
    fn claude_setup_relative_paths_use_the_selected_repo_and_empty_override_defaults() {
        let repo = Path::new("/selected/repo");
        let ambient = crate::agent_env::Stated::saying(&[
            ("HOME", "relative-home"),
            ("CLAUDE_CONFIG_DIR", "relative-profile"),
        ])
        .at(Path::new("/caller/home"), Path::new("/caller/cwd"));
        let (home, cwd, roots) = provider_launch_paths(repo, Tool::Claude, None, &ambient).unwrap();
        assert_eq!(home, repo.join("relative-home"));
        assert_eq!(cwd, repo);
        assert_eq!(roots, [repo.join("relative-profile")]);
        let launch = clank_core::agent_config::LaunchConfig {
            env: std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".into(), "".into())]),
            ..Default::default()
        };
        assert_eq!(
            provider_launch_paths(repo, Tool::Claude, Some(&launch), &ambient)
                .unwrap()
                .2,
            [repo.join("relative-home/.claude")]
        );
    }

    #[tokio::test]
    async fn launch_setup_repairs_selected_custom_home_and_healthy_launch_does_not_write() {
        for (tool, key) in [
            (Tool::Codex, "CODEX_HOME"),
            (Tool::Grok, "GROK_HOME"),
            (Tool::OpenCode, "OPENCODE_CONFIG_DIR"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("profile");
            let mut launch = clank_core::agent_config::LaunchConfig::default();
            launch
                .env
                .insert("HOME".into(), home.to_string_lossy().into());
            launch.env.insert(key.into(), root.to_string_lossy().into());
            ensure_launch_setup(temp.path(), tool, Some(&launch))
                .await
                .unwrap();
            assert!(!home.exists(), "selected profile only");
            let manifest = manifest_path(&root);
            let modified = std::fs::metadata(&manifest).unwrap().modified().unwrap();
            ensure_launch_setup(temp.path(), tool, Some(&launch))
                .await
                .unwrap();
            assert_eq!(
                std::fs::metadata(manifest).unwrap().modified().unwrap(),
                modified
            );
            std::fs::write(root.join("skills/clank-master/SKILL.md"), "user edit").unwrap();
            std::fs::remove_file(root.join("skills/clank-reviewer/SKILL.md")).unwrap();
            assert!(
                ensure_launch_setup(temp.path(), tool, Some(&launch))
                    .await
                    .is_err()
            );
            assert!(
                !root.join("skills/clank-reviewer/SKILL.md").exists(),
                "invalid inspection must precede repair"
            );
        }
    }

    #[tokio::test]
    async fn claude_launch_setup_probes_selected_executable_once_before_repair() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profile");
        let program = temp.path().join("claude-fixture");
        let calls = temp.path().join("calls");
        std::fs::write(
            &program,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALLS\"\nprintf '2.1.0 (Claude Code)\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut launch = clank_core::agent_config::LaunchConfig::default();
        launch.command = Some(program.to_string_lossy().into());
        launch.args = vec!["--profile".into(), "test".into()];
        launch.env.insert(
            "HOME".into(),
            temp.path().join("home").to_string_lossy().into(),
        );
        launch
            .env
            .insert("CLAUDE_CONFIG_DIR".into(), root.to_string_lossy().into());
        launch
            .env
            .insert("CALLS".into(), calls.to_string_lossy().into());
        ensure_launch_setup(temp.path(), Tool::Claude, Some(&launch))
            .await
            .unwrap();
        ensure_launch_setup(temp.path(), Tool::Claude, Some(&launch))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            "--profile test --version\n"
        );
    }

    #[test]
    fn scoped_install_covers_each_provider_without_installing_other_providers() {
        for (tool, prefix) in TOOL_SKILL_DIRS {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path();
            let root = home.join(prefix);
            install_provider(home, tool, &root, false, false, false, &mut vec![]).unwrap();
            for asset in user_asset_inventory(false)
                .into_iter()
                .filter(|a| a.rel.starts_with(&format!("{prefix}/")))
            {
                assert_eq!(
                    std::fs::read_to_string(home.join(asset.rel)).unwrap(),
                    asset.expected
                );
            }
            for (other, dir) in TOOL_SKILL_DIRS {
                if other != tool {
                    assert!(!home.join(dir).exists(), "unselected provider {other:?}");
                }
            }
            let before = load_manifest(home);
            install_provider(home, tool, &root, false, false, false, &mut vec![]).unwrap();
            assert_eq!(load_manifest(home), before);
        }
    }

    #[tokio::test]
    async fn opencode_launch_upgrades_owned_global_copy_and_preserves_drift_before_profile_repair()
    {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let global = home.join(".config/opencode");
        let selected = temp.path().join("selected");
        install_provider(
            &home,
            Tool::OpenCode,
            &global,
            false,
            false,
            false,
            &mut vec![],
        )
        .unwrap();
        let old = "// previous setup-owned plugin\n";
        let plugin = global.join("plugin/clank.js");
        std::fs::write(&plugin, old).unwrap();
        let mut manifest = load_manifest(&home);
        manifest.insert(
            ".config/opencode/plugin/clank.js".into(),
            blake3::hash(old.as_bytes()).to_hex().to_string(),
        );
        save_manifest(&home, &manifest).unwrap();
        let launch = clank_core::agent_config::LaunchConfig {
            env: std::collections::BTreeMap::from([
                ("HOME".into(), home.to_string_lossy().into()),
                (
                    "XDG_CONFIG_HOME".into(),
                    home.join(".config").to_string_lossy().into(),
                ),
                (
                    "OPENCODE_CONFIG_DIR".into(),
                    selected.to_string_lossy().into(),
                ),
            ]),
            ..Default::default()
        };
        ensure_launch_setup(temp.path(), Tool::OpenCode, Some(&launch))
            .await
            .unwrap();
        for path in [&plugin, &selected.join("plugin/clank.js")] {
            assert_eq!(std::fs::read_to_string(path).unwrap(), OPENCODE_PLUGIN);
        }
        let modified = std::fs::metadata(&plugin).unwrap().modified().unwrap();
        ensure_launch_setup(temp.path(), Tool::OpenCode, Some(&launch))
            .await
            .unwrap();
        assert_eq!(
            std::fs::metadata(&plugin).unwrap().modified().unwrap(),
            modified
        );
        std::fs::write(&plugin, "user edit").unwrap();
        let missing = selected.join("skills/clank-master/SKILL.md");
        std::fs::remove_file(&missing).unwrap();
        assert!(
            ensure_launch_setup(temp.path(), Tool::OpenCode, Some(&launch))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), "user edit");
        assert!(
            !missing.exists(),
            "all loaded roots must pass inspection before repair"
        );
    }

    /// A skill clank stopped shipping goes only while setup's manifest
    /// still vouches for its bytes, in the default home and a custom one.
    #[test]
    fn a_retired_skill_is_removed_only_while_setup_owns_its_bytes() {
        const OLD: &str = "---\nname: clank-pr-review\n---\nold body\n";
        let rel = "skills/clank-pr-review/SKILL.md";
        for custom in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = if custom {
                temp.path().join("profile")
            } else {
                home.join(".codex")
            };
            let domain = if custom { root.clone() } else { home.clone() };
            let key = if custom {
                rel.to_owned()
            } else {
                format!(".codex/{rel}")
            };
            let install = |dry_run: bool| {
                let mut summary = Vec::new();
                install_provider(
                    &home,
                    Tool::Codex,
                    &root,
                    false,
                    false,
                    dry_run,
                    &mut summary,
                )
                .unwrap();
                summary
            };
            install(false);
            let path = root.join(rel);
            let plant = |content: &str, recorded: Option<&str>| {
                write_file(&path, content);
                let mut manifest = load_manifest(&domain);
                match recorded {
                    Some(bytes) => manifest.insert(key.clone(), content_hash(bytes)),
                    None => manifest.remove(&key),
                };
                save_manifest(&domain, &manifest).unwrap();
            };

            plant(OLD, Some(OLD));
            assert!(!inspect_provider(&home, Tool::Codex, &root, false).unwrap());
            let before = load_manifest(&domain);
            let summary = install(true);
            assert!(
                summary
                    .iter()
                    .any(|l| l.contains("remove") && l.contains("clank-pr-review")),
                "{summary:?}"
            );
            assert!(path.is_file(), "a dry run removes nothing");
            assert_eq!(load_manifest(&domain), before, "nor touches the manifest");

            install(false);
            assert!(!path.exists());
            assert!(!path.parent().unwrap().exists(), "its emptied dir goes too");
            assert!(!load_manifest(&domain).contains_key(&key));
            assert!(inspect_provider(&home, Tool::Codex, &root, false).unwrap());

            plant(OLD, Some(OLD));
            let sibling = path.with_file_name("notes.md");
            write_file(&sibling, "mine");
            install(false);
            assert!(!path.exists());
            assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "mine");
            std::fs::remove_file(&sibling).unwrap();

            for (content, recorded) in [("edited", Some(OLD)), (OLD, None)] {
                plant(content, recorded);
                assert!(inspect_provider(&home, Tool::Codex, &root, false).unwrap());
                install(false);
                assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            }
        }
    }

    #[test]
    fn scoped_custom_homes_keep_assets_and_manifest_in_the_selected_domain() {
        for (tool, _) in TOOL_SKILL_DIRS {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("profile");
            install_provider(&home, tool, &root, false, false, false, &mut vec![]).unwrap();
            assert!(!home.exists());
            assert!(root.join("skills/clank-master/SKILL.md").is_file());
            assert!(load_manifest(&root).contains_key("skills/clank-master/SKILL.md"));
            if tool == Tool::OpenCode {
                assert!(root.join("plugin/clank.js").is_file());
            }
        }
    }

    #[test]
    fn scoped_alias_of_default_home_uses_existing_manifest_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = home.join(".grok");
        install_provider(&home, Tool::Grok, &root, false, false, false, &mut vec![]).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        install_provider(&home, Tool::Grok, &alias, false, false, false, &mut vec![]).unwrap();
        assert!(load_manifest(&home).contains_key(".grok/skills/clank-master/SKILL.md"));
        assert!(!alias.join(".clank/setup-manifest.json").exists());
    }

    #[test]
    fn scoped_dry_run_does_not_create_a_lock_or_installation_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("absent");
        for (tool, dir) in TOOL_SKILL_DIRS {
            install_provider(
                &home,
                tool,
                &home.join(dir),
                false,
                false,
                true,
                &mut vec![],
            )
            .unwrap();
        }
        assert!(!home.exists());
    }

    #[test]
    fn concurrent_scoped_installs_preserve_the_shared_manifest_and_user_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_owned();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let user_hook = serde_json::json!({"type":"command","command":"user-command"});
        std::fs::write(
            home.join(".codex/hooks.json"),
            serde_json::to_vec(
                &serde_json::json!({"keep":true,"hooks":{"Stop":[{"hooks":[user_hook.clone()]}]}}),
            )
            .unwrap(),
        )
        .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = vec![];
        for _ in 0..2 {
            for (tool, dir) in TOOL_SKILL_DIRS {
                let home = home.clone();
                let barrier = barrier.clone();
                threads.push(std::thread::spawn(move || {
                    barrier.wait();
                    install_provider(
                        &home,
                        tool,
                        &home.join(dir),
                        false,
                        false,
                        false,
                        &mut vec![],
                    )
                    .unwrap();
                }));
            }
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let manifest = load_manifest(&home);
        for asset in user_asset_inventory(false) {
            assert_eq!(
                manifest.get(&asset.rel),
                Some(&content_hash(&asset.expected))
            );
        }
        let hooks: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join(".codex/hooks.json")).unwrap())
                .unwrap();
        assert_eq!(hooks["keep"], true);
        assert!(
            hooks["hooks"]["Stop"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap())
                .any(|h| h == &user_hook)
        );
        assert_eq!(clank_hook_entries(&hooks, "Stop").len(), 1);
    }

    #[test]
    fn scoped_non_force_setup_preserves_drift_and_malformed_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join(".codex");
        install_provider(home, Tool::Codex, &root, false, false, false, &mut vec![]).unwrap();
        let skill = root.join("skills/clank-master/SKILL.md");
        std::fs::write(&skill, "my local changes").unwrap();
        assert!(
            install_provider(home, Tool::Codex, &root, false, false, false, &mut vec![]).is_err()
        );
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), "my local changes");
        std::fs::remove_file(skill).unwrap();
        let hooks = root.join("hooks.json");
        std::fs::write(&hooks, "not JSON").unwrap();
        assert!(
            install_provider(home, Tool::Codex, &root, false, false, false, &mut vec![]).is_err()
        );
        assert_eq!(std::fs::read_to_string(hooks).unwrap(), "not JSON");
    }

    #[test]
    fn scoped_install_preserves_symlinked_settings_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join(".codex");
        std::fs::create_dir(&root).unwrap();
        let target = home.join("shared-hooks.json");
        std::fs::write(&target, r#"{"keep":true}"#).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let hooks = root.join("hooks.json");
        std::os::unix::fs::symlink(&target, &hooks).unwrap();
        install_provider(home, Tool::Codex, &root, false, false, false, &mut vec![]).unwrap();
        assert!(
            std::fs::symlink_metadata(hooks)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap();
        assert_eq!(value["keep"], true);
        assert_eq!(clank_hook_entries(&value, "Stop").len(), 1);
    }

    #[test]
    fn a_ci_watch_can_be_configured_from_the_docs_alone() {
        // The kinds, the scope, and the one filter that does not
        // apply to a conclusion — in the agent's own instructions.
        // In the events ARRAY, not merely in the prose beside it —
        // an agent copies the array.
        let body: String = SKILL_MASTER_BODY
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            body.contains(r#""ci_passed","ci_failed""#),
            "the kinds are in the events list an agent would copy",
        );
        assert!(
            SKILL_MASTER_BODY.contains(r#""prs":[7,9]"#),
            "the scope is shown, not just named",
        );
        assert!(
            SKILL_MASTER_BODY.contains("no actor"),
            "and that the own-actions filter has nothing to say about a conclusion",
        );
        // The scope is no longer CI's alone, and which kinds it does
        // NOT govern is the part a reader would otherwise assume.
        let master: String = SKILL_MASTER_BODY
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            master.contains("every kind that NAMES a PR"),
            "the scope's reach is stated: {master}",
        );
        assert!(
            master.contains("`pr_opened` is outside it"),
            "and its exception: {master}",
        );
    }

    #[test]
    fn the_github_skill_does_not_promise_catch_up_it_cannot_do() {
        // The feed's catch-up sentence is true of the feed and false
        // of a state read: the plan's own correction (a-red-build-is-
        // a-wake). Reading the CURRENT result is all an arm can do.
        // Whitespace-normalized: a doc test that a reflow can break
        // teaches people to reflow around the test.
        let ci: String = GITHUB_SKILL_BODY
            .split("## CI conclusions are read, not received")
            .nth(1)
            .expect("the github skill explains what a read is")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(ci.contains("not recoverable"), "{ci}");
        assert!(ci.contains("sees the CURRENT result"), "{ci}");
        assert!(
            ci.contains("announced once"),
            "and that an unchanged build stays quiet: {ci}",
        );
        // The two claims a reader could act on wrongly: that
        // something watches between waits (nothing does), and that a
        // cue always means an immediate read (a storm is dropped).
        assert!(
            ci.contains("as long as your `clank wait` does"),
            "observation needs an armed wait: {ci}",
        );
        assert!(ci.contains("DROPPED, not queued"), "{ci}");
    }

    #[test]
    fn the_master_skill_sends_readers_to_the_command_not_the_files() {
        let reading = SKILL_MASTER_BODY
            .split("### Reading feedback")
            .nth(1)
            .expect("the skill has a Reading feedback section");
        assert!(
            reading.contains("clank feedback read --commit <sha>"),
            "{reading}"
        );
        assert!(reading.contains("clank feedback read --plan"), "{reading}");
        assert!(
            reading.contains(".clank/agents/*/feedback/") && reading.contains("not to be read"),
            "the files are named only to say they are not the way in: {reading}",
        );
    }

    #[test]
    fn seed_autosquash_seeds_when_absent_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".clank/config.json");
        // Absent → seeds, and the notice DISCLOSES the destructive default
        // (in-place rewrite + published-history risk + both opt-outs) at the
        // surface, not just in a code comment (ruthless 432a82e).
        let notice = seed_autosquash_default(&cfg, false).unwrap().unwrap();
        assert!(
            notice.contains("REWRITING the current branch in place"),
            "{notice}"
        );
        assert!(notice.contains("published history"), "{notice}");
        assert!(notice.contains("finish.autosquash false"), "{notice}");
        assert!(notice.contains("--no-squash"), "{notice}");
        assert!(crate::cli::config::json_path_present(
            &cfg,
            &["finish", "autosquash"]
        ));
        // Already set → no-op (explicit value preserved).
        assert!(seed_autosquash_default(&cfg, false).unwrap().is_none());
    }

    #[test]
    fn seed_autosquash_dry_run_reports_but_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".clank/config.json");
        let line = seed_autosquash_default(&cfg, true).unwrap();
        assert!(line.unwrap().contains("would seed"));
        assert!(!cfg.exists(), "dry-run writes nothing");
    }

    /// The `- **FINISHED**:` bullet through (exclusive) the next
    /// `- **REQUEST_CHANGES**:` bullet.
    fn finished_block(skill: &str) -> &str {
        let start = skill
            .find("- **FINISHED**:")
            .expect("skill has a FINISHED bullet");
        let rest = &skill[start..];
        let end = rest
            .find("- **REQUEST_CHANGES**:")
            .expect("skill has a REQUEST_CHANGES bullet after FINISHED");
        &rest[..end]
    }

    #[test]
    fn finished_defined_once_in_reviewer_and_tool_independent() {
        // finished-means-impl-done-not-plan-text: FINISHED is a REVIEWER
        // verdict, defined in ONE source (the reviewer body), so the two
        // tools can't drift on what it means; the master never defines a
        // verdict. Single-source replaces the old per-tool equality guard.
        let rc = compose_skill(Role::Reviewer, Tool::Claude);
        let rx = compose_skill(Role::Reviewer, Tool::Codex);
        assert_eq!(
            finished_block(&rc),
            finished_block(&rx),
            "FINISHED must read identically across tools (single source)"
        );
        assert!(
            finished_block(&rc).contains("implemented and merge-ready"),
            "FINISHED must mean the work is implemented, not the plan text written"
        );
        assert!(
            !compose_skill(Role::Master, Tool::Claude).contains("- **FINISHED**:"),
            "the master skill must not define verdicts (reviewer-only)"
        );
    }

    #[test]
    fn grok_skill_teaches_the_native_guarded_handoff() {
        let mg = compose_skill(Role::Master, Tool::Grok);
        assert!(mg.contains("clank wait"));
        assert!(mg.contains("block_until_ms: 0"));
        assert!(mg.contains("clank auto off"));
        assert!(!mg.contains("Nothing reminds you"));
        assert!(
            mg.contains("run_terminal_command"),
            "grok shell name substituted"
        );
        assert!(!mg.contains("{{WORK_LOOP}}") && !mg.contains("{{SHELL}}"));
        assert!(
            !mg.contains("Stop hook (blocked) feedback:"),
            "codex-only phrase must not leak into grok"
        );
        assert!(
            !mg.contains("run_in_background: true"),
            "claude-only phrasing must not leak into grok"
        );
    }

    /// Every role handles a message the same way, and the master's
    /// command list names the sender (a-message-wakes-its-agent-until-
    /// it-is-handled). The verbs are the EVENT verbs now: a message is
    /// an event like any other (a-message-is-an-event-like-any-other),
    /// so a skill that still taught `clank inbox` would be teaching a
    /// command that no longer exists.
    #[test]
    fn both_role_skills_teach_the_message_loop() {
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode] {
            for role in [Role::Master, Role::Reviewer] {
                let skill = compose_skill(role, tool);
                for step in [
                    "clank events read",
                    "clank events ack",
                    "clank send",
                    "--master",
                ] {
                    assert!(
                        skill.contains(step),
                        "{role:?}/{tool:?} skill lacks `{step}`"
                    );
                }
            }
            assert!(
                compose_skill(Role::Master, tool)
                    .contains("- `clank send (--to <agent> | --master)"),
                "{tool:?}: the master's command list names `clank send`"
            );
        }
    }

    #[test]
    fn role_skills_are_lean_each_lacks_the_other_roles_commands() {
        for tool in [Tool::Claude, Tool::Codex] {
            let master = compose_skill(Role::Master, tool);
            let reviewer = compose_skill(Role::Reviewer, tool);
            // Master owns finish + roster; never writes verdicts.
            assert!(
                master.contains("clank finish"),
                "master must document finish"
            );
            assert!(
                master.contains("clank agent make-master"),
                "master must document roster commands"
            );
            assert!(
                !master.contains("--verdict"),
                "master must not carry verdict-writing mechanics"
            );
            // Reviewer writes verdicts; never finishes/promotes.
            assert!(
                reviewer.contains("clank feedback write") && reviewer.contains("--verdict"),
                "reviewer must document the verdict-write command"
            );
            for verdict in ["CONTINUE", "FINISHED", "REQUEST_CHANGES"] {
                assert!(reviewer.contains(verdict), "reviewer must define {verdict}");
            }
            assert!(
                !reviewer.contains("clank finish") && !reviewer.contains("queue promote"),
                "reviewer must not carry master-only commands"
            );
        }
    }

    /// One skill serves reviewers and standby agents, and it tells a
    /// standby agent to write no verdicts without telling it to ignore
    /// anything else it is woken with
    /// (a-standby-agent-wakes-only-for-messages).
    #[test]
    fn the_reviewer_skill_serves_standby_without_narrowing_what_it_handles() {
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode] {
            let reviewer = compose_skill(Role::Reviewer, tool);
            assert_eq!(
                compose_skill(Role::Standby, tool),
                reviewer,
                "{tool:?}: one skill serves both"
            );
            let guard = reviewer
                .lines()
                .find(|l| l.starts_with("> ROLE GUARD"))
                .unwrap();
            assert!(
                guard.contains("**standby**") && guard.contains("writes no verdicts"),
                "{tool:?}: {guard}"
            );
            for narrowing in ["messages only", "only messages", "act on messages"] {
                assert!(!guard.contains(narrowing), "{tool:?}: {guard}");
            }
            let description = reviewer
                .lines()
                .find(|l| l.starts_with("description:"))
                .unwrap();
            assert!(description.contains("standby"), "{tool:?}: {description}");
            assert!(
                !compose_skill(Role::Master, tool).contains("**standby**"),
                "{tool:?}: the master skill's guard is the master's alone"
            );
        }
    }

    /// The normative inventory, asserted PER TOOL.
    ///
    /// `compose_skill` takes the tool and the compositions are allowed
    /// to differ, so a leaner per-tool variant must not become a way to
    /// lose a rule. Checking only claude — as this test did — left
    /// exactly that hole: codex's composition could drop
    /// architecture-first review and the suite stayed green.
    ///
    /// Asserted against the COMPOSITION, never the asset files: the
    /// role guard is generated in `compose_skill_with` and the
    /// anti-poll rule lives in the `WORK_LOOP_*` constants, so neither
    /// appears in any `.md` and an asset-level test would miss both.
    /// The composed CODEX reviewer skill has a byte ceiling.
    ///
    /// Codex re-reads this document IN FULL on every reviewing turn —
    /// its skill contract says skills do not carry across turns and
    /// must be read completely before acting — so size here is a
    /// RECURRING cost, measured at 587 reads in one session. That
    /// makes bytes the acceptance criterion, and creep a regression.
    ///
    /// Claude's is deliberately NOT capped: it is read once per
    /// session and kept, so its bytes are amortised and guidance there
    /// is worth more than the space.
    #[test]
    fn the_codex_reviewer_skill_stays_under_its_ceiling() {
        // 5,667B before the compression that introduced this test, at
        // 5,300. Raised by the 285B the inbox section costs — every role
        // must be told how a message is handled
        // (a-message-wakes-its-agent-until-it-is-handled) — and by the
        // 112B that let the guard and description admit standby agents,
        // who would otherwise be told to use the master's skill
        // (a-standby-agent-wakes-only-for-messages).
        const CEILING: usize = 5_700;
        let len = compose_skill(Role::Reviewer, Tool::Codex).len();
        assert!(
            len <= CEILING,
            "codex reviewer skill is {len}B, over the {CEILING}B ceiling (5,667B \
             before compression, 5,300B until the inbox, 5,600B until standby) — \
             re-read in full every reviewing turn, so growth here is paid again \
             on every wake"
        );
    }

    /// Every role on every tool is taught to hand long background work to
    /// clank: `run` for shell work, `attend` by name for a subagent, and no
    /// polling while it runs.
    #[test]
    fn every_skill_teaches_background_attendance() {
        for tool in [Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode] {
            for role in [Role::Master, Role::Reviewer] {
                let skill = compose_skill(role, tool);
                for needle in [
                    "clank run --desc",
                    "clank attend \"<name>\"",
                    "never poll it",
                ] {
                    assert!(
                        skill.contains(needle),
                        "{role:?} skill for {tool:?} does not teach `{needle}`"
                    );
                }
            }
        }
    }

    #[test]
    fn role_invariants_lead_each_body() {
        for tool in [Tool::Claude, Tool::Codex] {
            let master = compose_skill(Role::Master, tool);
            let reviewer = compose_skill(Role::Reviewer, tool);
            assert!(
                master.contains("Commit → STOP → get woken") && master.contains("PROMOTING"),
                "master must encode the commit->yield loop incl. promotion ({tool:?})"
            );
            // Two DISTINCT rules that read as one. The first bounds
            // what a verdict may consider; the second forbids blocking
            // on something outside that bound. Asserting only the
            // second would let a compression delete the scope boundary
            // itself with every test still green.
            assert!(
                reviewer.contains("Review ONLY committable artifacts"),
                "reviewer must bound its scope to committable artifacts ({tool:?})"
            );
            assert!(
                reviewer
                    .contains("NEVER withhold CONTINUE or FINISHED waiting on a manual/external"),
                "reviewer must never gate a verdict on a manual/external step ({tool:?})"
            );
            assert!(
                reviewer.contains("ARCHITECTURE-FIRST"),
                "reviewer must encode architecture-first review ({tool:?})"
            );
            // Workflow semantics `clank --help` cannot supply, and the
            // guard against the historical failure mode: writing
            // feedback files directly, or running on into the next
            // item. So it can never be compressed out as "reference".
            assert!(
                reviewer.contains("exactly ONE verdict") && reviewer.contains("then STOP"),
                "reviewer must encode one-verdict-then-stop ({tool:?})"
            );
            assert!(
                reviewer.contains("DO NOT** CONTINUE"),
                "reviewer must forbid CONTINUE while gating on a change ({tool:?})"
            );
            // The COMPLETE minimal form. Asserting only the command
            // name and `--verdict` would let `--commit`, `--author` or
            // `-m` fall out of the template with the suite green,
            // leaving a reviewer to guess the invocation it is told to
            // compose itself.
            for part in [
                "clank feedback write --commit <sha>",
                "--verdict continue|finished|request-changes",
                "--author <agent> -m \"<message>\"",
            ] {
                assert!(
                    reviewer.contains(part),
                    "reviewer is missing part of the feedback-write form: {part} ({tool:?})"
                );
            }
            // The PROHIBITION itself, not merely the absence of the
            // commands. A compression could delete this line while
            // `clank finish` stays absent, and absence alone never
            // tells a reviewer it must not reach for them.
            assert!(
                reviewer
                    .contains("You never run finish / promote / roster / plan-authoring commands"),
                "reviewer must STATE the reviewer-only command prohibition ({tool:?})"
            );
            for body in [&master, &reviewer] {
                assert!(
                    body.contains("ROLE GUARD"),
                    "the guard is generated, not an asset — it must survive composition ({tool:?})"
                );
                assert!(
                    body.contains(work_loop(tool, false).prohibition),
                    "both roles must carry the work-loop prohibition ({tool:?})"
                );
            }
        }
    }

    /// EVERY work loop forbids the agent taking the wait into its own
    /// hands, and the prohibition reaches the reader.
    ///
    /// The wording cannot be shared, so it must not be asserted: where
    /// the hook or the plugin owns the wait the rule is "do not arm
    /// one", and grok's is the opposite half — arm exactly one, in the
    /// background, and do not spin. What every loop owes is the
    /// SUBJECT and a reader who sees it.
    ///
    /// This replaces a search for the literal "NEVER poll" over two of
    /// the five loops. Two of the other three do not contain that
    /// phrase at all and never could, so the old assertion could not
    /// have been extended to them; it went red when a rewrite changed
    /// `NEVER` to `Never` while leaving the rule intact, which is a
    /// test measuring the wrong thing in both directions at once.
    #[test]
    fn every_work_loop_forbids_taking_the_wait_into_its_own_hands() {
        for (tool, _) in TOOL_SKILL_DIRS {
            for claude_async in [false, true] {
                let teaching = work_loop(tool, claude_async);
                // Naming the subject is also what rules out the empty
                // prohibition, which every `contains` below would
                // otherwise accept.
                assert!(
                    teaching.prohibition.contains("clank wait"),
                    "{tool:?} (async={claude_async}) must name the wait it rules on"
                );
                for role in [Role::Master, Role::Reviewer, Role::Standby] {
                    assert!(
                        compose_skill_with(role, tool, claude_async).contains(teaching.prohibition),
                        "{role:?}/{tool:?} (async={claude_async}) drops the prohibition in \
                         composition"
                    );
                }
            }
        }

        // grok is the only tool whose AGENT owns the wait — nothing
        // else arms one for it. Handing it another tool's loop would
        // tell it not to arm, and then nothing would ever wake it, so
        // its teaching must be no one else's.
        let grok = work_loop(Tool::Grok, false).arrival;
        for (tool, _) in TOOL_SKILL_DIRS {
            if tool == Tool::Grok {
                continue;
            }
            for claude_async in [false, true] {
                assert_ne!(
                    work_loop(tool, claude_async).arrival,
                    grok,
                    "{tool:?} (async={claude_async}) was handed grok's arming loop"
                );
            }
        }
    }

    #[test]
    fn role_skills_mark_repo_config_local_and_gitignored() {
        // clank-config-local-only: generated skills must not teach agents
        // that `.clank/config.json` is a shared/tracked project artifact.
        for role in [Role::Master, Role::Reviewer] {
            for tool in [Tool::Claude, Tool::Codex, Tool::Grok] {
                let body = compose_skill(role, tool);
                assert!(
                    body.contains("`config.json` — local, gitignored repo config"),
                    "{role:?}/{tool:?} skill must mark config.json local-only"
                );
                assert!(
                    body.contains("repo ROSTER"),
                    "{role:?}/{tool:?} skill must still name the roster semantics"
                );
                assert!(
                    !body.contains("`config.json` — the repo ROSTER"),
                    "{role:?}/{tool:?} skill must not carry the legacy tracked-artifact wording"
                );
            }
        }
    }

    #[test]
    fn role_guard_descriptions_are_unambiguous_both_ways() {
        let m = compose_skill(Role::Master, Tool::Claude);
        assert!(m.contains("name: clank-master"));
        assert!(
            m.contains("Use ONLY when you are the master") && m.contains("use clank-reviewer"),
            "master description must guard its role and redirect the other"
        );
        let r = compose_skill(Role::Reviewer, Tool::Claude);
        assert!(r.contains("name: clank-reviewer"));
        assert!(
            r.contains("Use ONLY when you are a reviewer") && r.contains("use clank-master"),
            "reviewer description must guard its role and redirect the other"
        );
    }

    #[test]
    fn tool_specific_bits_are_substituted() {
        let mc = compose_skill(Role::Master, Tool::Claude);
        let mx = compose_skill(Role::Master, Tool::Codex);
        assert!(mc.contains("run via Bash"));
        assert!(mx.contains("run via shell"));
        assert!(
            !mc.contains("{{") && !mx.contains("{{"),
            "all tokens substituted"
        );
        // Slash command is claude-only.
        assert!(
            mc.contains("$ARGUMENTS"),
            "claude skill has the /clank slash command"
        );
        assert!(
            !mx.contains("$ARGUMENTS"),
            "codex skill omits the slash command"
        );
        // Codex carries the stop-hook phrasing note; claude does not.
        assert!(mx.contains("Stop hook (blocked) feedback:"));
        assert!(!mc.contains("Stop hook (blocked) feedback:"));
    }

    #[test]
    fn work_loop_teaches_the_per_tool_delivery_model() {
        // Claude and Codex both let the Stop hook own the wait.
        let mc = compose_skill(Role::Master, Tool::Claude);
        let mx = compose_skill(Role::Master, Tool::Codex);
        assert!(mc.contains("Stop hook owns the"));
        assert!(!mc.contains("run_in_background"));
        assert!(mx.contains("Act on Stop-hook work IMMEDIATELY"));
        assert!(!mx.contains("run_in_background"));
        // opencode is plugin-driven: work is INJECTED as a prompt;
        // self-arming is forbidden (a second wait duplicates the
        // plugin's deliveries).
        let mo = compose_skill(Role::Master, Tool::OpenCode);
        assert!(mo.contains("Work arrives on its own"));
        assert!(mo.contains("NEVER run `clank wait` yourself"));
        assert!(!mo.contains("Keep a `clank wait` armed"));
        assert!(!mo.contains("run_in_background"));
        // Async Claude also keeps the wait in its Stop hook.
        let ma = compose_skill_with(Role::Master, Tool::Claude, true);
        assert!(ma.contains("Work arrives on its own"));
        assert!(ma.contains("NEVER run `clank wait` yourself"));
        assert!(!ma.contains("Keep a `clank wait` armed"));
        assert_eq!(
            compose_skill_with(Role::Master, Tool::Claude, false),
            compose_skill(Role::Master, Tool::Claude),
            "legacy unchanged"
        );
    }

    #[test]
    fn skills_teach_compose_from_hint_not_verbatim() {
        // wait-output-is-a-minimal-hint: the wake carries no verbatim
        // command; the shared core teaches composing from the hint.
        const HINT_SENTENCE: &str = "Each item is a one-line hint: kind, plan, short sha.";
        for role in [Role::Master, Role::Reviewer] {
            for tool in [Tool::Claude, Tool::Codex] {
                let body = compose_skill(role, tool);
                assert!(!body.contains("run it verbatim"), "no verbatim promise");
                assert!(
                    body.contains(HINT_SENTENCE),
                    "must teach composing from the one-line hint"
                );
            }
        }
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn install_skill_writes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "a/b/SKILL.md",
            "hello",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        let path = dir.path().join("a/b/SKILL.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(summary[0].contains("write"));
    }

    #[test]
    fn install_skill_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "hello");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "hello",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert!(summary[0].contains("ok"));
    }

    #[test]
    fn install_skill_refuses_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "edited by user\n");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        let err = install_skill(
            dir.path(),
            "SKILL.md",
            "hello",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--force"), "unexpected: {err}");
        // File preserved.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited by user\n");
    }

    #[test]
    fn install_skill_force_overwrites_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "edited by user\n");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "hello",
            &[],
            true,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(summary[0].contains("force"));
    }

    // ── provenance manifest (setup-overwrites-its-own-old-versions) ──

    #[test]
    fn install_skill_upgrades_a_known_old_version_without_force() {
        // The file on disk is setup's OWN previous version: the
        // manifest recorded its hash when setup wrote it. That is a
        // defined OLD state, not user drift, so plain setup overwrites
        // and records the new hash.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "setup v1 content");
        let mut manifest = load_manifest(dir.path());
        manifest.insert("SKILL.md".to_string(), content_hash("setup v1 content"));
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "setup v2 content",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "setup v2 content");
        assert!(summary[0].contains("upgrade"), "{summary:?}");
        assert_eq!(
            manifest.get("SKILL.md").map(String::as_str),
            Some(content_hash("setup v2 content").as_str()),
            "the new hash is on record for the next upgrade"
        );
    }

    #[test]
    fn install_skill_never_upgrades_unrecorded_drift() {
        // No provenance entry for this path: different content is
        // user drift, and the guard keeps its teeth.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "setup v1 content");
        let mut manifest = load_manifest(dir.path());
        manifest.insert("SKILL.md".to_string(), content_hash("some other file"));
        let mut summary = Vec::new();
        let err = install_skill(
            dir.path(),
            "SKILL.md",
            "setup v2 content",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
    }

    #[test]
    fn a_force_write_records_provenance_for_the_next_upgrade() {
        // The honest bridge: one --force against drift, and every
        // upgrade after it is silent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "edited by user\n");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "setup v1 content",
            &[],
            true,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        // The next setup with NEW expected content must not ask again.
        install_skill(
            dir.path(),
            "SKILL.md",
            "setup v2 content",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "setup v2 content");
    }

    #[test]
    fn an_ok_install_records_provenance_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "hello");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "hello",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(
            manifest.get("SKILL.md").map(String::as_str),
            Some(content_hash("hello").as_str())
        );
    }

    #[test]
    fn a_corrupt_manifest_is_empty_and_nothing_upgrades_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        write_file(&path, "setup v1 content");
        write_file(&manifest_path(dir.path()), "{not json");
        let mut manifest = load_manifest(dir.path());
        assert!(manifest.is_empty(), "corrupt reads as empty");
        let mut summary = Vec::new();
        let err = install_skill(
            dir.path(),
            "SKILL.md",
            "setup v2 content",
            &[],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
    }

    #[test]
    fn the_manifest_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = load_manifest(dir.path());
        manifest.insert("a".to_string(), content_hash("1"));
        save_manifest(dir.path(), &manifest).unwrap();
        assert_eq!(load_manifest(dir.path()), manifest);
    }

    #[test]
    fn classify_asset_state_is_the_one_definition_of_old_setup() {
        let expected = "v2";
        let alternate = "v2 other mode".to_string();
        let recorded = content_hash("v1");
        assert_eq!(
            classify_asset_state("v2", expected, &[alternate.clone()], Some(&recorded)),
            AssetState::Current
        );
        assert_eq!(
            classify_asset_state(
                "v2 other mode",
                expected,
                &[alternate.clone()],
                Some(&recorded)
            ),
            AssetState::ModeAlternate
        );
        // What doctor calls old, setup overwrites: manifest match.
        assert_eq!(
            classify_asset_state("v1", expected, &[alternate.clone()], Some(&recorded)),
            AssetState::OldSetup
        );
        // No record of it at all: drift, which refuses.
        assert_eq!(
            classify_asset_state("v1", expected, &[alternate.clone()], None),
            AssetState::Drift
        );
        assert_eq!(
            classify_asset_state("edited", expected, &[alternate], Some(&recorded)),
            AssetState::Drift
        );
    }

    #[test]
    fn install_skill_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        let mut manifest = load_manifest(dir.path());
        let mut summary = Vec::new();
        install_skill(
            dir.path(),
            "SKILL.md",
            "hello",
            &[],
            false,
            true,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert!(!path.exists());
        assert!(summary[0].contains("write")); // reports the intent
    }

    #[test]
    fn master_skill_uses_repo_wide_block_create_recipe() {
        // Every block is repo-wide, so the recipe must NOT carry a
        // scope flag — `--plan` is rejected at the parser now, and a
        // skill that still teaches it hands agents a failing command.
        // Scoped to the recipe's own paragraph: `--plan` is a real
        // flag on other commands the skill teaches.
        // `block ask` is a MASTER command — reviewers must not
        // carry it.
        for tool in [Tool::Claude, Tool::Codex] {
            let body = compose_skill(Role::Master, tool);
            assert!(
                body.contains(r#"clank block ask <name> -m "question""#),
                "master must document the flat block ask recipe"
            );
            let recipe = body
                .split("\n\n")
                .find(|para| para.contains("clank block ask"))
                .expect("the block ask recipe");
            assert!(
                !recipe.contains("--plan"),
                "block ask recipe must not carry the deleted --plan flag: {recipe}"
            );
            assert!(
                !compose_skill(Role::Reviewer, tool).contains("clank block ask"),
                "reviewer must not carry the master-only block-ask command"
            );
        }
    }

    #[test]
    fn merge_hook_creates_settings_with_claude_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook { async_mode: false }, false, &mut summary)
            .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let stop = &v["hooks"]["Stop"];
        assert_eq!(stop.as_array().unwrap().len(), 1);
        let inner = &stop[0]["hooks"][0];
        let cmd = inner["command"].as_str().unwrap();
        assert_eq!(cmd, "clank stop-hook --tool claude");
        let timeout = inner["timeout"].as_u64().unwrap();
        assert_eq!(timeout, HOOK_TIMEOUT_SECS);
        // Claude entries don't carry statusMessage.
        assert!(inner.get("statusMessage").is_none());
    }

    #[test]
    fn an_id_carrying_entry_is_owned_by_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".codex/hooks.json");
        write_file(
            &path,
            r#"{"hooks": {
                "SessionStart": [{"hooks": [{
                    "command": "\"/Users/x/.cargo/bin/clank\" stop-hook --tool codex --session-start",
                    "id": "clank-session-start", "timeout": 15, "type": "command"}]}],
                "Stop": [{"hooks": [{
                    "command": "'/Users/x/My Tools/clank' stop-hook --tool codex",
                    "id": "clank-stop-hook", "timeout": 60, "type": "command"}]}]
            }}"#,
        );
        let mut summary = Vec::new();
        install_codex_hooks(&path, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for event in ["Stop", "SessionStart"] {
            assert_eq!(
                v["hooks"][event].as_array().unwrap().len(),
                1,
                "{event}: replaced, not duplicated: {v}"
            );
        }
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["timeout"].as_u64(),
            Some(HOOK_TIMEOUT_SECS),
            "the entry is rewritten from the binary's constants"
        );
        assert_eq!(summary.len(), 2, "{summary:?}");
        assert!(summary.iter().all(|l| l.contains("merge")), "{summary:?}");
    }

    #[test]
    fn merge_hook_codex_carries_status_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".codex/hooks.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, CodexHook, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let msg = v["hooks"]["Stop"][0]["hooks"][0]["statusMessage"]
            .as_str()
            .unwrap();
        assert!(msg.contains("Clank"));
    }

    /// The codex SessionStart companion is installed beside the Stop
    /// entry, and a legacy file holding only the Stop entry gains it
    /// without the Stop entry moving (a-session-binds-itself-at-start).
    #[test]
    fn codex_gets_a_session_start_hook_beside_its_stop_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".codex/hooks.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, CodexHook, false, &mut summary).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        install_codex_hooks(&path, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ss = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(
            ss["command"],
            "clank stop-hook --tool codex --session-start"
        );
        assert_eq!(ss["timeout"], 15);
        assert!(ss.get("asyncRewake").is_none(), "codex knows no such field");
        let before: serde_json::Value = serde_json::from_str(&before).unwrap();
        assert_eq!(
            v["hooks"]["Stop"], before["hooks"]["Stop"],
            "the Stop entry is untouched"
        );
    }

    #[test]
    fn codex_hook_carries_no_async_field_of_any_kind() {
        // codex 0.147.0 PARSES `async` (and knows `asyncRewake`) but
        // has not implemented them: the binary carries
        // `skipping async hook in ` / `async hooks are not supported
        // yet`. Setting either would not make the hook non-blocking —
        // codex would decline to run it at all, silently disabling the
        // whole codex work loop for one log line. The field being
        // visible in codex's schema is an active invitation to that
        // mistake, so its absence is pinned rather than assumed
        // (codex-idle-is-not-a-hook-failure).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".codex/hooks.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, CodexHook, false, &mut summary).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let entry = &serde_json::from_str::<serde_json::Value>(&raw).unwrap()["hooks"]["Stop"][0]["hooks"]
            [0];
        assert!(
            entry.get("async").is_none(),
            "`async` must stay unset: {entry}"
        );
        assert!(
            entry.get("asyncRewake").is_none(),
            "`asyncRewake` must stay unset: {entry}"
        );
        // The ceiling still ships — absent, codex applies its own
        // default and cuts a legitimate park short.
        assert_eq!(entry["timeout"].as_u64(), Some(HOOK_TIMEOUT_SECS));
    }

    #[test]
    fn merge_hook_preserves_unrelated_stop_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        // Seed with an existing unrelated Stop hook.
        write_file(
            &path,
            r#"{
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "/my/other-tool --hook" }] }
                    ]
                }
            }"#,
        );
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook { async_mode: false }, false, &mut summary)
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "expected both hooks present, got: {v}");
        let commands: Vec<&str> = stop
            .iter()
            .map(|w| w["hooks"][0]["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&"/my/other-tool --hook"));
        assert!(commands.contains(&"clank stop-hook --tool claude"));
    }

    #[test]
    fn merge_hook_replaces_prior_clank_entry_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook { async_mode: false }, false, &mut summary)
            .unwrap();
        merge_hook_into_settings(&path, ClaudeHook { async_mode: false }, false, &mut summary)
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "re-run should not duplicate; got: {v}");
    }

    #[test]
    fn merge_hook_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.json");
        let mut summary = Vec::new();
        merge_hook_into_settings(&path, ClaudeHook { async_mode: false }, true, &mut summary)
            .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn mode_flip_migrates_canonical_claude_skills_without_force() {
        // codex 1483318: doctor tells the user plain `clank setup`
        // fixes mode drift, so it must actually run — a file exactly
        // equal to the OTHER mode's canonical variant is ours to
        // migrate; genuinely edited content still refuses.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("SKILL.md");
        let legacy = compose_skill_with(Role::Master, Tool::Claude, false);
        let asyncv = compose_skill_with(Role::Master, Tool::Claude, true);
        let mut summary = Vec::new();

        // legacy → async.
        std::fs::write(&path, &legacy).unwrap();
        let mut manifest = load_manifest(tmp.path());
        install_skill(
            tmp.path(),
            "SKILL.md",
            &asyncv,
            &[legacy.clone()],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), asyncv);
        assert!(summary.last().unwrap().contains("mode"), "{summary:?}");

        // async → legacy (the downgrade).
        install_skill(
            tmp.path(),
            "SKILL.md",
            &legacy,
            &[asyncv.clone()],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);

        // A user edit is NOT a canonical alternate — still refused
        // without --force.
        std::fs::write(&path, format!("{legacy}\n# my local note\n")).unwrap();
        let err = install_skill(
            tmp.path(),
            "SKILL.md",
            &asyncv,
            &[legacy.clone()],
            false,
            false,
            &mut manifest,
            &mut summary,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");

        // Dry-run migration reports but writes nothing.
        std::fs::write(&path, &legacy).unwrap();
        install_skill(
            tmp.path(),
            "SKILL.md",
            &asyncv,
            &[legacy.clone()],
            false,
            true,
            &mut manifest,
            &mut summary,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
    }

    #[test]
    fn asyncrewake_capability_floor_is_2_1_223() {
        assert!(claude_asyncrewake_capable("2.1.223 (Claude Code)"));
        assert!(claude_asyncrewake_capable("2.1.230 (Claude Code)"));
        assert!(claude_asyncrewake_capable("3.0.0 (Claude Code)"));
        assert!(!claude_asyncrewake_capable("2.1.222 (Claude Code)"));
        assert!(!claude_asyncrewake_capable("1.9.999"));
        assert!(!claude_asyncrewake_capable("garbage"));
        assert!(!claude_asyncrewake_capable(""));
    }

    #[test]
    fn claude_mode_writes_matching_entries_and_the_companion_in_every_mode() {
        // The durable-mode contract (claude-asyncrewake-work-loop):
        // async installs the --loop argv + asyncRewake field; legacy
        // neither. The SessionStart companion is MODE-INDEPENDENT —
        // it binds the launched session, so a downgrade keeps it
        // (a-session-binds-itself-at-start).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let mut summary = Vec::new();

        // A LEGACY machine, from nothing: the companion is installed
        // with the legacy Stop hook — binding does not wait for a
        // capable Claude Code.
        install_claude_hooks(&path, false, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "clank stop-hook --tool claude"
        );
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "clank stop-hook --tool claude --session-start",
            "legacy mode binds sessions too"
        );

        install_claude_hooks(&path, true, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(
            stop["command"],
            "clank stop-hook --tool claude --loop asyncrewake"
        );
        assert_eq!(stop["asyncRewake"], true);
        assert_eq!(stop["statusMessage"], "Clank: watching for work");
        let ss = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(
            ss["command"],
            "clank stop-hook --tool claude --session-start"
        );

        // Downgrade: legacy entry replaces in place; asyncRewake is
        // ABSENT (not false); the companion STAYS.
        install_claude_hooks(&path, false, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let stop = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["command"], "clank stop-hook --tool claude");
        assert!(stop.get("asyncRewake").is_none(), "absent, not false");
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "clank stop-hook --tool claude --session-start",
            "the companion binds sessions in every mode"
        );
    }

    #[test]
    fn question_hooks_are_owned_idempotent_and_leave_other_provider_hooks_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let user = serde_json::json!({"type":"command","command":"my-question-ui"});
        std::fs::write(&path,serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"AskUserQuestion","custom":7,"hooks":[user.clone(),{"type":"command","command":"clank question-hook"}]}]}}).to_string()).unwrap();
        install_claude_hooks(&path, false, false, &mut vec![]).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        install_claude_hooks(&path, false, false, &mut vec![]).unwrap();
        assert_eq!(first, std::fs::read_to_string(&path).unwrap());
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        for event in [
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "PostToolUseFailure",
        ] {
            assert_eq!(clank_hook_entries(&value, event).len(), 1);
        }
        assert_eq!(value["hooks"]["PreToolUse"][0]["custom"], 7);
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"],
            serde_json::json!([user])
        );
    }

    #[test]
    fn a_hook_is_clanks_by_the_command_it_runs() {
        let owned = |command: &str| hook_is_clank(&serde_json::json!({"command": command}));
        assert!(owned("clank stop-hook --tool claude"));
        assert!(owned("clank stop-hook --tool claude --loop asyncrewake"));
        assert!(owned(
            "/Users/x/.cargo/bin/clank stop-hook --tool codex --session-start"
        ));
        assert!(owned(
            r#""/Users/x/.cargo/bin/clank" stop-hook --tool claude --session-start"#
        ));
        assert!(owned("'/Users/x/My Tools/clank' stop-hook --tool codex"));
        assert!(!owned("'/Users/x/My Tools/clank stop-hook --tool codex"));
        assert!(!owned(r"clank\ stop-hook"));
        assert!(!owned("clank status"));
        assert!(!owned("clank stop-hooks"));
        assert!(!owned("notclank stop-hook"));
        assert!(!owned("echo clank stop-hook"));
        assert!(!owned(""));
        assert!(!hook_is_clank(
            &serde_json::json!({"id": "clank-stop-hook"})
        ));
    }

    /// `~/.claude/settings.json` as a restored machine had it: every
    /// clank `id` gone, three SessionStart copies piled up by setups
    /// that could no longer see their own entries.
    #[test]
    fn claude_settings_that_lost_their_ids_heal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let session_start = serde_json::json!({"hooks": [{
            "type": "command",
            "command": "clank stop-hook --tool claude --session-start",
            "timeout": 15
        }]});
        let user_stop = serde_json::json!({"hooks": [{"type": "command", "command": "/my/stop"}]});
        let user_start =
            serde_json::json!({"hooks": [{"type": "command", "command": "/my/start"}]});
        write_file(
            &path,
            &serde_json::json!({"hooks": {
                "Stop": [
                    {"hooks": [{
                        "type": "command",
                        "command": "clank stop-hook --tool claude --loop asyncrewake",
                        "timeout": HOOK_TIMEOUT_SECS,
                        "statusMessage": "Clank: watching for work",
                        "asyncRewake": true
                    }]},
                    user_stop
                ],
                "SessionStart": [session_start, user_start, session_start, session_start]
            }})
            .to_string(),
        );
        let mut summary = Vec::new();
        install_claude_hooks(&path, true, false, &mut summary).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (event, user) in [("Stop", &user_stop), ("SessionStart", &user_start)] {
            assert_eq!(
                clank_hook_entries(&v, event).len(),
                1,
                "{event}: one clank entry: {v}"
            );
            assert_eq!(
                &v["hooks"][event][0], user,
                "{event}: the user's hook leads"
            );
            assert_eq!(v["hooks"][event].as_array().unwrap().len(), 2, "{v}");
        }
        assert_eq!(summary.len(), 6, "{summary:?}");
        assert!(
            summary[..2].iter().all(|l| l.contains("merge")),
            "{summary:?}"
        );
        assert!(
            summary[2..].iter().all(|l| l.contains("add")),
            "{summary:?}"
        );
    }

    /// Ownership is the inner entry's, not its wrapper's: a user's
    /// hook sharing a wrapper with ours survives the merge, with the
    /// wrapper's other fields and its place.
    #[test]
    fn a_mixed_wrapper_keeps_what_is_not_clanks() {
        assert_mixed_wrapper_survives(ClaudeHook { async_mode: false });
        assert_mixed_wrapper_survives(SessionStartHook);
    }

    fn assert_mixed_wrapper_survives(kind: impl HookKind) {
        let (event, ours) = (kind.event(), kind.command());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let user = serde_json::json!({"type": "command", "command": "echo user-hook"});
        let other = serde_json::json!({"hooks": [{"type": "command", "command": "/other"}]});
        write_file(
            &path,
            &serde_json::json!({"hooks": {event: [
                {"matcher": "startup", "hooks": [
                    {"type": "command", "command": ours},
                    user
                ]},
                other
            ]}})
            .to_string(),
        );
        merge_hook_into_settings(&path, kind, false, &mut Vec::new()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let wrappers = v["hooks"][event].as_array().unwrap();
        assert_eq!(
            wrappers[0],
            serde_json::json!({"matcher": "startup", "hooks": [user]}),
            "{event}: the mixed wrapper keeps its place, matcher and user hook: {v}"
        );
        assert_eq!(wrappers[1], other, "{event}: {v}");
        assert_eq!(wrappers[2]["hooks"][0]["command"], ours, "{event}: {v}");
        assert_eq!(wrappers.len(), 3, "{event}: {v}");
    }

    #[test]
    fn user_asset_inventory_covers_every_tool_dir_and_the_plugin() {
        // Doctor verifies exactly this inventory; a skill added to
        // setup but missing here (or vice versa) is structurally
        // impossible — this test just pins the expected shape.
        let inv = user_asset_inventory(false);
        for (_, dir) in TOOL_SKILL_DIRS {
            for skill in ["clank-master", "clank-reviewer", "clank-github"] {
                let rel = format!("{dir}/skills/{skill}/SKILL.md");
                assert!(inv.iter().any(|a| a.rel == rel), "missing {rel}");
            }
        }
        assert!(
            inv.iter()
                .any(|a| a.rel == ".config/opencode/plugin/clank.js"
                    && a.expected == OPENCODE_PLUGIN)
        );
        assert_eq!(inv.len(), TOOL_SKILL_DIRS.len() * 3 + 1);
    }

    #[test]
    fn opencode_plugin_scrubs_every_foreign_identity_var() {
        // The plugin's blank-scrub list must track SESSION_IDENTITY_VARS
        // (minus its own var, which it SETS): a var added to clank's
        // identity set but not blanked by the plugin would leak a
        // parent agent's identity into opencode tool shells.
        for var in crate::agent_env::SESSION_IDENTITY_VARS {
            if *var == "OPENCODE_SESSION_ID" {
                assert!(OPENCODE_PLUGIN.contains("output.env.OPENCODE_SESSION_ID"));
                continue;
            }
            assert!(
                OPENCODE_PLUGIN.contains(&format!("\"{var}\"")),
                "plugin must blank-scrub {var}"
            );
        }
    }

    /// In-process model of the plugin's event-loop discipline —
    /// the state machine opencode_plugin.js's event handler must
    /// implement. The source-invariant tests below pin the JS to
    /// this model's critical ordering; the full JS is executable
    /// manually via `node crates/cli/tests/opencode_plugin_lifecycle.mjs`.
    struct LoopModel {
        inflight: bool,
        activity: u32,
        seen_msgs: std::collections::HashSet<&'static str>,
        pending_wait: Option<u32>,
        /// The turn was stopped by a human, not finished.
        aborted: bool,
        arms: u32,
        injections: u32,
    }

    impl LoopModel {
        fn new() -> Self {
            LoopModel {
                inflight: false,
                activity: 0,
                seen_msgs: Default::default(),
                pending_wait: None,
                aborted: false,
                arms: 0,
                injections: 0,
            }
        }

        /// message.updated with role=user: activity is FIRST
        /// SIGHTINGS of message ids only — opencode re-emits the
        /// turn's own user message after idle (housekeeping).
        fn user_message(&mut self, id: &'static str) {
            if self.seen_msgs.insert(id) {
                self.activity += 1;
                // Prompting again is what ends the stop Esc began.
                self.aborted = false;
            }
        }

        /// message.updated, role=assistant, error MessageAbortedError:
        /// opencode's mark for a turn a human stopped.
        fn abort(&mut self) {
            self.aborted = true;
        }

        fn idle(&mut self) {
            if self.inflight {
                return;
            }
            // Arming after an interrupt would long-poll holding the
            // guard, so the next genuine idle is ignored as in-flight.
            if self.aborted {
                return;
            }
            self.inflight = true;
            self.arms += 1;
            self.pending_wait = Some(self.activity);
        }

        /// The wait completes: the guard is released BEFORE the
        /// injection decision — the injected turn's terminal idle
        /// must arm the next wait (codex d7c8908).
        fn wait_returns(&mut self, work: bool) {
            let seen = self.pending_wait.take().expect("wait armed");
            self.inflight = false;
            if work && self.activity == seen && !self.aborted {
                self.injections += 1;
            }
        }
    }

    #[test]
    fn opencode_loop_model_rearms_and_discards_stale() {
        let mut m = LoopModel::new();
        // Turn 1: the turn's own user message, then idle; opencode
        // RE-updates that same message during the wait — a
        // re-sighting must not discard the continuation.
        m.user_message("msg_user_1");
        m.idle();
        m.user_message("msg_user_1");
        m.wait_returns(true);
        assert_eq!((m.arms, m.injections), (1, 1));
        // The injected turn runs (its own user message appears),
        // ends in a terminal idle: the next wait MUST arm even
        // though the injected turn's prompt call may still be
        // pending — the guard covers only the wait.
        m.user_message("msg_inject_1");
        m.idle();
        m.wait_returns(true);
        assert_eq!((m.arms, m.injections), (2, 2));
        // A genuinely NEW user message during a wait discards.
        m.idle();
        m.user_message("msg_user_2");
        m.wait_returns(true);
        assert_eq!((m.arms, m.injections), (3, 2));
        // An idle during an in-flight wait is ignored.
        m.idle();
        m.idle();
        assert_eq!(m.arms, 4);
    }

    /// Esc means stop. `session.idle` fires for an interrupted turn
    /// exactly as it does for a finished one, so the idle alone cannot
    /// tell them apart — and the staleness guard cannot either, since
    /// an abort produces no new USER message and leaves the activity
    /// count untouched. Both orderings are covered: opencode emits
    /// housekeeping after the idle, so the abort can be observed on
    /// either side of it.
    #[test]
    fn opencode_loop_model_does_not_prod_an_interrupted_turn() {
        // Abort observed BEFORE the idle: nothing arms, nothing injects.
        let mut m = LoopModel::new();
        m.user_message("msg_user_1");
        m.abort();
        m.idle();
        assert_eq!(
            (m.arms, m.injections),
            (0, 0),
            "an interrupted turn must not even arm a wait"
        );

        // Prompting again ends the stop: the loop works normally after.
        m.user_message("msg_user_2");
        m.idle();
        m.wait_returns(true);
        assert_eq!(
            (m.arms, m.injections),
            (1, 1),
            "a new prompt re-arms the loop"
        );

        // Abort observed AFTER the idle, while the wait is in flight:
        // the wait armed, so the discard has to happen at injection.
        m.user_message("msg_user_3");
        m.idle();
        m.abort();
        m.wait_returns(true);
        assert_eq!(
            (m.arms, m.injections),
            (2, 1),
            "an abort during the wait discards its continuation"
        );

        // And the latch persists: a later idle still injects nothing.
        m.idle();
        assert_eq!((m.arms, m.injections), (2, 1));

        // A turn that ends by COMPLETING is unaffected — the fix must
        // not buy quiet by breaking the loop.
        m.user_message("msg_user_4");
        m.idle();
        m.wait_returns(true);
        assert_eq!((m.arms, m.injections), (3, 2));
    }

    #[test]
    fn opencode_plugin_keys_the_abort_on_the_assistant_message() {
        let src = OPENCODE_PLUGIN;
        // The FACT, not a heuristic (elapsed time, empty output): the
        // assistant message's error, or a session error.
        assert!(src.contains(r#"error?.name === "MessageAbortedError""#));
        assert!(src.contains(r#"info?.role === "assistant""#));
        // session.error's schema makes sessionID optional. One without it
        // is dropped before anything reads it, never guessed at.
        let unattributed = src.find("if (!id) return").expect("no-session guard");
        assert!(unattributed < src.find("\"session.error\"").expect("session.error"));
        // The latch clears when the human prompts again.
        assert!(src.contains("aborted.delete(id)"));
        // Checked before arming, before renewing after a deadline,
        // and before injection. The abort can arrive on either side
        // of the idle or while the wait is in flight.
        assert_eq!(src.matches("aborted.has(id)").count(), 3);
        let arm = src.find("inflight.add(id)").expect("arm site");
        let release = src.find("inflight.delete(id)").expect("wait release");
        let inject = src
            .find("client.session.promptAsync(")
            .expect("injection site");
        let checks: Vec<_> = src
            .match_indices("aborted.has(id)")
            .map(|(i, _)| i)
            .collect();
        assert!(checks[0] < arm, "the arm-time check must precede arming");
        assert!(
            checks[1] > release && checks[1] < inject,
            "the renewal-time check must follow the wait release"
        );
        assert!(
            checks[2] > checks[1] && checks[2] < inject,
            "the injection-time check must follow renewal and precede injection"
        );
    }

    #[test]
    fn opencode_plugin_releases_the_wait_guard_before_injecting() {
        // The lifecycle regression (codex d7c8908): a guard held
        // across the injection await swallows the injected turn's
        // terminal session.idle and the loop goes dormant after one
        // delivery. Pinned at the source level: the finally that
        // releases the guard closes before the injection, which
        // must sit OUTSIDE the guarded try and use promptAsync
        // (return-on-accept), never the synchronous prompt().
        let src = OPENCODE_PLUGIN;
        let guarded_try = src.find("try {").expect("wait try block");
        let release = src.find("} finally {").expect("guard release");
        let inject = src
            .find("client.session.promptAsync(")
            .expect("promptAsync injection");
        assert!(guarded_try < release && release < inject);
        assert!(
            !src[guarded_try..inject].contains("promptAsync("),
            "injection must not be inside the wait guard"
        );
        assert!(
            !src.contains("session.prompt("),
            "the synchronous prompt() pins the whole model turn"
        );
    }

    #[test]
    fn opencode_plugin_staleness_counts_first_sightings_only() {
        // Observed live: opencode re-emits message.updated for the
        // turn's OWN user message right after session.idle; counting
        // user-role events (rather than new ids) marks every wait
        // stale and every continuation is silently discarded.
        assert!(OPENCODE_PLUGIN.contains("!seenUserMessages.has(info.id)"));
        assert!(OPENCODE_PLUGIN.contains("seenUserMessages.add(info.id)"));
    }

    #[test]
    fn opencode_plugin_pins_the_loop_discipline() {
        // Exact-session-only binding: no sessionID in the hook call
        // (user PTYs) → no injection, never a cwd guess.
        assert!(OPENCODE_PLUGIN.contains("if (!input.sessionID) return"));
        // At most one in-flight wait per session.
        assert!(OPENCODE_PLUGIN.contains("if (inflight.has(id)) return"));
        // Stale continuations are discarded, not injected.
        assert!(OPENCODE_PLUGIN.contains("!== seen) return"));
        // The real command path, empty-stdout-quiescent wire.
        assert!(OPENCODE_PLUGIN.contains("clank stop-hook --tool opencode"));
    }
}
