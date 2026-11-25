//! File-IO for per-agent (`.clank/agents/<label>/config.json`)
//! and repo-level (`.clank/config.json`) typed configs, and for which
//! session holds each agent's label (its `state.json`, [`state`]).
//!
//! Pure types live in `clank_core::agent_config`; this module is
//! the only place that knows where they sit on disk and how to
//! load/save them atomically.

use std::path::{Path, PathBuf};

use anyhow::Context;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use clank_core::agent_config::{AgentConfig, OutageNotified, ResumePending, Session};
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::{AutoMode, Tool};
use clank_user_config::versioned::{self, Refusal};
pub(crate) use state::Bound;

pub(crate) mod identity;
pub mod repo_config;
pub(crate) mod state;

/// The newest version of an agent's `config.json` this build reads and
/// writes.
pub const AGENT_CONFIG_VERSION: u64 = 1;

/// A successfully resolved roster does not contain the requested identity.
///
/// This is structurally distinct from transient config I/O/parse failures so
/// a parked wait can terminate a removed identity without giving up its
/// fail-soft retry behavior for temporary read failures.
#[derive(Debug, thiserror::Error)]
pub enum RoleResolutionError {
    #[error("agent `{label}` is not on this repo's roster")]
    NotRegistered { label: AgentLabel },
}

/// Repo-relative path: `.clank/agents`.
pub fn agents_root(repo: &Path) -> PathBuf {
    repo.join(".clank").join("agents")
}

/// Repo-relative path: `.clank/agents/<label>/config.json`.
pub fn agent_config_path(repo: &Path, label: &AgentLabel) -> PathBuf {
    config_in(&agents_root(repo).join(label.as_str()))
}

fn config_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("config.json")
}

/// Load this agent's config from disk. Returns `Ok(None)` if the
/// file doesn't exist (caller may default-construct); other I/O
/// or JSON errors propagate.
pub fn load_agent_config(repo: &Path, label: &AgentLabel) -> anyhow::Result<Option<AgentConfig>> {
    let path = agent_config_path(repo, label);
    load_json(&path).with_context(|| format!("reading `{}`", path.display()))
}

/// An agent config as the document it is stored as, for diagnostics
/// about what the typed config ignores on read. `None` when absent or
/// unreadable.
pub(crate) fn agent_config_document(
    repo: &Path,
    label: &AgentLabel,
) -> Option<clank_user_config::Document> {
    versioned::read(&agent_config_path(repo, label), AGENT_CONFIG_VERSION)
        .ok()
        .flatten()
}

/// The session bound to the agent whose directory is `agent_dir`. A state
/// written before the binding was kept there binds nothing.
pub(crate) fn binding_in(agent_dir: &Path) -> anyhow::Result<Option<Session>> {
    #[derive(serde::Deserialize)]
    struct Binding {
        binding: Option<Session>,
    }
    let path = state::path(agent_dir);
    let Some(stored) = versioned::read_stored(&path, state::VERSION)? else {
        return Ok(None);
    };
    if !stored.versioned {
        return Ok(None);
    }
    let binding: Binding = serde_json::from_value(serde_json::Value::Object(stored.doc))
        .map_err(|e| Refusal::malformed(&path, e))?;
    Ok(binding.binding)
}

/// The session bound to `label`, if any.
pub fn binding(repo: &Path, label: &AgentLabel) -> anyhow::Result<Option<Session>> {
    identity::binding(repo, label)
}

/// Whether the current binding still assigns this label to this tool and session.
/// An unreadable or absent config cannot authorize a wake.
pub(crate) fn binding_matches(repo: &Path, label: &AgentLabel, tool: Tool, session: &str) -> bool {
    identity::binding(repo, label)
        .ok()
        .flatten()
        .is_some_and(|s| s.tool == tool && s.id.as_str() == session)
}

/// Only [`mutate_locked`] may call this: a write of a config loaded
/// outside the label's lock puts back whatever another writer changed
/// in between.
fn save_agent_config(guard: &StateGuard, cfg: &AgentConfig) -> anyhow::Result<()> {
    let path = config_in(&guard.agent_dir);
    save_json(&path, cfg).with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}

/// Edit one agent's config in place, defaulting a missing file. Every
/// writer of the file serializes on the agent's lock, so an edit always starts from the last
/// committed state and never reverts another writer's field. An
/// unreadable config is an error, never replaced.
pub fn update_agent_config(
    repo: &Path,
    label: &AgentLabel,
    edit: impl FnOnce(&mut AgentConfig),
) -> anyhow::Result<()> {
    let guard = StateGuard::acquire(repo, label)?;
    mutate_locked(&guard, edit)
}

/// [`update_agent_config`] for an edit that can refuse: a refusal
/// leaves the file exactly as it was, absent included.
pub fn try_update_agent_config<T>(
    repo: &Path,
    label: &AgentLabel,
    edit: impl FnOnce(&mut AgentConfig) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let guard = StateGuard::acquire(repo, label)?;
    try_mutate_locked(&guard, edit)
}

fn mutate_locked<T>(
    guard: &StateGuard,
    edit: impl FnOnce(&mut AgentConfig) -> T,
) -> anyhow::Result<T> {
    try_mutate_locked(guard, |cfg| Ok(edit(cfg)))
}

/// The one config transaction: load under `guard`, edit, and replace
/// the file only if the edit succeeded and changed it.
fn try_mutate_locked<T>(
    guard: &StateGuard,
    edit: impl FnOnce(&mut AgentConfig) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = config_in(&guard.agent_dir);
    let loaded: Option<AgentConfig> =
        load_json(&path).with_context(|| format!("reading `{}`", path.display()))?;
    let mut cfg = loaded.clone().unwrap_or_default();
    let out = edit(&mut cfg)?;
    #[cfg(test)]
    tests::inside_edit();
    if loaded.as_ref() != Some(&cfg) {
        save_agent_config(guard, &cfg)?;
    }
    Ok(out)
}

/// Set this agent's auto-mode, preserving the rest of its config.
/// The single write path for arming/disarming auto — `clank auto`
/// and the `clank tui` toggle both call it, so neither can drop
/// `wait_events` on a flip.
pub fn set_auto_mode(repo: &Path, label: &AgentLabel, mode: AutoMode) -> anyhow::Result<()> {
    update_agent_config(repo, label, |cfg| cfg.auto_mode = Some(mode))
}

/// Error returned by the roster resolvers when a repo has no
/// master designated. No legacy fallback exists: a repo without a
/// master is a setup error, not a master-only default.
fn no_team_configured() -> anyhow::Error {
    anyhow::anyhow!(
        "this repo has no master agent. Run `clank init --team <name>` or \
         `clank agent make-master <agent>` to set one."
    )
}

