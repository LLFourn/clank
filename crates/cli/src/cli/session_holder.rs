//! Who holds an agent's session, decided before the launch is composed.
//!
//! A session has one holder. `clank agent start` used to resume blind,
//! and two things can already hold the session by then: a claude
//! process that outlived its pane (Claude Code keeps a session running
//! in the background when its terminal goes away — closing a zellij
//! tab is exactly that), and a pane still open in another zellij
//! session. Both refuse a second resume, in the pane clank was meant
//! to fill, with a message the user then has to act on by hand
//! (a-session-has-one-holder).
//!
//! The decision is pure over listings the caller gathers; every probe
//! that fails leaves its listing empty, which decides "nobody" — the
//! launch of today, never a new failure.

use clank_core::agent_config::Session;
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;

use crate::cli::open_zellij::{self, CallerIdentity, SessionPane};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Holder {
    Nobody,
    /// A process that outlived its pane. Reconnect it rather than
    /// restart it: `claude attach <short id>`.
    ClaudeBackground {
        short_id: String,
    },
    /// A live pane elsewhere already runs this agent.
    Pane {
        session: String,
        tab: String,
    },
}

/// One entry of `claude agents --json`. Unknown fields are ignored so a
/// listing that grows new ones still parses.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct ClaudeSession {
    pub(crate) kind: String,
    #[serde(rename = "sessionId")]
    pub(crate) session_id: String,
    /// The short id `attach` takes. Interactive entries carry none.
    #[serde(default)]
    pub(crate) id: Option<String>,
}

/// Lenient: anything that is not a JSON array of entries is an empty
/// listing, and an empty listing names no holder.
pub(crate) fn parse_claude_agents(json: &[u8]) -> Vec<ClaudeSession> {
    serde_json::from_slice(json).unwrap_or_default()
}

/// How long the all-sessions pane scan may take before unanswered
/// sessions are dropped. Healthy servers answer in well under 0.2 s;
/// the stale ones that take seconds are the ones a launch must not
/// wait on.
const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// Gather the listings and decide. Every probe is a read.
///
/// `repo_arg` is the `--repo` the launching process was given, so the
/// command searched for is byte-identical to the one in its own pane.
/// `claude_program` is what the launch would exec, so a wrapper sees
/// the listing request the same way it would see the launch.
pub(crate) fn find(
    session: &Session,
    label: &AgentLabel,
    repo_arg: &str,
    claude_program: String,
) -> Holder {
    let launch_command = open_zellij::agent_start_command(label.as_str(), repo_arg);
    let caller = open_zellij::caller_identity();
    let panes = if caller == CallerIdentity::Unidentified {
        Vec::new()
    } else {
        open_zellij::panes_in_all_sessions(SCAN_DEADLINE)
    };
    let claude = if session.tool == Tool::Claude {
        claude_agents_listing(claude_program)
    } else {
        Vec::new()
    };
    decide(
        session.id.as_str(),
        &launch_command,
        &caller,
        &panes,
        &claude,
    )
}

