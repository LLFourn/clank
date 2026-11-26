//! What a press sends, and the only way to obtain a [`Command`] the
//! user-command writer will type.

use clank_core::ids::{AgentLabel, SessionId};
use std::path::PathBuf;

/// The longest request id.
pub(crate) const ID_MAX: usize = 64;
/// The longest command admitted at all. The bound a dispatch types is the
/// lower one its evidence observed (`pane_input::USER_COMMAND_EVIDENCE`).
pub(crate) const COMMAND_LIMIT: usize = 4096;
/// The longest request read from stdin.
pub(crate) const REQUEST_MAX: usize = 64 * 1024;

/// A request as `submit` reads it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    repo: String,
    agent: String,
    session: String,
    id: String,
    command: String,
}

/// One press: type `command` at `agent`'s idle prompt in `repo`, while
/// `session` holds that label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) repo: PathBuf,
    pub(crate) agent: AgentLabel,
    pub(crate) session: SessionId,
    pub(crate) id: Id,
    pub(crate) command: Command,
}

impl Request {
    /// One request from `text`, or why it is refused.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        if text.len() > REQUEST_MAX {
            return Err("the request is too long".into());
        }
        let w: Wire = serde_json::from_str(text).map_err(|e| format!("malformed request: {e}"))?;
        let bad = |why: String| format!("invalid request: {why}");
        let repo = PathBuf::from(&w.repo);
        if !repo.is_absolute() {
            return Err(bad("repo is not an absolute path".into()));
        }
        Ok(Request {
            repo,
            agent: AgentLabel::parse(&w.agent).map_err(|e| bad(format!("agent: {e}")))?,
            session: SessionId::parse(&w.session).map_err(|e| bad(format!("session: {e}")))?,
            id: Id::parse(&w.id).map_err(bad)?,
            command: Command::parse(w.command).map_err(bad)?,
        })
    }
}

/// A request id: a bounded `[A-Za-z0-9_-]` token, safe as a filename.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Id(String);

impl Id {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let ok = !text.is_empty()
            && text.len() <= ID_MAX
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        ok.then(|| Id(text.to_string()))
            .ok_or_else(|| format!("an id is 1 to {ID_MAX} of [A-Za-z0-9_-]"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One non-empty line of at most [`COMMAND_LIMIT`] characters with no
/// control character: the only text the user-command writer types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Command(String);

impl Command {
    pub(crate) fn parse(line: String) -> Result<Self, String> {
        if line.trim().is_empty() {
            Err("the command is empty".into())
        } else if line.chars().count() > COMMAND_LIMIT {
            Err("the command is too long".into())
        } else if line.chars().any(char::is_control) {
            Err("the command is not one line without control characters".into())
        } else {
            Ok(Command(line))
        }
    }

    pub(crate) fn line(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(line: &str) -> Self {
        Command(line.to_string())
    }
}

impl TryFrom<String> for Command {
    type Error = String;
    fn try_from(line: String) -> Result<Self, String> {
        Command::parse(line)
    }
}

impl From<Command> for String {
    fn from(command: Command) -> String {
        command.0
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::Path;

    /// A valid request for `repo`, agent `claude`, session `session-one`,
    /// id `r1`.
    pub(crate) fn request(repo: &Path) -> serde_json::Value {
        serde_json::json!({
            "repo": repo.to_string_lossy(),
            "agent": "claude",
            "session": "session-one",
            "id": "r1",
            "command": "git push origin feature",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::request;
    use super::*;
    use std::path::Path;

    fn repo() -> &'static Path {
        Path::new("/w/proj")
    }

    fn parse(v: &serde_json::Value) -> Result<Request, String> {
        Request::parse(&v.to_string())
    }

    #[test]
    fn a_request_carries_its_fields() {
        let r = parse(&request(repo())).unwrap();
        assert_eq!(r.repo, repo());
        assert_eq!(r.agent.as_str(), "claude");
        assert_eq!(r.session.as_str(), "session-one");
        assert_eq!(r.id.as_str(), "r1");
        assert_eq!(r.command.line(), "git push origin feature");
    }

    /// Malformed text, a missing or unknown field, a bad id, a multi-line,
    /// escaped or empty command, a relative repo, and an oversized request
    /// are each refused.
    #[test]
    fn an_invalid_request_is_refused() {
        assert!(Request::parse("").is_err());
        assert!(Request::parse("{}").is_err());
        assert!(Request::parse(&"x".repeat(REQUEST_MAX + 1)).is_err());
        let mut missing = request(repo());
        missing.as_object_mut().unwrap().remove("session");
        assert!(parse(&missing).is_err());
        let cases: [(&str, serde_json::Value); 8] = [
            ("id", "../x".into()),
            ("id", "x".repeat(ID_MAX + 1).into()),
            ("command", "ls\nrm -rf ~".into()),
            ("command", "ls\u{1b}[A".into()),
            ("command", "   ".into()),
            ("command", "x".repeat(COMMAND_LIMIT + 1).into()),
            ("repo", "relative/repo".into()),
            ("extra", 1.into()),
        ];
        for (field, value) in cases {
            let mut r = request(repo());
            r[field] = value;
            assert!(parse(&r).is_err(), "{field}");
        }
    }

    /// A stored command is checked as it is read back, so a record edited
    /// to hold a second line never yields one.
    #[test]
    fn a_stored_command_is_checked_on_load() {
        let ok: Command = serde_json::from_value("ls -l".into()).unwrap();
        assert_eq!(ok.line(), "ls -l");
        assert!(serde_json::from_value::<Command>("ls\nrm -rf ~".into()).is_err());
    }
}