/// Resolve an agent's role for this repo via the roster-resolved
/// registered set: `label == master` → `Master`; a standby member →
/// `Standby`; any other member → `Reviewer`; otherwise a typed
/// not-registered error.
///
/// Errors if the repo has no master designated — there is no
/// legacy fallback.
pub fn resolve_role(repo: &Path, label: &AgentLabel) -> anyhow::Result<clank_core::vocab::Role> {
    match try_resolve_via_team(repo)? {
        Some(set) => role_from_registered_set(&set, label).ok_or_else(|| {
            RoleResolutionError::NotRegistered {
                label: label.clone(),
            }
            .into()
        }),
        None => Err(no_team_configured()),
    }
}

/// The tool declared for a label in this repo's roster. Hook adapter
/// flags describe a wire protocol; the roster describes which agent
/// actually owns the session.
pub fn resolve_tool(repo: &Path, label: &AgentLabel) -> anyhow::Result<Tool> {
    let Some(set) = try_resolve_via_team(repo)? else {
        return Err(no_team_configured());
    };
    if &set.master == label {
        return Ok(set.master_desc.tool);
    }
    set.members
        .iter()
        .find(|member| &member.label == label)
        .map(|member| member.desc.tool)
        .ok_or_else(|| {
            RoleResolutionError::NotRegistered {
                label: label.clone(),
            }
            .into()
        })
}

/// Best-effort role text for diagnostic/status surfaces.
///
/// Preserve the historical no-master fallback while exposing a successfully
/// resolved roster's explicit identity absence. Callers must not flatten the
/// latter back into a missing-master diagnosis.
pub fn role_status_text(repo: &Path, label: &AgentLabel) -> String {
    match resolve_role(repo, label) {
        Ok(role) => role.as_str().to_string(),
        Err(err)
            if matches!(
                err.downcast_ref::<RoleResolutionError>(),
                Some(RoleResolutionError::NotRegistered { .. })
            ) =>
        {
            format!("unknown ({err})")
        }
        Err(_) => "unknown (no master configured)".to_string(),
    }
}

/// Plan: teams-based-agent-registration.
///
/// Two-tier reviewer split for a repo. Used by `WorkPolicy`
/// construction sites (status, wait, open) to feed the
/// two-tier gate state machine.
///
/// Returns the commit / plan / final reviewer tiers (gate folded into
/// both plan and final) from the resolved registered set. Errors if
/// the repo has no roster configured — there is no legacy fallback.
pub fn load_reviewer_tiers(repo: &Path) -> anyhow::Result<ReviewerTiers> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    load_reviewer_tiers_with(repo, home.as_deref())
}

/// Home-explicit [`load_reviewer_tiers`] — for in-process callers
/// (tests, query cores) that supply the home dir rather than
/// reading `$HOME`. Plan: dogfood-init-setup-in-tests (Phase B).
pub fn load_reviewer_tiers_with(repo: &Path, home: Option<&Path>) -> anyhow::Result<ReviewerTiers> {
    match try_resolve_via_team_with(repo, home)? {
        Some(set) => Ok(ReviewerTiers::from_registered(&set)),
        None => Err(no_team_configured()),
    }
}

/// The three gate-compute reviewer tiers (gate reviewers folded into
/// both `plan` and `final`), ready for `WorkPolicy` / `compute_gate`.
/// The single conversion from a [`crate::cli::teams_config::RegisteredSet`]
/// so every WorkPolicy builder gets the same gate-fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerTiers {
    pub commit: Vec<AgentLabel>,
    pub plan: Vec<AgentLabel>,
    pub final_: Vec<AgentLabel>,
}

impl ReviewerTiers {
    pub fn from_registered(set: &crate::cli::teams_config::RegisteredSet) -> Self {
        Self {
            commit: set.commit_tier(),
            plan: set.plan_tier(),
            final_: set.final_tier(),
        }
    }

    /// Build gate tiers directly from a prospective repo roster. Config
    /// mutation needs to evaluate the before/after gate before the new config
    /// is persisted, so it cannot go back through the filesystem resolver.
    pub fn from_roster(roster: &crate::cli::teams_config::Roster) -> Self {
        use crate::cli::teams_config::RosterRole;

        let labels = |include: fn(RosterRole) -> bool| {
            roster
                .iter()
                .filter(|(_, agent)| include(agent.role))
                .map(|(label, _)| label.clone())
                .collect()
        };
        Self {
            commit: labels(RosterRole::in_commit_tier),
            plan: labels(RosterRole::in_plan_tier),
            final_: labels(RosterRole::in_final_tier),
        }
    }

    pub fn empty() -> Self {
        Self {
            commit: Vec::new(),
            plan: Vec::new(),
            final_: Vec::new(),
        }
    }
}

/// Derive a role from a resolved registered set. Pure: no
/// $HOME, no filesystem. Testable directly.
pub fn role_from_registered_set(
    set: &crate::cli::teams_config::RegisteredSet,
    label: &AgentLabel,
) -> Option<clank_core::vocab::Role> {
    use clank_core::vocab::Role;
    if &set.master == label {
        return Some(Role::Master);
    }
    set.members
        .iter()
        .find(|m| &m.label == label)
        .map(|m| m.role.resolved())
}

/// Every agent this repo's roster names, the master first. Empty when
/// the repo has no master yet; an error when its config cannot be read.
pub fn roster_labels(repo: &Path) -> anyhow::Result<Vec<AgentLabel>> {
    let Some(set) = try_resolve_via_team(repo)? else {
        return Ok(Vec::new());
    };
    let mut out = vec![set.master.clone()];
    out.extend(set.members.iter().map(|m| m.label.clone()));
    Ok(out)
}

pub fn try_resolve_via_team(
    repo: &Path,
) -> anyhow::Result<Option<crate::cli::teams_config::RegisteredSet>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    try_resolve_via_team_with(repo, home.as_deref())
}

