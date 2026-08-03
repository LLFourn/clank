//! Env-var reads for agent context — the single CLI-side place
//! that knows about `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`,
//! and `CLANK_AGENT`. Every command that builds `IdentityInputs`
//! goes through here.

use std::env;
use std::path::Path;

use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::Tool;
use clank_core::{IdentityInputs, ResolveError, resolve_agent_identity};

use crate::agent_store::load_all_agent_configs_lossy;

const ENV_CLAUDE_SESSION: &str = "CLAUDE_CODE_SESSION_ID";
const ENV_CODEX_SESSION: &str = "CODEX_THREAD_ID";
/// Grok sets NO session id in tool subprocess envs — only this marker
/// (grok-first-class P2). Detection resolves the session as the
/// newest session dir for the cwd (see [`grok_session_for_cwd`]).
const ENV_GROK_MARKER: &str = "GROK_AGENT";
/// Injected by the clank opencode PLUGIN via opencode's `shell.env`
/// hook — opencode itself exports no session id
/// (opencode-agent-tool M0). Present iff the shell call carried the
/// exact session id; the plugin never guesses.
const ENV_OPENCODE_SESSION: &str = "OPENCODE_SESSION_ID";
const ENV_CLANK_AGENT: &str = "CLANK_AGENT";

/// Every env var that carries an agent IDENTITY (session ids,
/// markers, the explicit label override). Fresh launches scrub these
/// from the child env so an agent never inherits its parent's
/// identity; each tool re-exports its own to its shells.
pub(crate) const SESSION_IDENTITY_VARS: &[&str] = &[
    ENV_CLAUDE_SESSION,
    ENV_CODEX_SESSION,
    ENV_OPENCODE_SESSION,
    ENV_GROK_MARKER,
    ENV_CLANK_AGENT,
];

/// Try to detect (tool, session_id) from the env vars set by the
/// running agent. Returns `Ok(Some(...))` when exactly one of the
/// session env vars is set (the common case); `Ok(None)` when
/// none is set; `Err(ConflictingSessions)` when several are set
/// (e.g. a codex shell spawned from claude leaking
/// `CLAUDE_CODE_SESSION_ID` into the child env). Callers that
/// require a session bind the OK-None case to their own error
/// shape.
/// Pure, tool-neutral selection over the EXPLICIT session env vars
/// (codex 5395a5d): exactly one set → that tool claims the session;
/// two or more → the leak error naming every conflicting var; none →
/// `None` (marker-based fallbacks like grok run in the caller).
/// Pure so every combination is testable without touching process
/// env.
fn select_explicit_session(
    candidates: &[(&'static str, Tool, Option<String>)],
) -> Result<Option<(Tool, String)>, EnvError> {
    // A BLANK var reads as unset: environments that can only SET
    // vars, never unset them (opencode's shell.env hook), scrub
    // foreign session vars by blanking, and a blank must neither
    // claim the session nor count as a conflict.
    let set: Vec<(&'static str, Tool, &String)> = candidates
        .iter()
        .filter_map(|(var, tool, v)| v.as_ref().map(|v| (*var, *tool, v)))
        .filter(|(_, _, v)| !v.is_empty())
        .collect();
    match set.as_slice() {
        [] => Ok(None),
        [(_, tool, raw)] => Ok(Some((*tool, (*raw).clone()))),
        many => Err(EnvError::ConflictingSessions {
            vars: many
                .iter()
                .map(|(var, _, value)| (*var, (*value).clone()))
                .collect(),
        }),
    }
}

pub fn detect_session_from_env() -> Result<Option<(Tool, SessionId)>, EnvError> {
    let explicit = select_explicit_session(&[
        (
            ENV_CLAUDE_SESSION,
            Tool::Claude,
            env::var(ENV_CLAUDE_SESSION).ok(),
        ),
        (
            ENV_CODEX_SESSION,
            Tool::Codex,
            env::var(ENV_CODEX_SESSION).ok(),
        ),
        (
            ENV_OPENCODE_SESSION,
            Tool::OpenCode,
            env::var(ENV_OPENCODE_SESSION).ok(),
        ),
    ])?;
    match explicit {
        Some((tool, raw)) => Ok(Some((tool, parse_session_id(&raw, tool)?))),
        // Grok last: its marker carries no session id, and an explicit
        // claude/codex session var (even one leaked from a parent
        // shell) is stronger evidence than the bare marker.
        None => {
            // Blank marker = scrubbed-by-blanking, same as the
            // explicit vars above.
            if !env::var(ENV_GROK_MARKER).is_ok_and(|v| !v.is_empty()) {
                return Ok(None);
            }
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let cwd = std::env::current_dir().ok();
            match (home, cwd) {
                (Some(home), Some(cwd)) => {
                    Ok(grok_session_for_cwd(&home, &cwd).map(|sid| (Tool::Grok, sid)))
                }
                _ => Ok(None),
            }
        }
    }
}

/// The running grok session for `cwd`: the newest-mtime session dir
/// under `~/.grok/sessions/<percent-encoded cwd>/` (grok groups
/// sessions by URL-encoded working directory; the live session's dir
/// is the most recently written — grok-first-class P2). `None` when
/// the group doesn't exist. Two concurrent grok sessions in one
/// directory can misresolve — `clank as` echoes the binding so the
/// user can catch and rebind.
fn grok_session_for_cwd(home: &std::path::Path, cwd: &std::path::Path) -> Option<SessionId> {
    let mut encoded = String::new();
    for b in cwd.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    let group = home.join(".grok/sessions").join(encoded);
    let mut newest: Option<(std::time::SystemTime, SessionId)> = None;
    for entry in std::fs::read_dir(group).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(sid) = SessionId::parse(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, sid));
        }
    }
    newest.map(|(_, sid)| sid)
}

