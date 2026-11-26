//! `clank user-command`: a press on the story site types `!<command>`
//! into the idle prompt of the agent that asked.
//!
//! A convenience, not a security boundary: every agent and the site run as
//! the same Unix user, and any of them can type into any pane. `submit`
//! files a request after checking its target; only the monitor dispatches
//! it, at most once, into an idle, empty Claude Code prompt.

pub(crate) mod dispatch;
pub(crate) mod live;
pub(crate) mod request;
pub(crate) mod store;

use clank_core::Tool;
use clap::{Args, Subcommand};
use request::{Id, Request};
use std::io::Read;
use std::path::PathBuf;
use store::{Record, State, Status, Store};

/// `clank user-command`.
#[derive(Args, Debug)]
pub struct UserCommandArgs {
    #[command(subcommand)]
    pub command: UserCommandCmd,
}

#[derive(Subcommand, Debug)]
pub enum UserCommandCmd {
    /// File one request, read as JSON from stdin
    ///
    /// `{repo, agent, session, id, command}`. Prints the request's state as
    /// JSON. Only the repo's monitor dispatches it.
    Submit,
    /// A request's state, as JSON
    Status {
        /// The request's repo. Defaults to the cwd's git toplevel.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
        /// The agent it was filed for.
        #[arg(long, value_name = "AGENT")]
        agent: String,
        /// Its request id.
        #[arg(long, value_name = "ID")]
        id: String,
    },
}

pub async fn run(args: UserCommandArgs) -> anyhow::Result<()> {
    match args.command {
        UserCommandCmd::Submit => {
            let text = read_stdin(request::REQUEST_MAX)?;
            let admitted = admit(
                &Store::home()?,
                &text,
                crate::cli::recovery::unix_now(),
                target_check,
            );
            match admitted {
                Ok(status) => {
                    println!("{}", serde_json::to_string(&status)?);
                    Ok(())
                }
                Err(reason) => {
                    println!(
                        "{}",
                        serde_json::json!({"state": "refused", "reason": reason})
                    );
                    anyhow::bail!("refused: {reason}")
                }
            }
        }
        UserCommandCmd::Status { repo, agent, id } => {
            let repo = crate::cli::resolve_repo(repo.as_deref())?;
            let agent = clank_core::ids::AgentLabel::parse(&agent)?;
            let id = Id::parse(&id).map_err(anyhow::Error::msg)?;
            let tag = crate::cli::monitor::repo_tag(&repo);
            match Store::home()?.read(&tag, &agent, &id)? {
                Some(record) => {
                    println!("{}", serde_json::to_string(&record.status(&id))?);
                    Ok(())
                }
                None => anyhow::bail!(
                    "no request `{}` for `{agent}` in {}",
                    id.as_str(),
                    repo.display()
                ),
            }
        }
    }
}

fn read_stdin(max: usize) -> anyhow::Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .take(max as u64 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= max, "stdin is longer than {max} bytes");
    Ok(text)
}

/// Why a request for an agent bound to another tool is refused.
pub(crate) const NOT_CLAUDE: &str = "only Claude Code runs user commands; Codex, Grok and \
     OpenCode have no verified rendering of their shell mode";

/// Whether `bound`, an agent's binding now, is the Claude session `session`.
pub(crate) fn bound_to(
    bound: Option<(Tool, clank_core::ids::SessionId)>,
    session: &clank_core::ids::SessionId,
) -> Result<(), String> {
    match bound {
        Some((Tool::Claude, s)) if &s == session => Ok(()),
        Some((_, s)) if &s == session => Err(NOT_CLAUDE.into()),
        _ => Err("the session it names does not hold the label".into()),
    }
}

/// Whether a request's target is this machine's: its repo is a canonical
/// path, its agent is on that repo's roster, and the session it names is
/// the agent's Claude binding now.
fn target_check(r: &Request) -> Result<(), String> {
    let canonical = dunce::canonicalize(&r.repo).map_err(|e| format!("its repo: {e}"))?;
    if canonical != r.repo {
        return Err("its repo is not a canonical path".into());
    }
    crate::agent_store::resolve_role(&r.repo, &r.agent)
        .map_err(|_| format!("`{}` is not on that repo's roster", r.agent))?;
    let bound = crate::agent_store::binding(&r.repo, &r.agent)
        .ok()
        .flatten()
        .map(|s| (s.tool, s.id));
    bound_to(bound, &r.session)
}