/// Shared dispatch helper for `load_reviewer_tiers`,
/// `reviewer_tiers_for_render`, and `resolve_role`. Returns:
/// - `Ok(Some(RegisteredSet))` when the repo config parses and
///   resolves (a master is set).
/// - `Ok(None)` when there's no repo config OR the repo has no
///   master yet (`ResolutionError::NoMaster`). This is "no team
///   configured" — workflow commands error
///   (`no_team_configured()`), read-only renderers degrade to
///   empty tiers.
/// - `Err(_)` (fail-closed) when the config is malformed, uses
///   the old team schema (re-init hint), or resolution fails for
///   any reason other than `NoMaster` (e.g. `MultipleMasters`).
///
/// The `home` parameter is unused: the repo's roster is
/// self-contained (each agent carries its definition + role). It's
/// kept so the many workflow callers need no signature changes.
pub fn try_resolve_via_team_with(
    repo: &Path,
    _home: Option<&Path>,
) -> anyhow::Result<Option<crate::cli::teams_config::RegisteredSet>> {
    use crate::cli::teams_config::{ResolutionError, resolve_registered_set};

    let Some(repo_cfg) = repo_config::read(repo)? else {
        return Ok(None);
    };

    // The repo's roster is self-contained: every agent carries its
    // definition + role. A bootstrapped repo (no master yet)
    // resolves to `NoMaster` — surfaced as `Ok(None)` so workflow
    // callers report "no master configured" and read-only renderers
    // degrade to empty tiers, rather than every command erroring
    // before the user has set a master. A `MultipleMasters`
    // misconfiguration fails closed.
    match resolve_registered_set(&repo_cfg) {
        Ok(set) => Ok(Some(set)),
        Err(ResolutionError::NoMaster) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The port the remote listens on for this repo, if one was ever
/// sampled and kept.
pub fn web_port(repo: &Path) -> anyhow::Result<Option<u16>> {
    Ok(load_repo_config_required(repo)?.web.and_then(|w| w.port))
}

/// Keep `port` as this repo's: the next remote binds it rather
/// than asking the OS again, so the URL stays what it was.
pub fn record_web_port(repo: &Path, port: u16) -> anyhow::Result<()> {
    repo_config::replace(repo, |prior| {
        let mut file = match prior {
            repo_config::Prior::Valid(file) => *file,
            repo_config::Prior::Absent => return Err(no_repo_config(repo)),
            repo_config::Prior::Legacy => {
                return Err(repo_config::legacy_schema_error(&repo_config::path(repo)));
            }
        };
        file.web.get_or_insert_with(Default::default).port = Some(port);
        Ok((Some(file), ()))
    })
}

/// The repo's config, which must exist. Used by `clank team save`,
/// `clank export` and the remote, which need the repo's self-contained
/// config and must not publish or dump a half-read one.
pub fn load_repo_config_required(
    repo: &Path,
) -> anyhow::Result<crate::cli::teams_config::RepoConfigFile> {
    repo_config::read(repo)?.ok_or_else(|| no_repo_config(repo))
}

fn no_repo_config(repo: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: no repo config found. Run `clank init` first.",
        repo_config::path(repo).display()
    )
}

/// The label of every agent directory under `.clank/agents/`, whatever
/// files it holds.
pub(crate) fn agent_labels(repo: &Path) -> anyhow::Result<Vec<AgentLabel>> {
    iter_agent_dirs(repo)?
        .map(|entry| entry.map(|(label, _)| label))
        .collect()
}

/// Iterate `.clank/agents/*` returning each (label, dir-path)
/// that parses. The outer Result is for the read_dir on the
/// `agents/` root; per-entry errors are propagated as inner
/// `Result`s so callers can decide whether to drop or surface.
fn iter_agent_dirs(
    repo: &Path,
) -> anyhow::Result<impl Iterator<Item = anyhow::Result<(AgentLabel, PathBuf)>>> {
    let root = agents_root(repo);
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::<anyhow::Result<(AgentLabel, PathBuf)>>::new().into_iter());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", root.display())));
        }
    };
    let mut out: Vec<anyhow::Result<(AgentLabel, PathBuf)>> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.push(Err(e.into()));
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                out.push(Err(e.into()));
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match AgentLabel::parse(&name) {
            Ok(label) => out.push(Ok((label, entry.path()))),
            Err(_) => continue,
        }
    }
    Ok(out.into_iter())
}

/// Outcome of [`bind_session_to_agent`]: the bound agent's
/// label (echoed for diagnostics) plus the labels of any OTHER
/// agents whose stale binding to the same session id was cleared
/// in the process. Both `clank as` and `clank init` phase 2
/// surface the cleared list to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindOutcome {
    pub label: AgentLabel,
    pub cleared_from: Vec<AgentLabel>,
}

/// Release `label` only while `session` still holds it. The compare
/// and clear run under the agent's lock, so an old pane cannot clear a
/// successor's binding. Waiters check the binding before they present
/// work and stop when it changes.
pub fn release_binding(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
) -> anyhow::Result<bool> {
    let guard = StateGuard::acquire(repo, label)?;
    identity::mutate(&guard, |id| {
        if id.binding.as_ref().is_none_or(|s| &s.id != session) {
            return false;
        }
        id.binding = None;
        true
    })
}

/// Serializes every write of ONE agent's `state.json` and `config.json`:
/// its binding's transitions and every other part.
///
/// Comparing the session before clearing is not enough on its own:
/// read, compare and write are separate steps, and a replacement
/// binding that lands between the read and the write is overwritten
/// by a decision made before it existed (codex on 7e1a33e). Both
/// sides of the transition — claiming the label and releasing it —
/// take this lock, so the compare and everything it authorises happen
/// against the same state.
///
/// BLOCKING, unlike the leases elsewhere: a caller here is not
/// choosing whether to drive, it is performing a short file write it
/// must not do concurrently. The monitor's serve loop is the exception:
/// an agent's process may hold this lock across a side effect, so the
/// loop only ever tries it ([`state::try_transact`]). Held for one agent
/// only, and never across another's, so there is no lock order to get
/// wrong.
pub(crate) struct StateGuard {
    agent_dir: PathBuf,
    label: AgentLabel,
    _lock: state::Lock,
}