/// `claude agents --json` needs no TTY. Any failure is an empty listing.
fn claude_agents_listing(program: String) -> Vec<ClaudeSession> {
    std::process::Command::new(program)
        .args(["agents", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_claude_agents(&o.stdout))
        .unwrap_or_default()
}

/// `launch_command` is the byte-exact `clank agent start …` string
/// the agent's pane runs; a sibling worktree's pane differs in its
/// `--repo` and so never matches.
///
/// The caller's own pane is in `panes` too, so it is excluded by
/// (session, pane id). An unidentified caller inside zellij cannot be
/// excluded, so the panes are not consulted at all — a match might be
/// the caller, and refusing every launch is the worse error.
pub(crate) fn decide(
    session_id: &str,
    launch_command: &str,
    caller: &CallerIdentity,
    panes: &[SessionPane],
    claude: &[ClaudeSession],
) -> Holder {
    if *caller != CallerIdentity::Unidentified {
        let is_caller = |sp: &SessionPane| match caller {
            CallerIdentity::Pane { session, pane_id } => {
                sp.session == *session && sp.pane.pane_id() == *pane_id
            }
            _ => false,
        };
        if let Some(other) = panes
            .iter()
            .find(|sp| sp.pane.runs(launch_command) && !is_caller(sp))
        {
            return Holder::Pane {
                session: other.session.clone(),
                tab: other.pane.tab_name().to_string(),
            };
        }
    }
    if let Some(short_id) = claude
        .iter()
        .find(|c| c.kind == "background" && c.session_id == session_id)
        .and_then(|c| c.id.clone())
    {
        return Holder::ClaudeBackground { short_id };
    }
    Holder::Nobody
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::open_zellij::ZellijPane;

    const CMD: &str = "clank agent start codex --repo /r";
    const SESSION: &str = "01a04034-cebe-7c12-8595-99c9b9b9de9c";

    /// A `list-panes --json --command --tab` element as zellij 0.45.0
    /// emits it, trimmed to the fields that vary here.
    fn pane_json(id: u32, command: &str, exited: bool, tab: &str) -> String {
        format!(
            r#"{{"id":{id},"is_plugin":false,"is_focused":false,"exited":{exited},
                "title":"codex (reviewer)","terminal_command":"{command}",
                "tab_id":0,"tab_name":"{tab}","pane_x":0,"pane_y":0,"pane_columns":80,"pane_rows":24}}"#
        )
    }

    fn listing(session: &str, elements: &[String]) -> Vec<SessionPane> {
        let json = format!("[{}]", elements.join(","));
        serde_json::from_str::<Vec<ZellijPane>>(&json)
            .expect("fixture is a list-panes element")
            .into_iter()
            .map(|pane| SessionPane {
                session: session.to_string(),
                pane,
            })
            .collect()
    }

    fn caller(session: &str, id: u32) -> CallerIdentity {
        CallerIdentity::Pane {
            session: session.into(),
            pane_id: format!("terminal_{id}"),
        }
    }

    #[test]
    fn a_lone_self_match_is_nobody() {
        // The launching pane's own command is the one being searched
        // for; a scan that does not know who is asking finds itself
        // and refuses every fresh launch (codex on f125770).
        let panes = listing("clank-r", &[pane_json(3, CMD, false, "r")]);
        assert_eq!(
            decide(SESSION, CMD, &caller("clank-r", 3), &panes, &[]),
            Holder::Nobody
        );
    }

    #[test]
    fn self_plus_another_names_the_other() {
        let mut panes = listing("clank-r", &[pane_json(3, CMD, false, "r")]);
        panes.extend(listing("clank-big", &[pane_json(9, CMD, false, "r-tab")]));
        assert_eq!(
            decide(SESSION, CMD, &caller("clank-r", 3), &panes, &[]),
            Holder::Pane {
                session: "clank-big".into(),
                tab: "r-tab".into()
            }
        );
    }

    #[test]
    fn the_same_pane_id_in_another_session_is_not_the_caller() {
        let panes = listing("clank-big", &[pane_json(3, CMD, false, "r-tab")]);
        assert_eq!(
            decide(SESSION, CMD, &caller("clank-r", 3), &panes, &[]),
            Holder::Pane {
                session: "clank-big".into(),
                tab: "r-tab".into()
            }
        );
    }

    #[test]
    fn an_unidentified_caller_does_not_consult_panes() {
        let panes = listing("clank-big", &[pane_json(9, CMD, false, "r-tab")]);
        assert_eq!(
            decide(SESSION, CMD, &CallerIdentity::Unidentified, &panes, &[]),
            Holder::Nobody
        );
    }

    #[test]
    fn outside_zellij_a_lone_match_anywhere_is_a_holder() {
        let panes = listing("clank-big", &[pane_json(9, CMD, false, "r-tab")]);
        assert_eq!(
            decide(SESSION, CMD, &CallerIdentity::NotInZellij, &panes, &[]),
            Holder::Pane {
                session: "clank-big".into(),
                tab: "r-tab".into()
            }
        );
    }

    #[test]
    fn an_exited_pane_holds_nothing() {
        // zellij keeps a pane open after its process ends and still
        // reports its command; refusing over that corpse is the
        // failure this exists to remove (codex on d38a63d).
        let mut panes = listing("clank-r", &[pane_json(3, CMD, false, "r")]);
        panes.extend(listing("clank-big", &[pane_json(9, CMD, true, "r-tab")]));
        assert_eq!(
            decide(SESSION, CMD, &caller("clank-r", 3), &panes, &[]),
            Holder::Nobody
        );
        let alone = listing("clank-big", &[pane_json(9, CMD, true, "r-tab")]);
        assert_eq!(
            decide(SESSION, CMD, &CallerIdentity::NotInZellij, &alone, &[]),
            Holder::Nobody
        );
        let live = listing("clank-big", &[pane_json(9, CMD, false, "r-tab")]);
        assert!(matches!(
            decide(SESSION, CMD, &CallerIdentity::NotInZellij, &live, &[]),
            Holder::Pane { .. }
        ));
    }

    #[test]
    fn another_repos_pane_with_the_same_label_does_not_count() {
        let panes = listing(
            "clank-big",
            &[pane_json(
                9,
                "clank agent start codex --repo /r/.clank/worktrees/x",
                false,
                "x",
            )],
        );
        assert_eq!(
            decide(SESSION, CMD, &CallerIdentity::NotInZellij, &panes, &[]),
            Holder::Nobody
        );
    }

    /// Two entries of real `claude agents --json` output (2026-09-04),
    /// one of each kind, ids shortened.
    const CLAUDE_AGENTS: &str = r#"[
        {"id":"1f47fd71","cwd":"/r","kind":"background","startedAt":"1783234258327",
         "sessionId":"1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69","name":"Bind it yes","state":"blocked"},
        {"pid":"57431","cwd":"/r","kind":"interactive","startedAt":"1783641752694",
         "sessionId":"7f4ce6a6-93b6-4a9b-8b8b-73dbd6e36bd4","name":"driver","status":"busy"}
    ]"#;

    #[test]
    fn a_background_claude_session_is_attached_to() {
        let claude = parse_claude_agents(CLAUDE_AGENTS.as_bytes());
        assert_eq!(claude.len(), 2, "both kinds parse");
        assert_eq!(
            decide(
                "1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69",
                CMD,
                &CallerIdentity::NotInZellij,
                &[],
                &claude
            ),
            Holder::ClaudeBackground {
                short_id: "1f47fd71".into()
            }
        );
    }

    #[test]
    fn an_interactive_claude_entry_is_not_background() {
        let claude = parse_claude_agents(CLAUDE_AGENTS.as_bytes());
        assert_eq!(
            decide(
                "7f4ce6a6-93b6-4a9b-8b8b-73dbd6e36bd4",
                CMD,
                &CallerIdentity::NotInZellij,
                &[],
                &claude
            ),
            Holder::Nobody
        );
    }

    #[test]
    fn a_session_absent_from_the_listing_or_an_unreadable_listing_is_nobody() {
        let claude = parse_claude_agents(CLAUDE_AGENTS.as_bytes());
        assert_eq!(
            decide(SESSION, CMD, &CallerIdentity::NotInZellij, &[], &claude),
            Holder::Nobody
        );
        assert!(
            parse_claude_agents(b"'claude agents' requires an interactive terminal").is_empty()
        );
        assert!(parse_claude_agents(b"").is_empty());
    }

    #[test]
    fn a_live_pane_wins_over_a_background_entry() {
        // A pane still open elsewhere is the holder even if claude
        // also lists the session; attaching would not free the pane.
        let claude = parse_claude_agents(CLAUDE_AGENTS.as_bytes());
        let panes = listing("clank-big", &[pane_json(9, CMD, false, "r-tab")]);
        assert!(matches!(
            decide(
                "1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69",
                CMD,
                &CallerIdentity::NotInZellij,
                &panes,
                &claude
            ),
            Holder::Pane { .. }
        ));
    }
}
