//! An OpenCode session clank binds before launch, and what OpenCode's own
//! store says of a bound one. OpenCode 2.1.4's `--session <id>` stores
//! nothing until a prompt is submitted
//! (`tests/fixtures/opencode-2.1.4-session.json`), so clank has OpenCode
//! create the session first, binds only the id OpenCode returns, and then
//! tells a shared server's plugin it is bound ([`announce_bound`]).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

use super::opencode_fork::{Api, Backend, CallError, same_dir};
use clank_core::agent_config::Session;
use clank_core::ids::{AgentLabel, SessionId};

#[cfg(test)]
#[path = "opencode_session_tests.rs"]
pub(crate) mod tests;

/// What OpenCode's own store says about one session: the one record of it
/// that does not depend on clank's plugin having loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stored {
    /// Stored; its latest execution settled at `unix_secs`.
    At { unix_secs: u64, outcome: String },
    /// Stored, and no execution of it has settled.
    Never,
    /// The store was read and holds no such session.
    Missing,
    /// Whether it is stored cannot be told.
    Unknown(String),
}

/// The session store a launch reads.
pub(crate) struct Store(Result<PathBuf, String>);

impl Store {
    /// The store an OpenCode launch with `server` (the profile's
    /// server-selecting args) and environment `var` reads: `OPENCODE_DB`
    /// resolved in its data dir, `opencode.db` by default (`mon` in the
    /// 2.1.4 binary). A `--server` keeps its sessions out of reach.
    pub(crate) fn of(server: &[String], var: &dyn Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| var(k).filter(|v| !v.is_empty());
        if server.iter().any(|a| a.starts_with("--server")) {
            return Self(Err(
                "the profile's `--server` keeps its sessions in that server's store".into(),
            ));
        }
        let Some(data) = var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| Path::new(&h).join(".local/share")))
        else {
            return Self(Err("neither XDG_DATA_HOME nor HOME is set".into()));
        };
        let name = var("OPENCODE_DB").unwrap_or_else(|| "opencode.db".into());
        if name == ":memory:" {
            return Self(Err("the profile keeps OpenCode's store in memory".into()));
        }
        Self(Ok(data.join("opencode").join(name)))
    }

    /// Read-only lookup of one session's row in OpenCode 2.x's store. Why
    /// not a SQLite crate: one bounded read does not earn a bundled C
    /// dependency, and without `sqlite3` the answer is [`Stored::Unknown`]
    /// rather than a guess.
    pub(crate) fn lookup(&self, session: &str) -> Stored {
        let db = match &self.0 {
            Ok(db) if db.is_file() => db,
            Ok(_) => return Stored::Unknown("OpenCode's store was not found".into()),
            Err(why) => return Stored::Unknown(why.clone()),
        };
        if !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Stored::Unknown(format!("`{session}` is not an OpenCode session id"));
        }
        // Its own `.timeout 1000` bounds the lock wait; this bounds the rest.
        const SQLITE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
        use crate::proc_run::{Group, Out, RunError, Spec};
        let out = crate::proc_run::run_blocking(
            Spec::new("sqlite3")
                .arg("-readonly")
                .args(["-cmd", ".timeout 1000"])
                .arg(format!("file:{}?mode=ro", db.display()))
                .arg(format!(
                    "select coalesce(time_idle, ''), coalesce(idle_outcome, '') \
                     from session_v2 where id = '{session}'"
                ))
                .stdout(Out::Capture { cap: 1 << 20 })
                .stderr(Out::Ring { bytes: 4096 })
                .group(Group::New),
            SQLITE_BUDGET,
        );
        match out {
            Ok(out) if out.status.success() => parse_row(&String::from_utf8_lossy(&out.stdout)),
            Ok(out) => Stored::Unknown(format!(
                "sqlite3 could not read OpenCode's store: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(RunError::Spawn(e)) => Stored::Unknown(format!("sqlite3 is not available: {e}")),
            Err(e) => Stored::Unknown(format!("sqlite3 could not read OpenCode's store: {e}")),
        }
    }
}

/// A query that succeeded with no row is the store saying it has no such
/// session.
fn parse_row(row: &str) -> Stored {
    let row = row.trim();
    if row.is_empty() {
        return Stored::Missing;
    }
    let (idle, outcome) = row.split_once('|').unwrap_or((row, ""));
    match idle.parse::<u64>() {
        Ok(ms) => Stored::At {
            unix_secs: ms / 1000,
            outcome: outcome.to_string(),
        },
        Err(_) if idle.is_empty() => Stored::Never,
        Err(_) => Stored::Unknown(format!("unexpected row `{row}`")),
    }
}

/// A new id in OpenCode 2.1.4's own session format (`Session.ID.create`
/// in the shipped binary): `ses_`, 12 hex digits of the bitwise NOT of
/// `ms * 0x1000 + counter`, so newer sessions sort first (a start mints
/// one id, so its counter is 1), then 14 random base62 characters.
pub(crate) fn mint_session_id() -> SessionId {
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut random = [0u8; 14];
    getrandom::fill(&mut random).expect("the OS can produce random bytes");
    let tail: String = random
        .iter()
        .map(|b| BASE62[usize::from(*b) % BASE62.len()] as char)
        .collect();
    let id = format!("ses_{:012x}{tail}", !((ms << 12) + 1) & 0xffff_ffff_ffff);
    SessionId::parse(&id).expect("an OpenCode session id is a valid session id")
}