impl StateGuard {
    fn acquire(repo: &Path, label: &AgentLabel) -> anyhow::Result<Self> {
        let agent_dir = agents_root(repo).join(label.as_str());
        Ok(Self {
            _lock: state::Lock::acquire(&agent_dir)?,
            agent_dir,
            label: label.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn in_dir(agent_dir: &Path) -> anyhow::Result<Self> {
        let name = agent_dir
            .file_name()
            .and_then(|n| n.to_str())
            .context("an agent's directory is named for its label")?;
        Ok(Self {
            _lock: state::Lock::acquire(agent_dir)?,
            agent_dir: agent_dir.to_path_buf(),
            label: AgentLabel::parse(name).or_else(|_| AgentLabel::parse("agent"))?,
        })
    }

    pub(crate) fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    pub(crate) fn lock(&self) -> &state::Lock {
        &self._lock
    }

    /// Whether `session` of `tool` holds the label. True until the guard
    /// is dropped: no binding commits while it is held.
    pub(crate) fn bound_to(&self, tool: Tool, session: &str) -> bool {
        binding_in(&self.agent_dir)
            .ok()
            .flatten()
            .is_some_and(|s| s.tool == tool && s.id.as_str() == session)
    }
}

/// Hold `label`'s lock: no bind, rebind or release of the label, and no
/// other write of its state, commits until the guard is dropped, so a
/// binding checked under it stays the binding until then.
pub(crate) fn hold_state(repo: &Path, label: &AgentLabel) -> anyhow::Result<StateGuard> {
    StateGuard::acquire(repo, label)
}

/// Block until this process holds `path`'s exclusive flock, creating
/// the file if need be. Held until the returned file is dropped.
pub(crate) fn exclusive_lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening `{}`", path.display()))?;
    if let Err(e) = rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive) {
        return Err(anyhow::anyhow!(
            "locking `{}`: {}",
            path.display(),
            std::io::Error::from(e)
        ));
    }
    Ok(file)
}

/// Record where `session`'s transcript lives — only while that session
/// still holds the label. Compare-and-set under the agent's lock for
/// the same reason release is: a path arriving after the label was
/// rebound belongs to a session that is no longer this agent.
pub fn record_transcript(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
    transcript: &str,
) -> anyhow::Result<bool> {
    let guard = StateGuard::acquire(repo, label)?;
    identity::mutate(&guard, |id| {
        let Some(bound) = id.binding.as_mut().filter(|s| &s.id == session) else {
            return false;
        };
        bound.transcript = Some(transcript.to_string());
        true
    })
}

/// How long a resumed Claude session has to check in (its SessionStart
/// hook) before the next start forgets it.
pub const CHECK_IN_WITHIN: time::Duration = time::Duration::seconds(60);

/// Whether `pending`, a resume of the bound session, has gone
/// [`CHECK_IN_WITHIN`] without checking in. An unreadable `at` is not
/// evidence of anything.
pub fn resume_unconfirmed(pending: Option<&ResumePending>, now: OffsetDateTime) -> bool {
    pending.is_some_and(|p| {
        OffsetDateTime::parse(&p.at, &Rfc3339).is_ok_and(|at| now - at >= CHECK_IN_WITHIN)
    })
}

/// The bound session and its unconfirmed resume, if one stands.
pub fn resume_pending(
    repo: &Path,
    label: &AgentLabel,
) -> anyhow::Result<(Option<Session>, Option<ResumePending>)> {
    let state = state::State::load(&agents_root(repo).join(label.as_str()))?;
    Ok((state.binding()?, state.resume_pending()?))
}

/// Record, before `session` is resumed, that it has yet to check in.
/// A marker already standing is kept, so the window runs from the first
/// unconfirmed attempt. Only a binding with a recorded transcript is
/// armed: that field is written by the same hook that checks in, so for
/// any other binding silence proves nothing.
pub fn arm_resume(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
    now: OffsetDateTime,
) -> anyhow::Result<bool> {
    let at = now.format(&Rfc3339).context("formatting timestamp")?;
    let guard = StateGuard::acquire(repo, label)?;
    identity::mutate(&guard, |id| {
        let hook_ran = id
            .binding
            .as_ref()
            .is_some_and(|s| &s.id == session && s.transcript.is_some());
        if !hook_ran {
            return false;
        }
        id.resume_pending.get_or_insert(ResumePending { at });
        true
    })
}

/// `session` checked in: clear its unconfirmed resume, and only its.
pub fn resume_checked_in(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
) -> anyhow::Result<bool> {
    let guard = StateGuard::acquire(repo, label)?;
    identity::mutate(&guard, |id| {
        if id.binding.as_ref().is_none_or(|s| &s.id != session) || id.resume_pending.is_none() {
            return false;
        }
        id.resume_pending = None;
        true
    })
}

/// Record `session` as told of an outage, at most once until a monitor
/// serves it ([`outage_served`]): `true` when this call is the one that
/// tells it, `false` when it was told already or no longer holds the
/// label. A caller given `true` owes the session the telling.
///
/// Every waiter of a session announces through here, under the agent's
/// lock, so two of them cannot both find it untold.
pub(crate) fn announce_outage(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
    now: OffsetDateTime,
) -> anyhow::Result<bool> {
    let at = now.format(&Rfc3339).context("formatting timestamp")?;
    let agent_dir = agents_root(repo).join(label.as_str());
    let permit = crate::upgrade::writing()?;
    state::transact(&agent_dir, &permit, |txn| {
        let told: Option<OutageNotified> = txn.bound(session, Bound::Outage)?;
        let bound = txn.binding()?.is_some_and(|s| &s.id == session);
        if !bound || told.is_some() {
            return Ok(false);
        }
        txn.patch(session, Bound::Outage, Some(&OutageNotified { at }))?;
        Ok(true)
    })
}

/// `session`, bound to `label`, told its repo has no monitor.
#[cfg(test)]
pub(crate) fn told_of_an_outage(repo: &Path, label: &AgentLabel, session: &str) {
    assert!(
        announce_outage(
            repo,
            label,
            &SessionId::parse(session).unwrap(),
            OffsetDateTime::now_utc(),
        )
        .unwrap()
    );
}

/// What serving a session did to its outage notice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutageServed {
    /// A notice stood for the session, and it is cleared.
    Cleared,
    /// No notice stood for the session.
    NoneStood,
    /// The agent's lock was held elsewhere, so nothing was read or
    /// cleared: a notice standing for the session stays until a later
    /// serve finds the lock free.
    Contended,
}

/// A monitor served `session`, so its outage is over and the next one is
/// announced again.
///
/// Never waits for the agent's lock: an agent's process may hold it
/// across a side effect, and a server that waited would stall serving
/// for every agent. Clearing is therefore best effort; a serve that
/// finds the lock held clears nothing ([`OutageServed::Contended`]), and
/// the notice is cleared only by a later serve whose attempt succeeds.
///
/// The marker is read under the lock, never before it: a read taken
/// first could miss an announcement recording it meanwhile and leave it
/// standing after the recovery. A healthy attach costs the lock and a
/// read; nothing is written unless a notice stood.
///
/// What serving is belongs to the caller's connection: a verdict for a
/// hook's report, the first note for a subscription.
pub(crate) fn outage_served(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
) -> anyhow::Result<OutageServed> {
    let agent_dir = agents_root(repo).join(label.as_str());
    let permit = crate::upgrade::writing()?;
    let served = state::try_transact(&agent_dir, &permit, |txn| {
        let told: Option<OutageNotified> = txn.bound(session, Bound::Outage)?;
        if told.is_none() {
            return Ok(OutageServed::NoneStood);
        }
        txn.patch::<OutageNotified>(session, Bound::Outage, None)?;
        Ok(OutageServed::Cleared)
    })?;
    Ok(served.unwrap_or(OutageServed::Contended))
}