/// Read the explicit-override label from `CLANK_AGENT`. Empty
/// string is treated as "unset" so users can clear the override
/// with `CLANK_AGENT= clank ...`.
pub fn explicit_label_from_env() -> Result<Option<AgentLabel>, EnvError> {
    let Ok(raw) = env::var(ENV_CLANK_AGENT) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    AgentLabel::parse(&raw)
        .map(Some)
        .map_err(|e| EnvError::InvalidLabel {
            var: ENV_CLANK_AGENT,
            value: raw,
            reason: e.to_string(),
        })
}

fn parse_session_id(raw: &str, tool: Tool) -> Result<SessionId, EnvError> {
    SessionId::parse(raw).map_err(|e| EnvError::InvalidSessionId {
        var: match tool {
            Tool::Claude => ENV_CLAUDE_SESSION,
            Tool::Codex => ENV_CODEX_SESSION,
            Tool::Grok => ENV_GROK_MARKER,
            Tool::OpenCode => ENV_OPENCODE_SESSION,
        },
        value: raw.to_string(),
        reason: e.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvError {
    /// Two or more explicit session env vars are set — tool-neutral
    /// (opencode joined the set; codex 5395a5d): almost always a var
    /// leaked from a parent shell; the caller should disambiguate
    /// (e.g. via an explicit `--tool` flag in the hook config).
    /// Carries every conflicting (var, value) for diagnostics — the
    /// user needs to know which one to expect to keep.
    ConflictingSessions { vars: Vec<(&'static str, String)> },
    InvalidSessionId {
        var: &'static str,
        value: String,
        reason: String,
    },
    InvalidLabel {
        var: &'static str,
        value: String,
        reason: String,
    },
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::ConflictingSessions { vars } => {
                let listing = vars
                    .iter()
                    .map(|(var, value)| format!("{var}={value:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "multiple session env vars are set ({listing}) — ambiguous. \
                     One probably leaked from a parent shell; restart the inner \
                     agent or pass --tool explicitly."
                )
            }
            EnvError::InvalidSessionId { var, value, reason } => {
                write!(f, "{var}={value:?} is not a valid session id: {reason}")
            }
            EnvError::InvalidLabel { var, value, reason } => {
                write!(f, "{var}={value:?} is not a valid agent label: {reason}")
            }
        }
    }
}

impl std::error::Error for EnvError {}

/// High-level wrapper: read env, load this repo's agent configs,
/// and call the pure `resolve_agent_identity`. The single way
/// any non-hook CLI command finds out "who am I" — used by
/// `clank auto`, `clank wait`, `clank feedback write` (later),
/// and `clank doctor`.
///
/// Stop-hook callers go through a different path because they
/// receive `session_id` via stdin and `tool` via `--tool`, not
/// from env — see `cli::stop_hook` (later commit).
pub fn resolve_identity_from_env(repo: &Path) -> anyhow::Result<AgentLabel> {
    // Highest precedence: explicit CLANK_AGENT override. Short-
    // circuit BEFORE touching session env — otherwise lower-
    // precedence env state (BothToolsDetected, an unparseable
    // session id, etc.) could fail the command even though the
    // resolver would happily return the explicit label. This
    // mirrors the pure resolver's precedence rule and is what
    // codex's review on `99ce55b` flagged.
    if let Some(label) = explicit_label_from_env()? {
        return Ok(label);
    }

    let detected = detect_session_from_env()?;
    let agent_configs = load_all_agent_configs_lossy(repo)?;
    let (tool, session_id_owned) = match detected {
        Some((t, sid)) => (t, Some(sid)),
        None => {
            // No session detected. The resolver will return
            // NoSession; we map that to a friendly error below.
            // Default tool to Claude — it doesn't matter since
            // the lookup path won't run without a session id.
            (Tool::Claude, None)
        }
    };
    let inputs = IdentityInputs {
        tool,
        explicit_label: None,
        session_id: session_id_owned.as_ref(),
        agent_configs: &agent_configs,
    };
    resolve_agent_identity(&inputs).map_err(|e| match e {
        ResolveError::NoSession => anyhow::anyhow!(
            "no session detected — pass --author <label>, set CLANK_AGENT, \
             or run inside claude/codex/grok/opencode"
        ),
        ResolveError::NoAgentForSession { session_id, tool } => anyhow::anyhow!(
            "no agent is bound to {tool} session {sid} in this repo — run \
             `clank as <label>` first (inside this session) or `clank init` \
             to bootstrap",
            tool = tool.as_str(),
            sid = session_id.as_str(),
        ),
    })
}

/// Hook-specific identity resolution: caller passes `tool`
/// (from `--tool`) and `session_id` (from hook stdin); env
/// `CLANK_AGENT` still wins if set. Used by `clank stop-hook`,
/// NOT by regular CLI commands (which read session_id from env).
pub fn resolve_identity_for_hook(
    repo: &Path,
    tool: Tool,
    session_id: &SessionId,
) -> anyhow::Result<AgentLabel> {
    if let Some(label) = explicit_label_from_env()? {
        return Ok(label);
    }
    let agent_configs = load_all_agent_configs_lossy(repo)?;
    let inputs = IdentityInputs {
        tool,
        explicit_label: None,
        session_id: Some(session_id),
        agent_configs: &agent_configs,
    };
    resolve_agent_identity(&inputs).map_err(|e| match e {
        ResolveError::NoSession => {
            anyhow::anyhow!("hook called without a session id (internal: caller passed Some)")
        }
        ResolveError::NoAgentForSession {
            session_id: id,
            tool: t,
        } => anyhow::anyhow!(
            "no agent set up for {tool} session {sid} — run \
             `clank as <label>` (inside this session) to bootstrap",
            tool = t.as_str(),
            sid = id.as_str(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(
        claude: Option<&str>,
        codex: Option<&str>,
        opencode: Option<&str>,
    ) -> Vec<(&'static str, Tool, Option<String>)> {
        vec![
            (ENV_CLAUDE_SESSION, Tool::Claude, claude.map(str::to_owned)),
            (ENV_CODEX_SESSION, Tool::Codex, codex.map(str::to_owned)),
            (
                ENV_OPENCODE_SESSION,
                Tool::OpenCode,
                opencode.map(str::to_owned),
            ),
        ]
    }

    #[test]
    fn selector_claims_each_single_tool() {
        // Pure over the candidates — no process env (codex 5395a5d).
        for (c, x, o, want) in [
            (Some("a-claude-id"), None, None, Tool::Claude),
            (None, Some("a-codex-id"), None, Tool::Codex),
            (None, None, Some("ses_0123456789ab"), Tool::OpenCode),
        ] {
            let got = select_explicit_session(&cands(c, x, o)).unwrap().unwrap();
            assert_eq!(got.0, want);
        }
        assert!(
            select_explicit_session(&cands(None, None, None))
                .unwrap()
                .is_none(),
            "none set defers to marker fallbacks"
        );
    }

    #[test]
    fn selector_treats_blank_vars_as_unset() {
        // Scrub-by-blanking: opencode's shell.env hook can only SET
        // vars, so the clank plugin blanks foreign session vars. A
        // blank neither claims the session nor conflicts.
        let out = select_explicit_session(&[
            ("CLAUDE_CODE_SESSION_ID", Tool::Claude, Some(String::new())),
            ("CODEX_THREAD_ID", Tool::Codex, None),
            (
                "OPENCODE_SESSION_ID",
                Tool::OpenCode,
                Some("ses_039d60658ffe0RPgue3noZ0Qqf".into()),
            ),
        ]);
        assert_eq!(
            out.unwrap(),
            Some((Tool::OpenCode, "ses_039d60658ffe0RPgue3noZ0Qqf".into()))
        );
        let out = select_explicit_session(&[
            ("CLAUDE_CODE_SESSION_ID", Tool::Claude, Some(String::new())),
            ("CODEX_THREAD_ID", Tool::Codex, Some(String::new())),
            ("OPENCODE_SESSION_ID", Tool::OpenCode, None),
        ]);
        assert_eq!(out.unwrap(), None);
    }

    #[test]
    fn selector_conflicts_name_every_var() {
        // Every pair + the three-way: the error lists exactly the
        // conflicting vars — never a claude/codex-shaped mislabel of
        // an opencode id (codex 5395a5d).
        let pairs: [(Option<&str>, Option<&str>, Option<&str>, &[&str]); 4] = [
            (
                Some("c"),
                Some("x"),
                None,
                &[ENV_CLAUDE_SESSION, ENV_CODEX_SESSION],
            ),
            (
                Some("c"),
                None,
                Some("ses_1234567890"),
                &[ENV_CLAUDE_SESSION, ENV_OPENCODE_SESSION],
            ),
            (
                None,
                Some("x"),
                Some("ses_1234567890"),
                &[ENV_CODEX_SESSION, ENV_OPENCODE_SESSION],
            ),
            (
                Some("c"),
                Some("x"),
                Some("ses_1234567890"),
                &[ENV_CLAUDE_SESSION, ENV_CODEX_SESSION, ENV_OPENCODE_SESSION],
            ),
        ];
        for (c, x, o, want_vars) in pairs {
            let err = select_explicit_session(&cands(c, x, o)).unwrap_err();
            let EnvError::ConflictingSessions { vars } = err else {
                panic!("expected ConflictingSessions");
            };
            let got: Vec<&str> = vars.iter().map(|(v, _)| *v).collect();
            assert_eq!(got, want_vars);
        }
    }
}
