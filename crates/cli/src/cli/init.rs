//! `clank init` — pure repo setup. Non-interactive, assigns
//! nothing (`teams-based-agent-registration`, lloyd 2026-06-09):
//!
//! - write `.clank/.gitignore`,
//! - write `.claude/settings.local.json` with claude
//!   edit-permission rules for `.clank/agents/**`,
//! - install the `post-rewrite` git hook,
//! - warn if the root gitignore swallows any `.clank/` path the
//!   plan needs tracked,
//! - write a self-contained repo config: bare init writes an
//!   empty new-shape config; `--team <name>` copies a user-scope
//!   team's composition + referenced agents into it.
//!
//! It does NOT bind a session, read agent env vars
//! (`CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`), prompt for an
//! identity, or assign a role. Session binding is `clank as`'s
//! sole job; roles are roster-derived.

use std::path::Path;

use anyhow::Context;

use super::{InitArgs, resolve_repo};

use crate::init_facts::{
    CLAUDE_ALLOW_RULES, POST_REWRITE_BODY, POST_REWRITE_MARKER, clank_gitignore_body,
    classify_gitignore_body,
};

/// `clank init` is pure repo setup: scaffold `.clank/`, install
/// the git hook + claude perms, and (optionally) pick a team.
/// It does NOT bind sessions, read agent env vars
/// (`CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`), prompt for an
/// identity, or assign any role — that's `clank as`'s job. Plan:
/// `teams-based-agent-registration` (lloyd 2026-06-09: init
/// assigns nothing).
pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    write_scaffold(&repo)?;
    write_claude_perms(&repo)?;
    write_post_rewrite_hook(&repo, args.force_hooks)?;
    warn_if_globally_excluded(&repo);

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
        // Bare `clank init`: write a VALID empty new-shape config
        // (empty agents, default team) WITHOUT requiring any global
        // template. This is the from-scratch escape from the
        // no-compat chicken-and-egg. Never CLOBBER an existing team
        // selection on re-init.
        None => {
            if repo_has_team(&repo)? {
                // Already configured; bare re-init preserves it.
            } else {
                bootstrap_empty_repo_config(&repo)?;
            }
        }
    }
    Ok(())
}