/// Unbind `session` from `label` with its unconfirmed resume, only while
/// the whole judgment still holds under the lock: `session` is bound and
/// [`resume_unconfirmed`]. A judgment made before a rebind, a check-in or
/// a fresh arm landed is refused (`false`), never acted on.
pub fn release_unconfirmed_resume(
    repo: &Path,
    label: &AgentLabel,
    session: &SessionId,
    now: OffsetDateTime,
) -> anyhow::Result<bool> {
    let guard = StateGuard::acquire(repo, label)?;
    identity::mutate(&guard, |id| {
        if id.binding.as_ref().is_none_or(|s| &s.id != session)
            || !resume_unconfirmed(id.resume_pending.as_ref(), now)
        {
            return false;
        }
        id.binding = None;
        true
    })
}

/// Bind `label` to an OpenCode session before it is launched, so the
/// model never spends a turn on it, under the label's lock the caller holds
/// across the work before the bind. `replaces` is the binding the caller
/// judged replaceable: another tool's, or an OpenCode session its store
/// lacks. Only it is replaced, by the session `mint` answers; mint runs
/// only then, and a failed one binds nothing. An OpenCode session bound
/// since (by a concurrent start) is returned rather than replaced, and any
/// other binding that appeared is refused.
pub(crate) fn reserve_opencode_locked(
    guard: &StateGuard,
    replaces: Option<&Session>,
    mint: impl FnOnce() -> anyhow::Result<SessionId>,
) -> anyhow::Result<Session> {
    let label = &guard.label;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("formatting timestamp")?;
    identity::mutate(guard, |id| match &id.binding {
        Some(s) if Some(s) != replaces && s.tool == Tool::OpenCode => Ok(s.clone()),
        Some(s) if Some(s) != replaces => Err(anyhow::anyhow!(
            "`{label}` was bound to {} session {} while it was starting",
            s.tool.as_str(),
            s.id.as_str()
        )),
        _ => {
            let s = Session {
                id: mint()?,
                tool: Tool::OpenCode,
                updated_at: now,
                transcript: None,
            };
            id.binding = Some(s.clone());
            Ok(s)
        }
    })?
}

/// Unbind `session` from the label `guard` holds, if it is still bound.
pub(crate) fn unbind_locked(guard: &StateGuard, session: &SessionId) -> anyhow::Result<bool> {
    identity::mutate(guard, |id| {
        if id.binding.as_ref().is_none_or(|s| &s.id != session) {
            return false;
        }
        id.binding = None;
        true
    })
}

/// Bind a session to an agent label, and clear the same session id from
/// any OTHER agent that holds it. A different session than the one bound
/// starts with nothing known of it ([`identity::mutate`]).
///
/// Single source of truth for the bind operation — used by
/// `clank as` AND `clank init` phase 2 so the two-shells-one-
/// session uniqueness invariant can't be re-introduced by a
/// caller that forgets the clear-stale step.
pub fn bind_session_to_agent(
    repo: &Path,
    label: &AgentLabel,
    tool: Tool,
    session_id: &SessionId,
) -> anyhow::Result<BindOutcome> {
    let all = identity::bindings(repo, false)?;
    let stale: Vec<(AgentLabel, Session)> = all
        .into_iter()
        .filter(|(other_label, cfg)| other_label != label && &cfg.id == session_id)
        .collect();

    // Bind the new label FIRST. Order matters: if the clear-stale
    // step below partially fails, the worst case is a ghost
    // binding on the old agent (which `clank as` or `clank doctor`
    // can clean up later). The reverse (clear-then-bind) could
    // leave them with neither.
    //
    // Under the label's lock, so a release that read this binding a
    // moment ago cannot land its clear on top of the claim.
    let owned = StateGuard::acquire(repo, label)?;
    let consume_grok_fork = if tool == Tool::Grok {
        crate::cli::grok_launch::check_binding(repo, label, session_id.as_str())?
    } else {
        false
    };
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("formatting timestamp")?;
    identity::mutate(&owned, |id| {
        id.binding = Some(Session {
            id: session_id.clone(),
            tool,
            updated_at: now,
            transcript: None,
        })
    })?;
    if consume_grok_fork {
        std::fs::remove_file(crate::cli::fork::fork_spec_path(repo, label))
            .context("Grok child bound, but its launch intent could not be consumed")?;
    }
    // Dropped before any OTHER label is touched: one lock at a time is
    // what keeps two crossing binds from deadlocking on each other.
    drop(owned);

    let mut cleared_from: Vec<AgentLabel> = Vec::new();
    for (other_label, _) in stale {
        // The same compare-and-clear, for the same reason: the label
        // may have been rebound since the scan above, and only the
        // session that still holds it may give it up.
        if release_binding(repo, &other_label, session_id)
            .with_context(|| format!("clearing stale binding on `{}`", other_label.as_str()))?
        {
            cleared_from.push(other_label);
        }
    }

    Ok(BindOutcome {
        label: label.clone(),
        cleared_from,
    })
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    let Some(doc) = versioned::read(path, AGENT_CONFIG_VERSION)? else {
        return Ok(None);
    };
    serde_json::from_value(serde_json::Value::Object(doc))
        .map(Some)
        .map_err(|e| Refusal::malformed(path, e).into())
}

