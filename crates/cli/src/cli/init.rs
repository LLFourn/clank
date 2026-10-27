//! `clank init` — pure repo setup. Non-interactive, assigns
//! nothing (`teams-based-agent-registration`, lloyd 2026-06-09):
//!
//! - write `.clank/.gitignore`,
//! - remove legacy clank permission rules from an existing
//!   `.claude/settings.local.json` without changing operator rules,
//! - install the `post-rewrite` git hook,
//! - warn if an ancestor ignore rule swallows any tracked `.clank/`
//!   plan artifact path,
//! - write a self-contained repo config: bare init writes an
//!   empty new-shape config; `--team <name>` copies a user-scope
//!   team's composition + referenced agents into it.
//!
//! It does NOT bind a session, read harness session env vars, prompt for an
//! identity, or assign a role. Session binding is `clank as`'s
//! sole job; roles are roster-derived.

use std::path::Path;

use super::InitArgs;

use crate::init_facts::{
    POST_REWRITE_BODY, POST_REWRITE_MARKER, clank_gitignore_body, classify_gitignore_body,
};

/// `clank init` is pure repo setup: scaffold `.clank/`, install
/// the git hook, retire legacy Claude permissions, and optionally
/// pick a team.
/// It does NOT bind sessions, read harness session env vars, prompt for an
/// identity, or assign any role — that's `clank as`'s job. Plan:
/// `teams-based-agent-registration` (lloyd 2026-06-09: init
/// assigns nothing).
pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = repo_for_init(args.repo.as_deref(), &std::env::current_dir()?)?;
    let setup = setup_repo_support(&repo, args.force_hooks)?;
    println!("set up clank in {}: {}", repo.display(), setup.summary());

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match args.team.as_deref() {
        // Explicit `--team <name>`: copy the named user-scope team's
        // composition + its referenced agent descriptions into THIS
        // repo's self-contained config (overwriting any prior team).
        Some(team_name) => {
            let home_ref = home.as_deref().ok_or_else(|| {
                anyhow::anyhow!("$HOME not set; --team requires a user-scope config")
            })?;
            register_repo_team_guarded(home_ref, &repo, team_name, args.force)?;
        }
        None => seed_bare(home.as_deref(), &repo)?,
    }
    Ok(())
}

/// What [`setup_repo_support`] did to each file it manages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportSetup {
    pub gitignore: GitignoreChange,
    pub retired_claude_perms: bool,
    pub hook: HookChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitignoreChange {
    Wrote,
    Upgraded,
    UpToDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    Wrote,
    Refreshed,
    ForceOverwrote,
    KeptForeign,
    NoGitDir,
}