/// File the request in `text`, after `check` accepts its target, under its
/// id. The same id asking for the same session and command answers with
/// the record as it stands, and rewrites a waiting one so the monitor looks
/// again; the same id with anything else, or a record this clank cannot
/// read, refuses and changes nothing. Nothing is filed for a request that
/// fails.
pub(crate) fn admit(
    store: &Store,
    text: &str,
    now: u64,
    check: impl Fn(&Request) -> Result<(), String>,
) -> Result<Status, String> {
    let request = Request::parse(text)?;
    check(&request)?;
    let tag = crate::cli::monitor::repo_tag(&request.repo);
    let held = store
        .lock(&tag, &request.agent, &request.id)
        .map_err(|e| format!("{e:#}"))?;
    let record = Record::admitted(request.session, request.command, now);
    match held.load().map_err(|e| format!("{e:#}"))? {
        Some(existing) => {
            if existing.asked != record.asked {
                return Err(format!(
                    "id `{}` already names a different request; nothing was changed",
                    request.id.as_str()
                ));
            }
            if matches!(existing.state, State::Admitted | State::WaitingForIdle) {
                held.save(&existing).map_err(|e| format!("{e:#}"))?;
            }
            Ok(existing.status(&request.id))
        }
        None => {
            held.save(&record).map_err(|e| format!("{e:#}"))?;
            Ok(record.status(&request.id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::request::fixtures::request;
    use super::*;

    fn repo() -> PathBuf {
        PathBuf::from("/w/proj")
    }

    fn ok(_: &Request) -> Result<(), String> {
        Ok(())
    }

    const NOW: u64 = 1_800_000_000;

    struct World {
        _home: tempfile::TempDir,
        store: Store,
    }

    fn world() -> World {
        let home = tempfile::tempdir().unwrap();
        World {
            store: Store::at(home.path().join("uc")),
            _home: home,
        }
    }

    fn tag() -> String {
        crate::cli::monitor::repo_tag(&repo())
    }

    fn filed(store: &Store) -> usize {
        store.list(&tag()).len()
    }

    fn text(v: &serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn a_request_is_filed_as_admitted() {
        let w = world();
        let status = admit(&w.store, &text(&request(&repo())), NOW, ok).unwrap();
        assert_eq!(status.state, State::Admitted);
        assert_eq!(status.id, "r1");
        assert_eq!(filed(&w.store), 1);
        assert!(!serde_json::to_string(&status).unwrap().contains("git push"));
    }

    /// A malformed request and a target that is not this machine's each
    /// file nothing.
    #[test]
    fn a_request_that_fails_files_nothing() {
        let w = world();
        assert!(admit(&w.store, "", NOW, ok).is_err());
        assert!(admit(&w.store, "{\"command\":\"ls\"}", NOW, ok).is_err());
        let elsewhere = admit(&w.store, &text(&request(&repo())), NOW, |_| {
            Err("the session it names does not hold the label".into())
        });
        assert!(elsewhere.is_err());
        assert_eq!(filed(&w.store), 0);
    }

    /// The same id asking for the same session and command answers with the
    /// record as it stands; with another command or session it refuses and
    /// the record is untouched.
    #[test]
    fn an_id_is_bound_to_its_request() {
        let w = world();
        let first = text(&request(&repo()));
        admit(&w.store, &first, NOW, ok).unwrap();
        let (label, id) = w.store.list(&tag()).remove(0);
        let mut record = w.store.read(&tag(), &label, &id).unwrap().unwrap();
        record.moved(State::Delivered, None, NOW + 1);
        w.store
            .lock(&tag(), &label, &id)
            .unwrap()
            .save(&record)
            .unwrap();
        let again = admit(&w.store, &first, NOW + 2, ok).unwrap();
        assert_eq!(again.state, State::Delivered);
        for (field, value) in [("command", "git push --force"), ("session", "session-two")] {
            let mut other = request(&repo());
            other[field] = value.into();
            assert!(
                admit(&w.store, &text(&other), NOW, ok)
                    .unwrap_err()
                    .contains("already names a different request"),
                "{field}"
            );
        }
        assert_eq!(w.store.read(&tag(), &label, &id).unwrap().unwrap(), record);
    }

    /// A repeat submit of a waiting request rewrites it, so the monitor's
    /// watch sees it and looks again.
    #[test]
    fn a_repeat_submit_pokes_a_waiting_request() {
        let w = world();
        let first = text(&request(&repo()));
        admit(&w.store, &first, NOW, ok).unwrap();
        let (label, id) = w.store.list(&tag()).remove(0);
        let before = w.store.stamp(&tag(), &label, &id);
        admit(&w.store, &first, NOW, ok).unwrap();
        assert_ne!(w.store.stamp(&tag(), &label, &id), before);
    }

    fn place(store: &Store, id: &str, bytes: &[u8]) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
        drop(store.lock(&tag(), &label, &Id::parse(id).unwrap()).unwrap());
        let path = store
            .repo_dir(&tag())
            .join("claude")
            .join(format!("{id}.json"));
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        path
    }

    /// An id held by a record an older clank wrote, or by one that cannot
    /// be read, is never filed afresh: the submit refuses and the bytes on
    /// disk stay as they were.
    #[test]
    fn an_older_or_unreadable_record_keeps_its_id() {
        let older: &[u8] =
            include_bytes!("../../../tests/fixtures/formats/user-command-keyed-waiting.json");
        for bytes in [older, b"{\"state\":".as_slice()] {
            let w = world();
            let path = place(&w.store, "r1", bytes);
            assert!(admit(&w.store, &text(&request(&repo())), NOW, ok).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn only_the_named_claude_session_is_a_target() {
        let s = |s: &str| clank_core::ids::SessionId::parse(s).unwrap();
        let one = s("session-one");
        assert_eq!(bound_to(Some((Tool::Claude, one.clone())), &one), Ok(()));
        assert_eq!(
            bound_to(Some((Tool::Codex, one.clone())), &one),
            Err(NOT_CLAUDE.into())
        );
        assert!(bound_to(Some((Tool::Claude, s("session-two"))), &one).is_err());
        assert!(bound_to(None, &one).is_err());
    }
}