fn save_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let _permit = crate::upgrade::writing()?;
    match serde_json::to_value(value)? {
        serde_json::Value::Object(doc) => versioned::write(path, &doc, AGENT_CONFIG_VERSION),
        _ => anyhow::bail!("`{}` did not serialize as an object", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::teams_config::{AgentDescription, RegisteredMember, RegisteredSet, RosterRole};
    use clank_core::vocab::{Role, Tool};

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    thread_local! {
        static INSIDE_EDIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Runs, once, whatever this thread armed with [`pause_inside_edit`]:
    /// the seam that holds a writer inside the transaction whose edit is
    /// not a closure the test can supply.
    pub(super) fn inside_edit() {
        if let Some(hook) = INSIDE_EDIT.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    const PAUSE_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

    /// A writer held inside its edit until the test lets it go.
    struct Paused {
        entered: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
        writer: std::thread::JoinHandle<anyhow::Result<()>>,
    }

    impl Paused {
        fn wait_until_inside(&self) {
            self.entered
                .recv_timeout(PAUSE_BOUND)
                .expect("writer A never reached its edit");
        }

        fn commit(self) {
            self.release.send(()).unwrap();
            self.writer.join().unwrap().unwrap();
        }
    }

    /// Run `write` on its own thread and return once it has called the
    /// hold it is handed, which blocks until [`Paused::commit`].
    fn paused_writer(
        write: impl FnOnce(Box<dyn FnOnce() + Send>) -> anyhow::Result<()> + Send + 'static,
    ) -> Paused {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let hold: Box<dyn FnOnce() + Send> = Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(PAUSE_BOUND)
                .expect("the test never released writer A");
        });
        let writer = std::thread::spawn(move || write(hold));
        let paused = Paused {
            entered,
            release,
            writer,
        };
        paused.wait_until_inside();
        paused
    }

    fn pause_inside_edit(hold: Box<dyn FnOnce() + Send>) {
        INSIDE_EDIT.with(|h| *h.borrow_mut() = Some(hold));
    }

    /// Whether another holder of the label's config lock would have to
    /// wait right now. Non-blocking: a fresh open of the lock file is a
    /// separate flock owner even within this process.
    fn domain_is_held(repo: &Path, l: &AgentLabel) -> bool {
        let file = std::fs::File::open(agents_root(repo).join(l.as_str())).unwrap();
        !clank_user_config::lease::take(&file).unwrap()
    }

    fn github_source(repo: &str) -> clank_core::agent_config::WaitEventSource {
        clank_core::agent_config::WaitEventSource::Github(clank_core::agent_config::GithubSource {
            interrupt: false,
            interrupt_message: None,
            repo: repo.into(),
            events: vec![clank_core::agent_config::GithubEventKind::PrOpened],
            poll_interval: None,
            watch: Vec::new(),
            include_own_actions: false,
            branches: Vec::new(),
            prs: Vec::new(),
            prompt: None,
        })
    }

    #[test]
    fn a_field_edit_and_an_auto_flip_both_survive_contention() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let l = label("claude");
        update_agent_config(&repo, &l, |cfg| cfg.auto_mode = Some(AutoMode::On)).unwrap();

        let a = paused_writer({
            let (repo, l) = (repo.clone(), l.clone());
            move |hold| {
                update_agent_config(&repo, &l, |cfg| {
                    cfg.wait_events.push(github_source("o/r"));
                    hold();
                })
            }
        });
        assert!(
            domain_is_held(&repo, &l),
            "while A is inside its edit, set_auto_mode could enter and its write would be reverted"
        );
        a.commit();

        set_auto_mode(&repo, &l, AutoMode::Off).unwrap();
        let cfg = load_agent_config(&repo, &l).unwrap().unwrap();
        assert_eq!(cfg.wait_events, vec![github_source("o/r")], "A's edit");
        assert_eq!(cfg.auto_mode, Some(AutoMode::Off), "B's edit");
    }

    #[test]
    fn a_transcript_record_and_an_auto_flip_share_one_lock() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let l = label("claude");
        let session = sess("e30f2b50-4cef-4955-86a0-fbfe64a768b3");
        bind_session_to_agent(&repo, &l, Tool::Claude, &session).unwrap();
        set_auto_mode(&repo, &l, AutoMode::On).unwrap();

        let a = paused_writer({
            let (repo, l, session) = (repo.clone(), l.clone(), session.clone());
            move |hold| {
                pause_inside_edit(hold);
                assert!(record_transcript(&repo, &l, &session, "/t/a.jsonl")?);
                Ok(())
            }
        });
        assert!(
            domain_is_held(&repo, &l),
            "a binding writer must hold the same lock as every other config edit"
        );
        a.commit();

        set_auto_mode(&repo, &l, AutoMode::Off).unwrap();
        let cfg = load_agent_config(&repo, &l).unwrap().unwrap();
        let bound = identity::binding(&repo, &l).unwrap().expect("binding kept");
        assert_eq!(bound.id, session);
        assert_eq!(bound.transcript.as_deref(), Some("/t/a.jsonl"), "A's edit");
        assert_eq!(cfg.auto_mode, Some(AutoMode::Off), "B's edit");
    }

    #[test]
    fn an_unreadable_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let l = label("claude");
        let path = agent_config_path(repo, &l);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let garbage = b"{\"auto_mode\": \"on\", trunc";
        std::fs::write(&path, garbage).unwrap();

        assert!(set_auto_mode(repo, &l, AutoMode::Off).is_err());
        assert!(update_agent_config(repo, &l, |_| {}).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), garbage);
    }

    /// The transcript path is recorded only while the session that
    /// reported it still holds the label — a path arriving after a
    /// rebind belongs to a session that is no longer this agent.
    #[test]
    fn a_transcript_is_recorded_for_the_bound_session_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let l = label("claude");
        let (old, new) = (
            sess("e30f2b50-4cef-4955-86a0-fbfe64a768b3"),
            sess("11111111-2222-3333-4444-555555555555"),
        );
        bind_session_to_agent(repo, &l, Tool::Claude, &old).unwrap();
        assert!(record_transcript(repo, &l, &old, "/t/old.jsonl").unwrap());
        assert_eq!(
            identity::binding(repo, &l)
                .unwrap()
                .unwrap()
                .transcript
                .as_deref(),
            Some("/t/old.jsonl")
        );
        assert!(
            record_transcript(repo, &l, &old, "/t/old.jsonl").unwrap(),
            "same path: still bound"
        );

        bind_session_to_agent(repo, &l, Tool::Claude, &new).unwrap();
        assert!(
            !record_transcript(repo, &l, &old, "/t/late.jsonl").unwrap(),
            "the old session's path does not land on the new binding"
        );
        assert_eq!(
            identity::binding(repo, &l).unwrap().unwrap().transcript,
            None,
            "a fresh binding starts without a transcript until its own session reports one"
        );
    }

    fn sess(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    fn desc(tool: Tool) -> AgentDescription {
        AgentDescription {
            tool,
            launch: None,
            initial_prompt: None,
        }
    }

    fn registered_set(master: &str, commit: &[&str], gate: &[&str]) -> RegisteredSet {
        let rev = |l: &&str, role: RosterRole| RegisteredMember {
            label: label(l),
            desc: desc(Tool::Claude),
            role,
        };
        RegisteredSet {
            master: label(master),
            master_desc: desc(Tool::Claude),
            members: commit
                .iter()
                .map(|l| rev(l, RosterRole::Commit))
                .chain(gate.iter().map(|l| rev(l, RosterRole::Gate)))
                .collect(),
        }
    }

    #[test]
    fn a_standby_member_resolves_to_standby_and_reviews_in_no_tier() {
        let mut set = registered_set("claude", &[], &[]);
        set.members.push(RegisteredMember {
            label: label("sb"),
            desc: desc(Tool::Claude),
            role: RosterRole::Standby,
        });
        assert_eq!(
            role_from_registered_set(&set, &label("sb")),
            Some(clank_core::vocab::Role::Standby)
        );
        let tiers = ReviewerTiers::from_registered(&set);
        assert!(
            tiers.commit.is_empty() && tiers.plan.is_empty() && tiers.final_.is_empty(),
            "a master plus standby agents is master-only: {tiers:?}"
        );
    }

    #[test]
    fn role_from_registered_set_returns_master_for_team_master() {
        // Codex b59dafb catch: the team master must be
        // Role::Master regardless of what the legacy
        // `default_agents` entry says. Pre-fix, the role
        // resolver was reading the legacy merged declaration
        // and returning Role::Reviewer for a team master whose
        // legacy entry said reviewer — leading to master work
        // items never emitting via wait.
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("codex")),
            Some(Role::Master)
        );
    }

    #[test]
    fn role_from_registered_set_returns_reviewer_for_commit_tier() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("claude")),
            Some(Role::Reviewer)
        );
    }

    #[test]
    fn role_from_registered_set_returns_reviewer_for_gate_tier() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(
            role_from_registered_set(&set, &label("ruthless")),
            Some(Role::Reviewer)
        );
    }

    #[test]
    fn role_from_registered_set_returns_none_for_unknown_label() {
        let set = registered_set("codex", &["claude"], &["ruthless"]);
        assert_eq!(role_from_registered_set(&set, &label("phantom")), None);
    }

    #[test]
    fn resolve_role_returns_typed_error_for_unknown_label() {
        let repo = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"agents":{"codex":{"tool":"codex","role":"master"}}}"#,
        );

        let err = resolve_role(repo.path(), &label("phantom")).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<RoleResolutionError>(),
            Some(RoleResolutionError::NotRegistered { label }) if label.as_str() == "phantom"
        ));
        assert!(format!("{err:#}").contains("not on this repo's roster"));
    }

    #[test]
    fn role_status_text_distinguishes_absence_from_no_master() {
        let repo = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"agents":{"codex":{"tool":"codex","role":"master"}}}"#,
        );
        assert_eq!(role_status_text(repo.path(), &label("codex")), "master");
        assert_eq!(
            role_status_text(repo.path(), &label("phantom")),
            "unknown (agent `phantom` is not on this repo's roster)"
        );

        let no_master = TempDir::new().unwrap();
        assert_eq!(
            role_status_text(no_master.path(), &label("phantom")),
            "unknown (no master configured)"
        );
    }

    // ── try_resolve_via_team_with — dispatch coverage
    //    (ruthless pin 1 on 8c0fdb2: the dispatch helper had
    //    no tests despite codex catching two bugs in it). ──

    use tempfile::TempDir;

    fn write_repo_config(repo: &Path, body: &str) {
        let p = repo.join(".clank/config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_repo_config_missing() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "no repo config → None (no master configured)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_none_when_empty_config() {
        // Bootstrapped repo (empty roster) has no master yet →
        // None (no master configured).
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), "{}");
        let r = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap();
        assert!(r.is_none(), "empty config → None (no master yet)");
    }

    #[test]
    fn try_resolve_via_team_with_returns_set_when_roster_resolves() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "codex": { "tool": "codex", "role": "master" },
                    "claude": { "tool": "claude", "role": "commit" }
                }
            }"#,
        );
        let set = try_resolve_via_team_with(repo.path(), Some(home.path()))
            .unwrap()
            .expect("master set → Some(set)");
        assert_eq!(set.master.as_str(), "codex");
        assert_eq!(set.commit_tier().len(), 1);
        assert_eq!(set.commit_tier()[0].as_str(), "claude");
    }

    #[test]
    fn try_resolve_via_team_with_propagates_multiple_masters() {
        // Two `master`-role entries → ResolutionError::MultipleMasters
        // propagates (fail-closed).
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{
                "agents": {
                    "claude": { "tool": "claude", "role": "master" },
                    "codex": { "tool": "codex", "role": "master" }
                }
            }"#,
        );
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("multiple masters"),
            "expected MultipleMasters error; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_old_team_string_shape() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"team": "dev"}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_just_shipped_agents_team_shape() {
        // The just-shipped `{agents: <role-less descriptions>, team: ...}`
        // shape must fail closed (the `team` field marks it legacy).
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"agents": {"claude": {"tool": "claude"}}, "team": {"master": "claude"}}"#,
        );
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_role_less_agents_map() {
        // An `agents` map of role-less descriptions (no `team` marker)
        // still fails closed: a RosterAgent requires `role`. The serde
        // failure maps to the re-init hint, not a cryptic message.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"agents": {"claude": {"tool": "claude"}}}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_old_promoted_shape() {
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(
            repo.path(),
            r#"{"team": [{"include": "dev"}], "promoted": "codex"}"#,
        );
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn try_resolve_via_team_with_fails_closed_on_legacy_agents_array() {
        // A pre-team `agents` array (old shape) → re-init hint, not a
        // cryptic serde error.
        let repo = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_repo_config(repo.path(), r#"{"agents": ["claude", "codex"]}"#);
        let err = try_resolve_via_team_with(repo.path(), Some(home.path())).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("old team schema") && msg.contains("clank init"),
            "expected re-init hint; got: {msg}"
        );
    }

    #[test]
    fn set_auto_mode_round_trips_and_preserves_unrelated_fields() {
        let repo = TempDir::new().unwrap();
        let lbl = label("codex");
        // Seed a config carrying a field the toggle has no business
        // touching. (This once guarded the per-agent poll bound;
        // that field is gone, but the merge-not-overwrite contract it
        // was really testing is not — so it guards a surviving one.)
        let seed = AgentConfig {
            wait_events: vec![clank_core::agent_config::WaitEventSource::Github(
                clank_core::agent_config::GithubSource {
                    interrupt: false,
                    interrupt_message: None,
                    repo: "o/r".into(),
                    events: vec![clank_core::agent_config::GithubEventKind::PrOpened],
                    poll_interval: None,
                    watch: Vec::new(),
                    include_own_actions: false,
                    branches: Vec::new(),
                    prs: Vec::new(),
                    prompt: None,
                },
            )],
            ..AgentConfig::default()
        };
        update_agent_config(repo.path(), &lbl, |cfg| *cfg = seed.clone()).unwrap();

        set_auto_mode(repo.path(), &lbl, AutoMode::On).unwrap();
        let armed = load_agent_config(repo.path(), &lbl).unwrap().unwrap();
        assert_eq!(armed.auto_mode, Some(AutoMode::On));
        assert_eq!(armed.wait_events, seed.wait_events, "preserved on arm");

        set_auto_mode(repo.path(), &lbl, AutoMode::Off).unwrap();
        let disarmed = load_agent_config(repo.path(), &lbl).unwrap().unwrap();
        assert_eq!(disarmed.auto_mode, Some(AutoMode::Off));
        assert_eq!(
            disarmed.wait_events, seed.wait_events,
            "preserved on disarm"
        );
    }

    #[test]
    fn set_auto_mode_defaults_a_missing_config() {
        let repo = TempDir::new().unwrap();
        let lbl = label("codex");
        // No file yet: the RMW defaults, sets the mode, and writes.
        set_auto_mode(repo.path(), &lbl, AutoMode::On).unwrap();
        let cfg = load_agent_config(repo.path(), &lbl).unwrap().unwrap();
        assert_eq!(cfg.auto_mode, Some(AutoMode::On));
    }

    const RESUMED: &str = "742f6a04-f174-409a-ab01-419a16c5f372";
    const SUCCESSOR: &str = "0f3e5c2a-1b4d-4e6f-8a9b-0c1d2e3f4a5b";

    fn sid(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    fn at(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).unwrap()
    }

    fn pending(repo: &Path, l: &AgentLabel) -> Option<ResumePending> {
        resume_pending(repo, l).unwrap().1
    }

    #[test]
    fn only_a_binding_whose_hook_recorded_a_transcript_is_armed() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        let now = at("2026-11-03T00:00:00Z");
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        assert!(!arm_resume(repo, &l, &sid(RESUMED), now).unwrap());
        assert_eq!(pending(repo, &l), None);
        assert!(record_transcript(repo, &l, &sid(RESUMED), "/t/a.jsonl").unwrap());
        assert!(!arm_resume(repo, &l, &sid(SUCCESSOR), now).unwrap());
        assert_eq!(pending(repo, &l), None, "not the bound session");
        assert!(arm_resume(repo, &l, &sid(RESUMED), now).unwrap());
        assert_eq!(
            pending(repo, &l),
            Some(ResumePending {
                at: "2026-11-03T00:00:00Z".into(),
            })
        );
    }

    fn told(repo: &Path, l: &AgentLabel) -> Option<OutageNotified> {
        state::State::load(&agents_root(repo).join(l.as_str()))
            .unwrap()
            .current(Bound::Outage)
            .unwrap()
            .map(|(_, told)| told)
    }

    fn announce(repo: &Path, l: &AgentLabel, session: &str, at_: &str) -> bool {
        announce_outage(repo, l, &sid(session), at(at_)).unwrap()
    }

    #[test]
    fn an_outage_is_announced_once_to_the_bound_session_until_a_monitor_serves_it() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        let now = "2026-11-10T00:00:00Z";
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        assert!(!announce(repo, &l, SUCCESSOR, now));
        assert_eq!(told(repo, &l), None, "not the bound session");
        assert_eq!(
            outage_served(repo, &l, &sid(RESUMED)).unwrap(),
            OutageServed::NoneStood
        );

        assert!(announce(repo, &l, RESUMED, now));
        assert!(!announce(repo, &l, RESUMED, "2026-11-10T00:05:00Z"));
        assert_eq!(told(repo, &l).unwrap().at, now);

        assert_eq!(
            outage_served(repo, &l, &sid(SUCCESSOR)).unwrap(),
            OutageServed::NoneStood
        );
        assert!(
            told(repo, &l).is_some(),
            "another session's monitor clears nothing"
        );
        assert_eq!(
            outage_served(repo, &l, &sid(RESUMED)).unwrap(),
            OutageServed::Cleared
        );
        assert_eq!(told(repo, &l), None);
        assert!(announce(repo, &l, RESUMED, now));
    }

    /// Two waiters of one session that reach the announcement together
    /// tell it once.
    #[test]
    fn two_waiters_announcing_together_tell_once() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        let together = std::sync::Barrier::new(2);
        let told = std::thread::scope(|scope| {
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        together.wait();
                        announce(repo, &l, RESUMED, "2026-11-10T00:00:00Z")
                    })
                })
                .collect();
            waiters
                .into_iter()
                .map(|w| w.join().unwrap())
                .filter(|told| *told)
                .count()
        });
        assert_eq!(told, 1);
    }

    /// A serve that finds the agent's lock held clears nothing, and the
    /// next serve ends the outage.
    #[test]
    fn a_session_served_while_its_lock_is_held_is_cleared_by_the_next_serve() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        assert!(announce(repo, &l, RESUMED, "2026-11-10T00:00:00Z"));
        let held =
            crate::cli::github::log::tests::hold_paths(&[agents_root(repo).join(l.as_str())]);
        assert_eq!(
            outage_served(repo, &l, &sid(RESUMED)).unwrap(),
            OutageServed::Contended
        );
        assert!(told(repo, &l).is_some(), "the notice stands");
        held.release();
        assert_eq!(
            outage_served(repo, &l, &sid(RESUMED)).unwrap(),
            OutageServed::Cleared
        );
        assert_eq!(told(repo, &l), None);
    }

    #[test]
    fn re_arming_the_same_session_keeps_the_first_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        record_transcript(repo, &l, &sid(RESUMED), "/t/a.jsonl").unwrap();
        arm_resume(repo, &l, &sid(RESUMED), at("2026-11-03T00:00:00Z")).unwrap();
        arm_resume(repo, &l, &sid(RESUMED), at("2026-11-03T00:00:50Z")).unwrap();
        assert_eq!(pending(repo, &l).unwrap().at, "2026-11-03T00:00:00Z");
        assert!(resume_unconfirmed(
            pending(repo, &l).as_ref(),
            at("2026-11-03T00:01:00Z")
        ));
    }

    #[test]
    fn a_check_in_clears_only_its_own_sessions_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, l) = (dir.path(), label("claude"));
        bind_session_to_agent(repo, &l, Tool::Claude, &sid(RESUMED)).unwrap();
        record_transcript(repo, &l, &sid(RESUMED), "/t/a.jsonl").unwrap();
        arm_resume(repo, &l, &sid(RESUMED), at("2026-11-03T00:00:00Z")).unwrap();
        assert!(!resume_checked_in(repo, &l, &sid(SUCCESSOR)).unwrap());
        assert!(pending(repo, &l).is_some());
        assert!(resume_checked_in(repo, &l, &sid(RESUMED)).unwrap());
        assert_eq!(pending(repo, &l), None);
    }
}