impl SupportSetup {
    /// One clause per file, in the order they were set up.
    pub fn summary(&self) -> String {
        let gitignore = match self.gitignore {
            GitignoreChange::Wrote => "wrote .clank/.gitignore",
            GitignoreChange::Upgraded => "upgraded .clank/.gitignore",
            GitignoreChange::UpToDate => ".clank/.gitignore already up to date",
        };
        let perms = self
            .retired_claude_perms
            .then_some("removed legacy clank permissions from .claude/settings.local.json");
        let hook = match self.hook {
            HookChange::Wrote => "installed the post-rewrite hook",
            HookChange::Refreshed => "the post-rewrite hook is up to date",
            HookChange::ForceOverwrote => "force-overwrote the post-rewrite hook",
            HookChange::KeptForeign => "kept your existing post-rewrite hook",
            HookChange::NoGitDir => "no git directory for the post-rewrite hook",
        };
        [Some(gitignore), perms, Some(hook)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The repo support `clank init` and a repo's first `clank agent add`
/// share: the `.clank/` scaffold and its `.gitignore`, the legacy
/// Claude permission cleanup, and the `post-rewrite` hook. It never
/// touches the roster. An unmanaged `.clank/.gitignore` is left as it is
/// and fails setup; a foreign hook is kept, with a warning, unless
/// `force_hooks`.
pub fn setup_repo_support(repo: &Path, force_hooks: bool) -> anyhow::Result<SupportSetup> {
    let gitignore = write_scaffold(repo)?;
    let retired_claude_perms = migrate_claude_perms(repo)?;
    let hook = write_post_rewrite_hook(repo, force_hooks)?;
    warn_if_globally_excluded(repo);
    Ok(SupportSetup {
        gitignore,
        retired_claude_perms,
        hook,
    })
}

/// Repo support for a command about to write `.clank/config.json`: when
/// the file does not exist yet, [`setup_repo_support`] without forcing
/// the hook, announced. `None` when the repo already has a config.
pub fn support_before_first_config(repo: &Path) -> anyhow::Result<Option<SupportSetup>> {
    let config = repo.join(".clank/config.json");
    if config
        .try_exists()
        .map_err(|e| anyhow::anyhow!("checking {}: {e}", config.display()))?
    {
        return Ok(None);
    }
    let setup = setup_repo_support(repo, false)?;
    eprintln!(
        "initialized clank in {}: {}",
        repo.display(),
        setup.summary()
    );
    Ok(Some(setup))
}

/// The repository `clank init` sets up: the one containing the selected
/// directory (`explicit`, else `cwd`), or, when discovery proves there
/// is none, a new one created there. Init only: every other command
/// still requires an existing repository.
pub fn repo_for_init(explicit: Option<&Path>, cwd: &Path) -> anyhow::Result<std::path::PathBuf> {
    use crate::git_io::Discovery;
    let selected = explicit.unwrap_or(cwd);
    let dir = dunce::canonicalize(selected)
        .map_err(|e| anyhow::anyhow!("`{}`: {e}", selected.display()))?;
    anyhow::ensure!(dir.is_dir(), "`{}` is not a directory", dir.display());
    match crate::git_io::discover(&dir) {
        Discovery::WorkTree(root) => Ok(dunce::canonicalize(root)?),
        Discovery::Absent => {
            crate::git_plumbing::init_repo(&dir)?;
            eprintln!("created a git repository in {}", dir.display());
            Ok(dir)
        }
        Discovery::Bare(git_dir) => anyhow::bail!(
            "`{}` is inside a bare repository ({}), which has no work tree for clank",
            dir.display(),
            git_dir.display()
        ),
        Discovery::Failed(why) => anyhow::bail!(
            "could not tell whether `{}` is inside a git repository ({why}); nothing was created",
            dir.display()
        ),
    }
}

/// The team bare `clank init` seeds a fresh repo from, when the user
/// has declared one by this name.
const DEFAULT_TEAM: &str = "default";

/// Bare `clank init`: a repo with a valid roster keeps it, checked
/// before any user config is read. Otherwise the user's `default` team
/// seeds it, or, with no such team, an empty roster that needs no
/// template.
pub fn seed_bare(home: Option<&Path>, repo: &Path) -> anyhow::Result<()> {
    if repo_has_team(repo)? {
        return Ok(());
    }
    match home {
        Some(home) if user_team_declared(home, DEFAULT_TEAM)? => {
            register_repo_team(home, repo, DEFAULT_TEAM)?;
            eprintln!("seeded the roster from your `{DEFAULT_TEAM}` team");
            Ok(())
        }
        _ => bootstrap_empty_repo_config(repo),
    }
}

/// Whether the user config declares team `name`. An absent config is
/// no team; an unreadable one is an error, never an empty roster.
fn user_team_declared(home: &Path, name: &str) -> anyhow::Result<bool> {
    let path = home.join(".clank/config.json");
    let raw: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| {
            anyhow::anyhow!("user-scope config {} is unreadable ({e})", path.display())
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading {}", path.display())));
        }
    };
    Ok(raw.get("teams").and_then(|t| t.get(name)).is_some())
}

/// Write a valid empty new-shape [`RepoConfigFile`] (an empty
/// roster). Standalone — needs no user-scope template. The repo
/// then has a parseable config; the user adds agents via
/// `clank agent add <label> --tool <claude|codex|grok|opencode>`, the
/// first of which becomes master.
///
/// Called only when there's no VALID new-shape config (the
/// `repo_has_team` guard already ran), so an OLD-shape config is
/// overwritten here — this is the "re-run `clank init` to
/// recreate it" recovery path the loader error points users at.
fn bootstrap_empty_repo_config(repo: &Path) -> anyhow::Result<()> {
    use crate::cli::teams_config::RepoConfigFile;
    let repo_cfg_path = repo.join(".clank/config.json");
    write_repo_config(&repo_cfg_path, &RepoConfigFile::default())?;
    eprintln!(
        "wrote empty roster to {} — add agents with \
         `clank agent add <label> --tool <claude|codex|grok|opencode>`; the first becomes \
         master (seed a whole team instead by declaring a `{DEFAULT_TEAM}` team, or with \
         `clank init --team <name>`)",
        repo_cfg_path.display()
    );
    Ok(())
}

/// Atomic write of a typed config via tempfile + rename.
fn write_repo_config<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// True iff `<repo>/.clank/config.json` already exists and parses
/// as a new-shape [`RepoConfigFile`] (a roster). Keeps bare
/// `clank init` from clobbering a VALID existing repo config on
/// re-init (the roster lives in this file). Missing config OR an
/// OLD-shape config → false, so bare init writes/recreates a
/// fresh empty config (the documented recovery path).
fn repo_has_team(repo: &Path) -> anyhow::Result<bool> {
    let path = repo.join(".clank/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(serde_json::from_str::<crate::cli::teams_config::RepoConfigFile>(&s).is_ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Install the `post-rewrite` git hook so feedback files
/// follow commits through rebases/amends. Resolves the real
/// hook path via the shared (common) gitdir so this works in
/// linked worktrees too (where `.git` is a file pointing at
/// the worktree's gitdir; hooks live in the main repo's
/// shared `.git/hooks/`). If a foreign hook exists, print a
/// warning and leave it alone unless `force` is set.
fn write_post_rewrite_hook(repo: &Path, force: bool) -> anyhow::Result<HookChange> {
    let Some(hook_path) = resolve_hook_path(repo, "post-rewrite") else {
        return Ok(HookChange::NoGitDir);
    };
    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating hooks dir `{}`: {e}", parent.display()))?;
    }
    Ok(match std::fs::read_to_string(&hook_path) {
        Ok(existing) if existing.contains(POST_REWRITE_MARKER) => {
            // Already ours; refresh body in case it changed.
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            HookChange::Refreshed
        }
        Ok(_) if force => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            HookChange::ForceOverwrote
        }
        Ok(_) => {
            eprintln!(
                "warning: {} already exists with non-clank content; \
                 leaving it alone. Pass `clank init --force-hooks` to overwrite, \
                 or chain `clank rewire --from-stdin` into it manually.",
                hook_path.display()
            );
            HookChange::KeptForeign
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            HookChange::Wrote
        }
        Err(e) => return Err(e.into()),
    })
}

/// Resolve `<git-common-dir>/hooks/<name>` via gix's common dir —
/// shared across linked worktrees, so the hook lands in the main
/// repo's `hooks/`.
fn resolve_hook_path(repo: &Path, name: &str) -> Option<std::path::PathBuf> {
    // `hooks/` is SHARED across linked worktrees → the common dir.
    Some(
        crate::git_io::common_dir(repo)
            .ok()?
            .join(format!("hooks/{name}")),
    )
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn write_scaffold(repo: &Path) -> anyhow::Result<GitignoreChange> {
    let clank_dir = repo.join(".clank");
    let plans_dir = clank_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)?;

    let gitignore = clank_dir.join(".gitignore");
    // Exact allow-list model: `/*` catches every local-only `.clank/`
    // path, and only the explicit `!` entries below are tracked.
    // Order matters, so a canonical file must match byte-for-byte.
    Ok(match std::fs::read_to_string(&gitignore) {
        Ok(existing) => match classify_gitignore_body(&existing) {
            crate::init_facts::GitignoreState::Canonical => GitignoreChange::UpToDate,
            crate::init_facts::GitignoreState::Legacy => {
                // The old per-dir deny list — replace wholesale with the
                // canonical allow-list (a Legacy body is only the recognized
                // managed entries, so nothing user-authored is lost).
                std::fs::write(&gitignore, clank_gitignore_body())?;
                GitignoreChange::Upgraded
            }
            _ => {
                anyhow::bail!(
                    "{} contains unmanaged content; refusing to overwrite. \
                     Inspect it, delete it, or reduce it to the managed entries:\n{}",
                    gitignore.display(),
                    clank_gitignore_body()
                );
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&gitignore, clank_gitignore_body())?;
            GitignoreChange::Wrote
        }
        Err(e) => return Err(e.into()),
    })
}

/// Tracked `.clank/` paths probed for ancestor-ignore warnings.
const TRACKED_PROBES: &[&str] = &[".clank/plans", ".clank/finished"];

/// Warn (but don't fail) if some ancestor `.gitignore` or
/// `core.excludesFile` excludes paths that need to be tracked.
///
/// Probes only tracked plan-artifact paths. Local-only state such as
/// `.clank/config.json` and `.clank/agents/**` is intentionally omitted:
/// an ancestor ignore rule for those paths is fine and must not warn as
/// if a tracked file were hidden. Parses
/// `git check-ignore -v`'s structured output:
///   `<source_file>:<line>:<pattern>\t<probed_path>`
/// We suppress only when `<source_file>` is the `.clank/.gitignore`
/// we just wrote. Substring matching on the whole record would be
/// fooled by paths or patterns that happen to contain that literal.
fn warn_if_globally_excluded(repo: &Path) {
    for rel in TRACKED_PROBES {
        check_one(repo, rel);
    }
}

fn check_one(repo: &Path, rel: &str) {
    let Some(line) = crate::git_io::check_ignore(repo, rel) else {
        return;
    };
    if line.is_empty() || matched_by_clank_gitignore(&line) {
        return;
    }
    eprintln!(
        "warning: an ancestor .gitignore (or core.excludesFile) excludes {rel} — \
         tracked clank files would be hidden. Source:\n  {line}"
    );
}

/// The three approvals older `clank init` versions wrote into every
/// repo. They remain here only as exact migration matches.
const LEGACY_CLAUDE_ALLOW_RULES: &[&str] = &[
    "Write(.clank/agents/**)",
    "Edit(.clank/agents/**)",
    "Read(.clank/agents/**)",
];

/// Retire clank's blanket Claude approvals on re-init. An absent or
/// unparsable operator file is untouched; a file with no matching
/// rules is byte-identical; a file clank alone wrote is removed.
fn migrate_claude_perms(repo: &Path) -> anyhow::Result<bool> {
    let path = repo.join(".claude/settings.local.json");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            eprintln!(
                "warning: cannot read {} for legacy permission cleanup: {e}",
                path.display()
            );
            return Ok(false);
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(e) => {
            eprintln!(
                "warning: cannot parse {} for legacy permission cleanup: {e}",
                path.display()
            );
            return Ok(false);
        }
    };
    let Some(allow) = value
        .get_mut("permissions")
        .and_then(|p| p.get_mut("allow"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(false);
    };
    let before = allow.len();
    allow.retain(|entry| {
        !entry
            .as_str()
            .is_some_and(|s| LEGACY_CLAUDE_ALLOW_RULES.contains(&s))
    });
    if allow.len() == before {
        return Ok(false);
    }
    let only_empty_permissions = value.as_object().is_some_and(|obj| {
        obj.len() == 1
            && obj
                .get("permissions")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|p| {
                    p.len() == 1
                        && p.get("allow")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(Vec::is_empty)
                })
    });
    if only_empty_permissions {
        std::fs::remove_file(&path)?;
    } else {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&value)?),
        )?;
    }
    Ok(true)
}