/// Write a valid empty new-shape [`RepoConfigFile`] (an empty
/// roster). Standalone — needs no user-scope template. The repo
/// then has a parseable config; the user adds agents via
/// `clank agent add <label> --tool <claude|codex>` and designates
/// a master via `clank agent promote <agent>` before any
/// workflow command works.
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
         `clank agent add <label> --tool <claude|codex>` and pick a master with \
         `clank agent promote <agent>`",
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
fn write_post_rewrite_hook(repo: &Path, force: bool) -> anyhow::Result<()> {
    let Some(hook_path) = resolve_hook_path(repo, "post-rewrite") else {
        // Not a real git repo (resolve failed); skip silently.
        return Ok(());
    };
    if let Some(parent) = hook_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating hooks dir `{}`: {e}", parent.display()))?;
    }
    match std::fs::read_to_string(&hook_path) {
        Ok(existing) if existing.contains(POST_REWRITE_MARKER) => {
            // Already ours; refresh body in case it changed.
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("{} up to date", hook_path.display());
        }
        Ok(_) if force => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("force-overwrote {}", hook_path.display());
        }
        Ok(_) => {
            eprintln!(
                "warning: {} already exists with non-clank content; \
                 leaving it alone. Pass `clank init --force-hooks` to overwrite, \
                 or chain `clank rewire --from-stdin` into it manually.",
                hook_path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&hook_path, POST_REWRITE_BODY)?;
            set_executable(&hook_path)?;
            println!("wrote {}", hook_path.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
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

fn write_scaffold(repo: &Path) -> anyhow::Result<()> {
    let clank_dir = repo.join(".clank");
    let plans_dir = clank_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)?;

    let gitignore = clank_dir.join(".gitignore");
    // SET-membership model (ruthless 02da305): three commands
    // mutate this file (init, fork's /worktrees/ ensure, open
    // zellij's /zellij/ ensure), so validation is "every managed
    // entry present, nothing foreign" — order-irrelevant — and
    // repair APPENDS the missing entries rather than rewriting,
    // so an incrementally-appended file can never wedge a later
    // init.
    match std::fs::read_to_string(&gitignore) {
        Ok(existing) => match classify_gitignore_body(&existing) {
            crate::init_facts::GitignoreState::Canonical => {
                println!("{} already up to date", gitignore.display());
            }
            crate::init_facts::GitignoreState::Legacy => {
                // The old per-dir deny list — replace wholesale with the
                // canonical allow-list (a Legacy body is only the recognized
                // managed entries, so nothing user-authored is lost).
                std::fs::write(&gitignore, clank_gitignore_body())?;
                println!("upgraded {}", gitignore.display());
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
            println!("wrote {}", gitignore.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Warn (but don't fail) if some ancestor `.gitignore` or
/// `core.excludesFile` excludes paths that need to be tracked.
///
/// Probes every `.clank/` subpath the plan requires committed
/// (`plans/`, `finished/`, `config.json`, `agents/`) and parses
/// `git check-ignore -v`'s structured output:
///   `<source_file>:<line>:<pattern>\t<probed_path>`
/// We suppress only when `<source_file>` is the `.clank/.gitignore`
/// we just wrote. Substring matching on the whole record would be
/// fooled by paths or patterns that happen to contain that literal.
fn warn_if_globally_excluded(repo: &Path) {
    const TRACKED_PROBES: &[&str] = &[
        ".clank/plans",
        ".clank/finished",
        ".clank/config.json",
        ".clank/agents",
    ];
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

/// Write/merge `.claude/settings.local.json` with the
/// `permissions.allow` rules that let claude edit
/// `.clank/agents/**` (the agent's own dir) without prompting on
/// every Write/Edit. Tag-merge: never clobber unrelated allow
/// entries the user may have added.
///
/// Codex's permission model is shell-command-based, not file-path-
/// based, so there's no equivalent file to write for codex —
/// its sandbox already permits writes inside the workspace.
fn write_claude_perms(repo: &Path) -> anyhow::Result<()> {
    let claude_dir = repo.join(".claude");
    let path = claude_dir.join("settings.local.json");

    let mut value: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("parsing `{}` as JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => return Err(e.into()),
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let permissions = obj
        .entry("permissions".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let permissions = permissions
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`permissions` is not a JSON object"))?;
    let allow = permissions
        .entry("allow".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let allow = allow
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("`permissions.allow` is not a JSON array"))?;

    let mut added = false;
    for rule in CLAUDE_ALLOW_RULES {
        let exists = allow.iter().any(|v| v.as_str() == Some(*rule));
        if !exists {
            allow.push(serde_json::Value::String((*rule).into()));
            added = true;
        }
    }

    std::fs::create_dir_all(&claude_dir)?;
    let serialized = serde_json::to_string_pretty(&value)?;
    std::fs::write(&path, format!("{serialized}\n"))?;
    if added {
        println!("wrote claude permissions to {}", path.display());
    } else {
        println!("{} already has clank permissions", path.display());
    }
    Ok(())
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
             roster locally (`clank agent add` + `clank agent promote`), or — to make \
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
                     or a full inline definition (`{{\"tool\": \"<claude|codex>\", \"role\": ...}}`)"
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
        assert!(body.contains("/*"));
        assert!(body.contains("!/plans/"));
        assert!(body.contains("!/finished/"));
        assert!(body.contains("!/.gitignore"));
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

    #[test]
    fn writes_claude_perms_creates_file_with_allow_rules() {
        let dir = init_repo();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
        assert!(entries.contains(&"Write(.clank/agents/**)"));
        assert!(entries.contains(&"Edit(.clank/agents/**)"));
        assert!(entries.contains(&"Read(.clank/agents/**)"));
    }

    #[test]
    fn writes_claude_perms_preserves_existing_allow_entries() {
        let dir = init_repo();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.local.json"),
            r#"{"permissions":{"allow":["Bash(ls *)"]}}"#,
        )
        .unwrap();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        let entries: Vec<&str> = allow.iter().filter_map(|x| x.as_str()).collect();
        assert!(entries.contains(&"Bash(ls *)"), "existing entry dropped");
        assert!(entries.contains(&"Write(.clank/agents/**)"));
    }

    #[test]
    fn writes_claude_perms_is_idempotent() {
        let dir = init_repo();
        write_claude_perms(dir.path()).unwrap();
        write_claude_perms(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".claude/settings.local.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allow = v["permissions"]["allow"].as_array().unwrap();
        // Three entries, no duplicates after re-run.
        let writes = allow
            .iter()
            .filter(|x| x.as_str() == Some("Write(.clank/agents/**)"))
            .count();
        assert_eq!(writes, 1, "duplicate entry on re-run");
    }

    /// `check-ignore` matrix per D10: with the recommended root
    /// gitignore + the managed `.clank/.gitignore`, which paths
    /// are tracked vs ignored.
    ///
    /// Check-ignore matrix for the simplified tracking model:
    /// only `plans/` and `finished/` need to be tracked;
    /// everything else under `.clank/` is per-user / local
    /// (agents/, feedback/, cache/, config.json — none of these
    /// exist anymore as repo-shared state).
    #[test]
    fn check_ignore_matrix_simplified() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join(".gitignore"),
            "/target/\n\
             .clank/*\n\
             !.clank/plans/\n\
             !.clank/finished/\n",
        )
        .unwrap();
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
        // Per-user / local.
        assert!(probe(".clank/agents/alice/config.json"));
        assert!(probe(".clank/agents/alice/feedback/foo/abc.md"));
        assert!(probe(".clank/cache/anything"));
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