/// Have OpenCode create a session in `backend.cwd`, answering its id only
/// once OpenCode's reply names it stored there.
fn create(api: &dyn Api, backend: &Backend) -> anyhow::Result<SessionId> {
    let id = mint_session_id();
    let body = json!({ "id": id.as_str(), "location": { "directory": backend.cwd } });
    let reply = api
        .call(backend, "session.create", id.as_str(), Some(body))
        .map_err(|e| match e {
            CallError::Refused(e) => e.context("OpenCode did not create a session"),
            CallError::Unknown(e) => e.context(format!(
                "OpenCode may have created session {id}; it is not bound, and the next start \
                 creates another"
            )),
        })?;
    let named = reply.get("id").and_then(Value::as_str);
    let stored = reply.pointer("/location/directory").and_then(Value::as_str);
    anyhow::ensure!(
        named == Some(id.as_str()) && stored.is_some_and(|d| same_dir(Path::new(d), &backend.cwd)),
        "OpenCode's reply to creating session {id} in `{}` does not confirm it: {reply}",
        backend.cwd.display()
    );
    Ok(id)
}

/// The session metadata that tells clank's plugin `label` is bound to the
/// session, on a session with no other metadata. The plugin clears it once
/// a wait it started on it has run.
pub fn bound_marker(label: &AgentLabel) -> Value {
    with_marker(None, label)
}

/// `current` session metadata with clank's bound marker for `label` set,
/// and everything else in it kept.
fn with_marker(current: Option<&Value>, label: &AgentLabel) -> Value {
    let mut metadata = current
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut clank = metadata
        .remove("clank")
        .and_then(|c| c.as_object().cloned())
        .unwrap_or_default();
    clank.insert("bound".into(), label.as_str().into());
    metadata.insert("clank".into(), Value::Object(clank));
    Value::Object(metadata)
}

fn standalone(backend: &Backend) -> bool {
    backend.server.iter().any(|a| a == "--standalone")
}

/// 2.1.4's `session.update` replaces the metadata whole and offers no
/// conditional write, so the marker is set on what `session.get` answered
/// just before: a write by anyone else between the two is lost.
fn set_marker(
    api: &dyn Api,
    backend: &Backend,
    label: &AgentLabel,
    session: &SessionId,
    current: Option<&Value>,
) -> Result<(), CallError> {
    let body = json!({ "metadata": with_marker(current, label) });
    api.call(backend, "session.update", session.as_str(), Some(body))
        .map(|_| ())
}

/// Tell the plugin of the server `backend` reaches that `label` is bound to
/// `session`, through OpenCode's `session.metadata.updated`. That server
/// may have seen the session's creation before the binding existed, and
/// only an event after the binding arms it. A `--standalone` pane runs its
/// own server, whose plugin loads after the binding and arms from
/// CLANK_BOOTSTRAP_SESSION_ID; telling the private server each `opencode
/// api` call starts would arm a wait in a process that is about to exit.
fn announce(
    api: &dyn Api,
    backend: &Backend,
    label: &AgentLabel,
    session: &SessionId,
) -> Result<(), CallError> {
    if standalone(backend) {
        return Ok(());
    }
    let info = api.call(backend, "session.get", session.as_str(), None)?;
    set_marker(api, backend, label, session, info.get("metadata"))
}

/// Write `session`'s bound marker again when it still stands: no wait
/// started on it has run, so the plugin that heard it could not arm (its
/// idle check failed, or it was unloaded first). The rewrite is a new
/// `session.metadata.updated` for whichever instance is loaded now.
/// Answers whether it was rewritten.
pub(crate) fn reannounce_pending(
    api: &dyn Api,
    backend: &Backend,
    label: &AgentLabel,
    session: &SessionId,
) -> Result<bool, CallError> {
    if standalone(backend) {
        return Ok(false);
    }
    let info = api.call(backend, "session.get", session.as_str(), None)?;
    let metadata = info.get("metadata");
    if metadata
        .and_then(|m| m.pointer("/clank/bound"))
        .is_none_or(|b| !b.is_string())
    {
        return Ok(false);
    }
    set_marker(api, backend, label, session, metadata).map(|()| true)
}

/// [`announce`] `session`, just bound under `guard`, before the lock lets
/// any other start see the binding; one OpenCode could not be told of is
/// undone.
pub(crate) fn announce_bound(
    guard: &crate::agent_store::StateGuard,
    api: &dyn Api,
    backend: &Backend,
    label: &AgentLabel,
    session: &SessionId,
) -> anyhow::Result<()> {
    let Err(e) = announce(api, backend, label, session) else {
        return Ok(());
    };
    let undone = crate::agent_store::unbind_locked(guard, session);
    let (CallError::Refused(e) | CallError::Unknown(e)) = e;
    Err(e.context(format!(
        "`{label}` was not launched: OpenCode was not told session {} is bound, so its \
         plugin would never wait for work; {}",
        session.as_str(),
        match undone {
            Ok(_) => "the binding is undone, and the next start binds it again".to_string(),
            Err(u) => format!("undoing the binding failed too: {u:#}"),
        }
    )))
}

/// Bind `label` to a session OpenCode has just created, in place of
/// `replaces`: the binding the caller judged replaceable, and
/// [`announce_bound`] it. A session another start bound meanwhile is
/// returned instead, and no session is created.
pub(crate) fn bind_created(
    repo: &Path,
    label: &AgentLabel,
    backend: &Backend,
    replaces: Option<&Session>,
    api: &dyn Api,
) -> anyhow::Result<Session> {
    let not_launched = || format!("`{label}` was not launched: its session could not be bound");
    let guard = crate::agent_store::hold_state(repo, label).with_context(not_launched)?;
    let mut created = None;
    let session = crate::agent_store::reserve_opencode_locked(&guard, replaces, || {
        let id = create(api, backend)?;
        created = Some(id.clone());
        Ok(id)
    })
    .with_context(not_launched)?;
    if created.as_ref() == Some(&session.id) {
        announce_bound(&guard, api, backend, label, &session.id)?;
    }
    Ok(session)
}