/// True if the matched rule lives in `<repo>/.clank/.gitignore`
/// (the file we just wrote). `git check-ignore -v` emits records as
/// `<source_file>:<line>:<pattern>\t<probed>`; we parse the
/// `source_file` column and check it's our managed file.
fn matched_by_clank_gitignore(record: &str) -> bool {
    let (source_part, _probed) = match record.split_once('\t') {
        Some(parts) => parts,
        None => return false,
    };
    let source_file = match source_part.split(':').next() {
        Some(s) => s,
        None => return false,
    };
    let p = Path::new(source_file);
    p.file_name().is_some_and(|n| n == ".gitignore")
        && p.parent().is_some_and(|parent| parent.ends_with(".clank"))
}

/// `clank init --team`'s entry point: apply the overwrite guard,
/// then copy the team roster down. Refuses to clobber a repo that
/// already has a NON-EMPTY roster unless `force`. The check reads
/// through [`crate::agent_store::repo_config_if_valid`], which
/// ignores the legacy shape — an old-shape config is the
/// documented "re-run `clank init`" recovery path, so it's allowed
/// to be overwritten. A freshly-bootstrapped empty roster is not
/// "configured", so the first `--team` after a bare `clank init`
/// is unaffected. `pub`, env-free (explicit `home`/`repo`).
pub fn register_repo_team_guarded(
    home: &Path,
    repo: &Path,
    team_name: &str,
    force: bool,
) -> anyhow::Result<()> {
    let configured =
        crate::agent_store::repo_config_if_valid(repo)?.is_some_and(|c| !c.agents.is_empty());
    if configured && !force {
        anyhow::bail!(
            "repo already has a roster; run `clank team save {team_name}` first to keep \
             local edits, or pass --force to replace"
        );
    }
    register_repo_team(home, repo, team_name)
}

/// Seed a repo by RESOLVING the named user-scope team TEMPLATE (a
/// [`TeamRoster`]) into `<repo>/.clank/config.json`'s `agents` — a
/// concrete [`Roster`] of full inline agents, with references
/// resolved against the library. `pub`, env-free core (takes explicit
/// `home`/`repo`) — both `clank init --team` and integration-test
/// setup call it.
///
/// Validations / fail-closed:
/// - The named team must exist in user-scope
///   `~/.clank/config.json#/teams`.
/// - A malformed team member errors naming the member and the valid
///   shapes; a dangling reference errors naming the missing library
///   agent.
/// - An existing VALID repo config round-trips through the new
///   schema's `extra` flatten catchall, so unknown fields
///   (review/hooks/diff) are preserved across the swap.
pub fn register_repo_team(home: &Path, repo: &Path, team_name: &str) -> anyhow::Result<()> {
    use crate::cli::teams_config::RepoConfigFile;

    let roster = load_user_team_roster(home, team_name)?;

    let repo_cfg_path = repo.join(".clank/config.json");
    let mut repo_cfg: RepoConfigFile = match std::fs::read_to_string(&repo_cfg_path) {
        // A VALID new-shape config: keep it so its `extra`
        // (review/hooks/diff) survives the roster swap. A LEGACY or
        // unparseable config: this is the recreate path (`init --team`
        // overwriting old shape, which the guard already permits) —
        // start from default rather than parse-erroring.
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RepoConfigFile::default(),
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("reading {}", repo_cfg_path.display()))
            );
        }
    };
    repo_cfg.agents = roster;

    write_repo_config(&repo_cfg_path, &repo_cfg)?;
    eprintln!(
        "wrote team `{team_name}` roster to {}",
        repo_cfg_path.display()
    );
    Ok(())
}

/// Load a named team template from user-scope
/// `~/.clank/config.json#/teams/<name>` and RESOLVE it into a
/// concrete [`Roster`] of full inline agents. References
/// ([`TeamMember::Ref`]) are resolved against the file's `agents`
/// library here, so the repo receives a self-contained roster —
/// references never reach a repo config.
///
/// Reads the global config as raw JSON and deserializes only the
/// requested team's value (plus the library), so an UNRELATED
/// malformed team can't block this one. Fail-closed: a missing team
/// errors with a compose-it hint; a malformed member errors naming
/// the member and valid shapes; a dangling reference errors naming
/// the missing library agent.
pub(crate) fn load_user_team_roster(
    home: &Path,
    team_name: &str,
) -> anyhow::Result<crate::cli::teams_config::Roster> {
    use crate::cli::teams_config::{AgentDescription, TeamRoster, resolve_team};
    use clank_core::ids::AgentLabel;
    use std::collections::BTreeMap;

    let user_path = home.join(".clank/config.json");
    let raw: serde_json::Value = match std::fs::read_to_string(&user_path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| {
            anyhow::anyhow!(
                "user-scope config {} is unreadable ({e}); fix or remove it \
                 (a fresh one is written by `clank team save`)",
                user_path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading {}", user_path.display())));
        }
    };

    let team_value = raw.get("teams").and_then(|t| t.get(team_name));
    let Some(team_value) = team_value else {
        anyhow::bail!(
            "team `{team_name}` not declared in user-scope teams \
             (`~/.clank/config.json#/teams`). Either run bare `clank init` and build a \
             roster locally (`clank agent add`; the first becomes master), or — to make \
             `{team_name}` a reusable template — build it in a repo and publish with \
             `clank team save {team_name}`."
        );
    };

    let team: TeamRoster = serde_json::from_value(team_value.clone())
        .map_err(|e| team_parse_error(team_name, team_value, e))?;

    let library: BTreeMap<AgentLabel, AgentDescription> = match raw.get("agents") {
        Some(agents) => serde_json::from_value(agents.clone()).map_err(|e| {
            anyhow::anyhow!(
                "user-scope `agents` library is malformed ({e}); fix `{}`",
                user_path.display()
            )
        })?,
        None => BTreeMap::new(),
    };

    resolve_team(&team, &library).map_err(anyhow::Error::from)
}

/// Turn a failed [`TeamRoster`] deserialize into an actionable error.
/// The untagged `TeamMember` error is generic ("data did not match
/// any variant"), so locate the offending member and spell out the
/// valid member shapes.
fn team_parse_error(
    team_name: &str,
    team_value: &serde_json::Value,
    err: serde_json::Error,
) -> anyhow::Error {
    use crate::cli::teams_config::TeamMember;

    if let Some(obj) = team_value.as_object() {
        for (member, value) in obj {
            if let Err(member_err) = serde_json::from_value::<TeamMember>(value.clone()) {
                return anyhow::anyhow!(
                    "team `{team_name}` member `{member}` in user-scope `teams` is malformed \
                     ({member_err}); a member must be a reference \
                     (`{{\"role\": \"<master|commit|gate>\", \"agent\"?: \"<library agent>\"}}`) \
                     or a full inline definition (`{{\"tool\": \"<claude|codex|grok|opencode>\", \"role\": ...}}`)"
                );
            }
        }
    }

    anyhow::Error::from(err).context(format!("parsing team `{team_name}` in user-scope `teams`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
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
    fn writes_scaffold_creates_plans_dir_and_gitignore() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        assert!(dir.path().join(".clank/plans").is_dir());
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, clank_gitignore_body());
        // The allow-list: ignore everything except plans/finished + self.
        let entries: Vec<&str> = body.lines().collect();
        assert_eq!(
            entries,
            crate::init_facts::CLANK_GITIGNORE_ENTRIES,
            "managed allow-list entries are the complete tracked surface"
        );
        assert!(
            !entries.iter().any(|e| e.contains("config.json")),
            ".clank/config.json is local-only, not an allow-list carve-out"
        );
    }

    #[test]
    fn writes_scaffold_is_idempotent_when_content_matches() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, clank_gitignore_body());
    }

    #[test]
    fn writes_scaffold_rewrites_a_legacy_body_to_the_allow_list() {
        // A pre-allow-list per-dir deny body classifies Legacy and is
        // REPLACED wholesale with the canonical allow-list (not appended).
        let dir = init_repo();
        let gitignore = dir.path().join(".clank/.gitignore");
        std::fs::create_dir_all(gitignore.parent().unwrap()).unwrap();
        std::fs::write(&gitignore, "/cache/\n/agents/\n").unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(&gitignore).unwrap();
        assert_eq!(body, clank_gitignore_body(), "rewritten to the allow-list");
        assert_eq!(
            classify_gitignore_body(&body),
            crate::init_facts::GitignoreState::Canonical
        );
    }

    #[test]
    fn writes_scaffold_migrates_an_older_legacy_body() {
        // A repo whose `.clank/.gitignore` predates the allow-list (a partial
        // legacy per-dir body) classifies Legacy, and `clank init` migrates
        // it to the allow-list — it must repair, not bail.
        let dir = init_repo();
        let gitignore = dir.path().join(".clank/.gitignore");
        std::fs::create_dir_all(gitignore.parent().unwrap()).unwrap();
        std::fs::write(
            &gitignore,
            "/agents/\n/cache/\n/feedback/\n/queue/\n/html/\n",
        )
        .unwrap();
        write_scaffold(dir.path()).expect("init must repair, not bail");
        let body = std::fs::read_to_string(&gitignore).unwrap();
        assert_eq!(body, clank_gitignore_body(), "migrated to the allow-list");
        assert_eq!(
            classify_gitignore_body(&body),
            crate::init_facts::GitignoreState::Canonical
        );
    }

    #[test]
    fn writes_scaffold_bails_on_foreign_content() {
        // Unmanaged lines (incl. truly ancient unanchored
        // spellings) are user content we refuse to clobber.
        let dir = init_repo();
        let gitignore = dir.path().join(".clank/.gitignore");
        std::fs::create_dir_all(gitignore.parent().unwrap()).unwrap();
        std::fs::write(&gitignore, "feedback/\ncache/\n").unwrap();
        assert!(write_scaffold(dir.path()).is_err());
    }

    #[test]
    fn matched_by_clank_gitignore_recognises_our_managed_file() {
        // Standard git check-ignore -v output: <source>:<line>:<pattern>\t<probed>.
        let record = ".clank/.gitignore:1:plans/extra\t.clank/plans/extra/foo";
        assert!(matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_ancestor_gitignore() {
        // Pattern column mentions the literal `.clank/.gitignore`
        // but the matching rule is in the repo-root .gitignore.
        let record = ".gitignore:5:!.clank/.gitignore\t.clank/.gitignore";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_unrelated_source() {
        let record = "../.gitignore:2:.clank/\t.clank/plans";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_missing_tab() {
        assert!(!matched_by_clank_gitignore(""));
        assert!(!matched_by_clank_gitignore(".clank/.gitignore:1:plans/"));
    }

    #[tokio::test]
    async fn fresh_init_writes_no_claude_permission_file() {
        let dir = init_repo();
        run(InitArgs {
            repo: Some(dir.path().to_path_buf()),
            force_hooks: false,
            force: false,
            team: None,
        })
        .await
        .unwrap();
        assert!(!dir.path().join(".claude/settings.local.json").exists());
    }

    #[test]
    fn migration_removes_a_file_containing_only_clank_rules() {
        let dir = init_repo();
        let path = dir.path().join(".claude/settings.local.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"permissions":{"allow":["Write(.clank/agents/**)","Edit(.clank/agents/**)","Read(.clank/agents/**)"]}}"#,
        )
        .unwrap();
        migrate_claude_perms(dir.path()).unwrap();
        assert!(!path.exists(), "clank-only settings file should disappear");
        migrate_claude_perms(dir.path()).unwrap();
        assert!(!path.exists(), "migration is idempotent");
    }

    #[test]
    fn migration_preserves_unrelated_settings_and_rules() {
        let dir = init_repo();
        let path = dir.path().join(".claude/settings.local.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"theme":"operator","permissions":{"deny":["Bash(rm *)"],"allow":["Bash(ls *)","Read(.clank/agents/**)","Edit(.clank/agents/**)","Write(.clank/agents/**)","Bash(git *)"]}}"#,
        )
        .unwrap();
        migrate_claude_perms(dir.path()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(value["theme"], "operator");
        assert_eq!(
            value["permissions"]["deny"],
            serde_json::json!(["Bash(rm *)"])
        );
        assert_eq!(
            value["permissions"]["allow"],
            serde_json::json!(["Bash(ls *)", "Bash(git *)"]),
        );
        migrate_claude_perms(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
    }

    #[test]
    fn migration_leaves_operator_and_unparseable_files_byte_identical() {
        let dir = init_repo();
        let path = dir.path().join(".claude/settings.local.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for body in [
            " { \"permissions\": { \"allow\": [\"Bash(ls *)\"] } }\n",
            "not json\n",
        ] {
            std::fs::write(&path, body).unwrap();
            migrate_claude_perms(dir.path()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn tracked_probes_are_only_tracked_plan_artifacts() {
        assert_eq!(
            TRACKED_PROBES,
            &[".clank/plans", ".clank/finished"],
            "ancestor-ignore warnings must not probe local-only .clank state"
        );
    }

    /// Check-ignore matrix for the managed `.clank/.gitignore` allow-list:
    /// only `plans/`, `finished/`, and the managed ignore file itself are
    /// tracked. Everything else under `.clank/` is per-user / local.
    #[test]
    fn check_ignore_matrix_simplified() {
        let dir = init_repo();
        std::fs::write(dir.path().join(".gitignore"), "/target/\n").unwrap();
        write_scaffold(dir.path()).unwrap();

        let probe = |rel: &str| -> bool {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["check-ignore", rel])
                .output()
                .unwrap();
            // exit 0 = ignored; 1 = tracked.
            out.status.code() == Some(0)
        };

        // Tracked.
        assert!(!probe(".clank/plans/foo.md"));
        assert!(!probe(".clank/finished/foo/seal.md"));
        assert!(!probe(".clank/.gitignore"));
        // Per-user / local.
        assert!(probe(".clank/config.json"));
        assert!(probe(".clank/agents/alice/config.json"));
        assert!(probe(".clank/agents/alice/feedback/foo/abc.md"));
        assert!(probe(".clank/cache/anything"));
        assert!(probe(".clank/drafts/foo.md"));
        assert!(probe(".clank/queue/500-foo.md"));
        assert!(probe(".clank/html/foo.html"));
        assert!(probe(".clank/worktrees/foo/.clank/config.json"));
    }

    #[test]
    fn writes_post_rewrite_hook_when_absent() {
        let dir = init_repo();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".git/hooks/post-rewrite")).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
        assert!(body.contains("clank rewire --from-stdin"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(".git/hooks/post-rewrite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "hook should be executable");
        }
    }

    #[test]
    fn writes_post_rewrite_hook_warns_on_foreign_content() {
        let dir = init_repo();
        let hook_path = dir.path().join(".git/hooks/post-rewrite");
        std::fs::write(&hook_path, "#!/bin/sh\necho not clank\n").unwrap();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(&hook_path).unwrap();
        assert!(
            !body.contains(POST_REWRITE_MARKER),
            "foreign hook must NOT be overwritten without --force-hooks; body={body}"
        );
    }

    #[test]
    fn force_hooks_overwrites_foreign_content() {
        let dir = init_repo();
        let hook_path = dir.path().join(".git/hooks/post-rewrite");
        std::fs::write(&hook_path, "#!/bin/sh\necho not clank\n").unwrap();
        write_post_rewrite_hook(dir.path(), true).unwrap();
        let body = std::fs::read_to_string(&hook_path).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
    }

    #[test]
    fn writes_post_rewrite_hook_idempotent_on_our_marker() {
        let dir = init_repo();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        write_post_rewrite_hook(dir.path(), false).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".git/hooks/post-rewrite")).unwrap();
        assert!(body.contains(POST_REWRITE_MARKER));
        assert_eq!(body, POST_REWRITE_BODY);
    }

    // ── Plan: repo-agents-no-team ──

    /// Seed a user-scope config with a by-name library plus team
    /// templates. Teams reference the library; resolution happens at
    /// `register_repo_team` (init --team) time.
    fn seed_user_config(
        home: &Path,
        library: &[(&str, clank_core::vocab::Tool)],
        teams: &[(&str, crate::cli::teams_config::TeamRoster)],
    ) {
        use crate::cli::teams_config::{AgentDescription, UserConfigFile};
        use clank_core::ids::AgentLabel;
        let mut cfg = UserConfigFile::default();
        for (label, tool) in library {
            cfg.agents.insert(
                AgentLabel::parse(label).unwrap(),
                AgentDescription {
                    tool: *tool,
                    launch: None,
                    initial_prompt: None,
                },
            );
        }
        for (name, team) in teams {
            cfg.teams.insert(name.to_string(), team.clone());
        }
        let path = home.join(".clank/config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    }

    fn team_ref(
        role: crate::cli::teams_config::RosterRole,
    ) -> crate::cli::teams_config::TeamMember {
        crate::cli::teams_config::TeamMember::Ref(crate::cli::teams_config::TeamRef {
            agent: None,
            role,
        })
    }

    #[test]
    fn register_repo_team_resolves_refs_into_inline_roster() {
        // `--team dev` resolves dev's REFERENCES against the library
        // into the repo's `agents` as full inline agents-with-roles.
        use crate::cli::teams_config::{RepoConfigFile, RosterRole, TeamRoster};
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;
        let user_home = tempfile::tempdir().unwrap();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(
            AgentLabel::parse("claude").unwrap(),
            team_ref(RosterRole::Master),
        );
        dev.insert(
            AgentLabel::parse("codex").unwrap(),
            team_ref(RosterRole::Commit),
        );
        seed_user_config(
            user_home.path(),
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("dev", dev)],
        );

        let repo = init_repo();
        register_repo_team(user_home.path(), repo.path(), "dev").unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        // Parsing as RepoConfigFile (whose agents REQUIRE `tool`)
        // proves the repo received full inline agents, not refs.
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.agents.len(), 2);
        let claude = &parsed.agents[&AgentLabel::parse("claude").unwrap()];
        assert_eq!(claude.role, RosterRole::Master);
        assert_eq!(claude.tool, Tool::Claude);
        assert_eq!(
            parsed.agents[&AgentLabel::parse("codex").unwrap()].role,
            RosterRole::Commit
        );
    }

    #[test]
    fn register_repo_team_dangling_ref_errors() {
        // A team referencing a library agent that doesn't exist fails
        // closed with an actionable message at consume time.
        use crate::cli::teams_config::{RosterRole, TeamRoster};
        use clank_core::ids::AgentLabel;
        let home = tempfile::tempdir().unwrap();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(
            AgentLabel::parse("ghost").unwrap(),
            team_ref(RosterRole::Master),
        );
        seed_user_config(home.path(), &[], &[("dev", dev)]);

        let repo = init_repo();
        let err = register_repo_team(home.path(), repo.path(), "dev").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ghost") && msg.contains("not in"),
            "expected dangling-ref hint; got: {msg}"
        );
    }

    #[test]
    fn register_repo_team_then_save_round_trips() {
        // init --team resolves refs down into the repo; team save
        // writes them back up as refs; the stored team is unchanged.
        use crate::cli::teams_config::{RosterRole, TeamRoster};
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;
        let home = tempfile::tempdir().unwrap();
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(
            AgentLabel::parse("claude").unwrap(),
            team_ref(RosterRole::Master),
        );
        dev.insert(
            AgentLabel::parse("codex").unwrap(),
            team_ref(RosterRole::Commit),
        );
        seed_user_config(
            home.path(),
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("dev", dev.clone())],
        );

        let repo = init_repo();
        register_repo_team(home.path(), repo.path(), "dev").unwrap();
        // Save under a new name and compare the stored team.
        crate::cli::team::save_team(home.path(), repo.path(), "dev2", false).unwrap();
        let user_cfg = crate::cli::team::read_user_config(home.path()).unwrap();
        assert_eq!(user_cfg.teams.get("dev2").unwrap(), &dev);
    }

    #[test]
    fn init_team_repo_config_identical_for_ref_and_inline_teams() {
        // HARD INVARIANT: a team saved as refs and the same team saved
        // inline must produce a BYTE-IDENTICAL repo config — refs
        // resolve entirely in the home-config layer.
        use crate::cli::teams_config::{RosterAgent, RosterRole, TeamMember, TeamRoster};
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;

        let ref_home = tempfile::tempdir().unwrap();
        let mut ref_team: TeamRoster = std::collections::BTreeMap::new();
        ref_team.insert(
            AgentLabel::parse("claude").unwrap(),
            team_ref(RosterRole::Master),
        );
        ref_team.insert(
            AgentLabel::parse("codex").unwrap(),
            team_ref(RosterRole::Commit),
        );
        seed_user_config(
            ref_home.path(),
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("dev", ref_team)],
        );
        let ref_repo = init_repo();
        register_repo_team(ref_home.path(), ref_repo.path(), "dev").unwrap();
        let ref_body = std::fs::read_to_string(ref_repo.path().join(".clank/config.json")).unwrap();

        let inline_home = tempfile::tempdir().unwrap();
        let inline = |tool, role| {
            TeamMember::Inline(RosterAgent {
                tool,
                launch: None,
                initial_prompt: None,
                role,
            })
        };
        let mut inline_team: TeamRoster = std::collections::BTreeMap::new();
        inline_team.insert(
            AgentLabel::parse("claude").unwrap(),
            inline(Tool::Claude, RosterRole::Master),
        );
        inline_team.insert(
            AgentLabel::parse("codex").unwrap(),
            inline(Tool::Codex, RosterRole::Commit),
        );
        seed_user_config(inline_home.path(), &[], &[("dev", inline_team)]);
        let inline_repo = init_repo();
        register_repo_team(inline_home.path(), inline_repo.path(), "dev").unwrap();
        let inline_body =
            std::fs::read_to_string(inline_repo.path().join(".clank/config.json")).unwrap();

        assert_eq!(
            ref_body, inline_body,
            "ref-based and inline-based teams must produce an identical repo config"
        );
    }

    #[test]
    fn bare_init_writes_empty_valid_repo_config() {
        // From-scratch path: bare init must produce a VALID
        // new-shape config (empty roster) with no user-scope template.
        use crate::cli::teams_config::{RepoConfigFile, ResolutionError, resolve_registered_set};
        let repo = init_repo();
        bootstrap_empty_repo_config(repo.path()).unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(parsed.agents.is_empty());
        // It resolves to NoMaster (the expected bootstrapped state).
        assert!(matches!(
            resolve_registered_set(&parsed).unwrap_err(),
            ResolutionError::NoMaster
        ));
    }

    #[test]
    fn repo_has_team_false_for_old_shape_so_bare_init_recreates() {
        // An old-shape config (the just-shipped `{agents, team}`
        // shape) is NOT a valid new-shape config → repo_has_team is
        // false, so bare init overwrites it with a fresh empty config
        // (the documented "re-run `clank init`" recovery path).
        use crate::cli::teams_config::RepoConfigFile;
        let repo = init_repo();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude"}},"team":{"master":"claude"}}"#,
        )
        .unwrap();
        assert!(!repo_has_team(repo.path()).unwrap());
        bootstrap_empty_repo_config(repo.path()).unwrap();
        let body = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(&body).unwrap();
        assert!(parsed.agents.is_empty());
    }

    #[test]
    fn register_repo_team_rejects_unknown_team() {
        let user_home = tempfile::tempdir().unwrap();
        seed_user_config(
            user_home.path(),
            &[],
            &[("dev", std::collections::BTreeMap::new())],
        );
        let repo = init_repo();
        let err = register_repo_team(user_home.path(), repo.path(), "nonexistent").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not declared"));
        assert!(msg.contains("clank team save"));
    }

    #[test]
    fn register_repo_team_malformed_member_names_member() {
        // A team member that's neither a valid ref nor inline (here a
        // mistyped `agnt` field) errors naming the offending member
        // and the valid shapes.
        let user_home = tempfile::tempdir().unwrap();
        let upath = user_home.path().join(".clank/config.json");
        std::fs::create_dir_all(upath.parent().unwrap()).unwrap();
        std::fs::write(
            &upath,
            r#"{"agents":{"codex":{"tool":"codex"}},"teams":{"dev":{"codex":{"agnt":"codex","role":"commit"}}}}"#,
        )
        .unwrap();
        let repo = init_repo();
        let err = register_repo_team(user_home.path(), repo.path(), "dev").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("codex") && msg.contains("malformed"),
            "expected the offending member named; got: {msg}"
        );
    }

    #[test]
    fn writes_scaffold_refuses_on_drifted_content() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/.gitignore"), "something/else\n").unwrap();
        let err = write_scaffold(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unmanaged content"), "unexpected error: {msg}");
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(
            body, "something/else\n",
            "drifted file must not be overwritten"
        );
    }

    // ── clank init --team overwrite guard (M3) ───────────────

    fn seed_user_team_dev(home: &Path) {
        use crate::cli::teams_config::{RosterRole, TeamRoster};
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;
        let mut dev: TeamRoster = std::collections::BTreeMap::new();
        dev.insert(
            AgentLabel::parse("claude").unwrap(),
            team_ref(RosterRole::Master),
        );
        seed_user_config(home, &[("claude", Tool::Claude)], &[("dev", dev)]);
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn init_outside_any_repo_creates_one_there_with_no_commit_or_remote() {
        for explicit in [false, true] {
            let outside = tempfile::tempdir().unwrap();
            let dir = dunce::canonicalize(outside.path()).unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let (arg, cwd) = match explicit {
                true => (Some(dir.as_path()), elsewhere.path()),
                false => (None, dir.as_path()),
            };
            let repo = repo_for_init(arg, cwd).unwrap();
            assert_eq!(repo, dir);
            let top = git(&repo, &["rev-parse", "--show-toplevel"]);
            assert_eq!(
                dunce::canonicalize(String::from_utf8(top.stdout).unwrap().trim()).unwrap(),
                dir
            );
            assert!(
                !git(&repo, &["rev-parse", "--verify", "HEAD"])
                    .status
                    .success()
            );
            assert!(git(&repo, &["remote"]).stdout.is_empty());
            write_scaffold(&repo).unwrap();
            write_post_rewrite_hook(&repo, false).unwrap();
            assert!(repo.join(".clank/.gitignore").is_file());
            assert!(repo.join(".git/hooks/post-rewrite").is_file());
        }
    }

    #[test]
    fn init_inside_a_repo_or_worktree_uses_it_and_creates_nothing() {
        let main = init_repo();
        let root = dunce::canonicalize(main.path()).unwrap();
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(repo_for_init(Some(&sub), Path::new("/")).unwrap(), root);
        assert_eq!(repo_for_init(None, &sub).unwrap(), root);
        assert!(!sub.join(".git").exists());

        assert!(
            git(&root, &["commit", "--allow-empty", "-qm", "base"])
                .status
                .success()
        );
        let linked = tempfile::tempdir().unwrap();
        let wt = linked.path().join("wt");
        assert!(
            git(&root, &["worktree", "add", "-q", wt.to_str().unwrap()])
                .status
                .success()
        );
        let wt = dunce::canonicalize(&wt).unwrap();
        assert_eq!(repo_for_init(None, &wt).unwrap(), wt);
        assert!(wt.join(".git").is_file(), "still a linked worktree");
    }

    #[test]
    fn a_missing_path_or_unreadable_git_metadata_creates_nothing() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("nope");
        assert!(repo_for_init(Some(&missing), parent.path()).is_err());
        assert!(!missing.exists());

        let broken = tempfile::tempdir().unwrap();
        std::fs::write(
            broken.path().join(".git"),
            "gitdir: /nonexistent/clank-test\n",
        )
        .unwrap();
        let err = repo_for_init(None, broken.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("nothing was created"),
            "{err:#}"
        );
        assert!(broken.path().join(".git").is_file(), "left as it was");
    }

    #[test]
    fn a_bare_repository_missing_any_one_part_is_refused_and_left_alone() {
        for part in ["HEAD", "refs", "objects", "config"] {
            let bare = tempfile::tempdir().unwrap();
            assert!(git(bare.path(), &["init", "--bare", "-q"]).status.success());
            std::fs::rename(
                bare.path().join(part),
                bare.path().join(format!("{part}.saved")),
            )
            .unwrap();
            assert!(repo_for_init(None, bare.path()).is_err(), "{part}");
            assert!(!bare.path().join(".git").exists(), "{part}");
            assert!(!bare.path().join(".clank").exists(), "{part}");
        }
    }

    #[test]
    fn an_ordinary_project_with_a_config_file_is_initialized_and_left_as_it_was() {
        let project = tempfile::tempdir().unwrap();
        let config = project.path().join("config");
        std::fs::write(&config, "[core]\nmode = useful\n").unwrap();
        let repo = repo_for_init(None, project.path()).unwrap();
        assert!(repo.join(".git").is_dir());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[core]\nmode = useful\n"
        );
    }

    /// Its config says nothing: only the part count sees a repository.
    #[test]
    fn a_bare_repository_without_head_and_with_an_emptied_config_is_refused() {
        let bare = tempfile::tempdir().unwrap();
        assert!(git(bare.path(), &["init", "--bare", "-q"]).status.success());
        std::fs::remove_file(bare.path().join("HEAD")).unwrap();
        std::fs::write(bare.path().join("config"), "").unwrap();
        assert!(repo_for_init(None, bare.path()).is_err());
        assert!(!bare.path().join(".git").exists());
    }

    #[test]
    fn a_bare_repository_is_refused() {
        let bare = tempfile::tempdir().unwrap();
        assert!(git(bare.path(), &["init", "--bare", "-q"]).status.success());
        let err = repo_for_init(None, bare.path()).unwrap_err();
        assert!(format!("{err:#}").contains("bare repository"), "{err:#}");
    }

    fn seed_user_team_default(home: &Path) {
        use crate::cli::teams_config::{RosterRole, TeamRoster};
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;
        let mut team: TeamRoster = std::collections::BTreeMap::new();
        team.insert(
            AgentLabel::parse("claude").unwrap(),
            team_ref(RosterRole::Master),
        );
        team.insert(
            AgentLabel::parse("codex").unwrap(),
            team_ref(RosterRole::Commit),
        );
        seed_user_config(
            home,
            &[("claude", Tool::Claude), ("codex", Tool::Codex)],
            &[("default", team)],
        );
    }

    fn roster_of(repo: &Path) -> crate::cli::teams_config::RepoConfigFile {
        serde_json::from_str(&std::fs::read_to_string(repo.join(".clank/config.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn bare_init_seeds_a_fresh_repo_from_the_default_team() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::ids::AgentLabel;
        use clank_core::vocab::Tool;
        let home = tempfile::tempdir().unwrap();
        seed_user_team_default(home.path());
        let repo = init_repo();
        seed_bare(Some(home.path()), repo.path()).unwrap();
        let roster = roster_of(repo.path());
        let claude = &roster.agents[&AgentLabel::parse("claude").unwrap()];
        assert_eq!(claude.role, RosterRole::Master);
        assert_eq!(claude.tool, Tool::Claude);
        assert_eq!(
            roster.agents[&AgentLabel::parse("codex").unwrap()].role,
            RosterRole::Commit
        );
    }

    #[test]
    fn bare_init_without_a_default_team_or_home_writes_the_empty_roster() {
        let other = tempfile::tempdir().unwrap();
        seed_user_team_dev(other.path());
        let missing = tempfile::tempdir().unwrap();
        for home in [Some(other.path()), Some(missing.path()), None] {
            let repo = init_repo();
            seed_bare(home, repo.path()).unwrap();
            assert!(roster_of(repo.path()).agents.is_empty(), "{home:?}");
        }
    }

    #[test]
    fn bare_reinit_keeps_an_existing_roster_without_reading_the_user_config() {
        let home = tempfile::tempdir().unwrap();
        seed_user_team_dev(home.path());
        let repo = init_repo();
        register_repo_team(home.path(), repo.path(), "dev").unwrap();
        let before = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        seed_user_team_default(home.path());
        seed_bare(Some(home.path()), repo.path()).unwrap();
        std::fs::write(home.path().join(".clank/config.json"), "{ not json").unwrap();
        seed_bare(Some(home.path()), repo.path()).unwrap();
        let after = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_broken_default_team_is_an_error_not_an_empty_roster() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            r#"{ "teams": { "default": { "ghost": { "ref": "nobody", "role": "master" } } } }"#,
        )
        .unwrap();
        let repo = init_repo();
        assert!(seed_bare(Some(home.path()), repo.path()).is_err());
        std::fs::write(home.path().join(".clank/config.json"), "{ not json").unwrap();
        assert!(seed_bare(Some(home.path()), repo.path()).is_err());
    }

    #[test]
    fn register_repo_team_guarded_refuses_overwrite_of_configured_roster() {
        use crate::cli::teams_config::{RepoConfigFile, RosterRole};
        use clank_core::ids::AgentLabel;
        let home = tempfile::tempdir().unwrap();
        seed_user_team_dev(home.path());
        let repo = init_repo();
        // First registration: succeeds (no prior roster).
        register_repo_team_guarded(home.path(), repo.path(), "dev", false).unwrap();
        let before = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();

        // Second registration without force: refuse, leave config
        // untouched.
        let err = register_repo_team_guarded(home.path(), repo.path(), "dev", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("repo already has a roster"));
        assert!(msg.contains("clank team save"));
        let after = std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap();
        assert_eq!(before, after, "config must not change on refusal");

        // With force: replace.
        register_repo_team_guarded(home.path(), repo.path(), "dev", true).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
    }

    #[test]
    fn register_repo_team_guarded_allows_first_team_after_bare_init() {
        // A freshly-bootstrapped empty repo config (empty roster) is
        // NOT a configured roster, so the first `--team` is allowed.
        use crate::cli::teams_config::{RepoConfigFile, RosterRole};
        use clank_core::ids::AgentLabel;
        let home = tempfile::tempdir().unwrap();
        seed_user_team_dev(home.path());
        let repo = init_repo();
        bootstrap_empty_repo_config(repo.path()).unwrap();

        register_repo_team_guarded(home.path(), repo.path(), "dev", false).unwrap();
        let parsed: RepoConfigFile = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
    }

    #[test]
    fn register_repo_team_recreates_over_old_shape_config() {
        // `init --team` over a LEGACY repo config must RECREATE it,
        // not parse-error. The guard already treats old-shape as
        // overwriteable, so the copy-down core tolerates it too.
        use crate::cli::teams_config::{RepoConfigFile, RosterRole};
        use clank_core::ids::AgentLabel;
        let home = tempfile::tempdir().unwrap();
        seed_user_team_dev(home.path());
        let repo = init_repo();
        let cfg_path = repo.path().join(".clank/config.json");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg_path, r#"{"team": "dev"}"#).unwrap(); // OLD shape

        register_repo_team(home.path(), repo.path(), "dev").unwrap();

        let parsed: RepoConfigFile =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap())
                .expect("recreated as new-shape");
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
    }

    #[test]
    fn register_repo_team_reads_global_config_with_legacy_keys() {
        // The GLOBAL config is NOT blocked by an UNRELATED legacy
        // key: a pre-hard-cut `default_agents` lands in `extra`-style
        // ignored territory and a valid ROSTER team still reads.
        use crate::cli::teams_config::{RepoConfigFile, RosterRole};
        use clank_core::ids::AgentLabel;
        let home = tempfile::tempdir().unwrap();
        let upath = home.path().join(".clank/config.json");
        std::fs::create_dir_all(upath.parent().unwrap()).unwrap();
        std::fs::write(
            &upath,
            r#"{"default_agents":["claude"],"teams":{"dev":{"claude":{"tool":"claude","role":"master"}}}}"#,
        )
        .unwrap();
        let repo = init_repo();

        register_repo_team(home.path(), repo.path(), "dev").unwrap();

        let parsed: RepoConfigFile = serde_json::from_str(
            &std::fs::read_to_string(repo.path().join(".clank/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed.agents[&AgentLabel::parse("claude").unwrap()].role,
            RosterRole::Master
        );
    }
}
