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

use crate::cli::open_zellij::{self, CallerIdentity, Lookup, SessionPane, SessionScan};
use std::path::Path;

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
    /// A live pane in the repo's workspace may run this agent: its name
    /// reads as this label and another, so neither "nobody" nor a holder
    /// is known.
    Uncertain {
        session: String,
        why: String,
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
/// `session` is the agent's bound session, `None` for a fresh agent:
/// then only a live pane can hold it. `claude_program` is what the
/// launch would exec, so a wrapper sees the listing request the same way
/// it would see the launch.
pub(crate) fn find(
    session: Option<&Session>,
    label: &AgentLabel,
    repo: &Path,
    claude_program: String,
) -> Holder {
    let caller = open_zellij::caller_identity();
    let scan = (caller != CallerIdentity::Unidentified)
        .then(|| open_zellij::panes_in_all_sessions(SCAN_DEADLINE));
    let claude = match session {
        Some(s) if s.tool == Tool::Claude => claude_agents_listing(claude_program),
        _ => Vec::new(),
    };
    let candidates = crate::cli::pane_owners::candidates(repo, label.as_str());
    decide(
        session.map(|s| s.id.as_str()),
        label.as_str(),
        repo,
        candidates,
        &caller,
        scan.as_ref(),
        &claude,
    )
}

/// `claude agents --json` needs no TTY. Any failure is an empty listing.
pub(crate) fn claude_agents_listing(program: String) -> Vec<ClaudeSession> {
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

/// A pane holds `label` when it is a live pane named `label` in the
/// repo's workspace, its panes named against `candidates`. A scan that
/// resolves no workspace names no pane holder: that fails open, the
/// launch of today.
///
/// The caller's own pane is in the scan too, so it is excluded by
/// (session, pane id). An unidentified caller inside zellij cannot be
/// excluded, so `scan` is `None` and panes are not consulted at all — a
/// match might be the caller, and refusing every launch is the worse
/// error.
pub(crate) fn decide(
    session_id: Option<&str>,
    label: &str,
    repo: &Path,
    candidates: std::collections::BTreeSet<String>,
    caller: &CallerIdentity,
    scan: Option<&SessionScan>,
    claude: &[ClaudeSession],
) -> Holder {
    if let Some(scan) = scan
        && let Ok((session, ws)) = open_zellij::scan_workspace(scan, repo, candidates)
    {
        let is_caller = |sp: &SessionPane| match caller {
            CallerIdentity::Pane { session, pane_id } => {
                sp.session == *session && sp.pane.pane_id() == *pane_id
            }
            _ => false,
        };
        let others = scan
            .panes
            .iter()
            .filter(|sp| sp.session == session && !sp.pane.has_exited() && !is_caller(sp));
        match ws.lookup(others, label) {
            Lookup::Known(found) => {
                if let Some(other) = found.first() {
                    return Holder::Pane {
                        session: other.session.clone(),
                        tab: other.pane.tab_name().to_string(),
                    };
                }
            }
            Lookup::Uncertain(doubts) => {
                return Holder::Uncertain {
                    session,
                    why: open_zellij::describe_ambiguities(&doubts),
                };
            }
        }
    }
    if let Some(short_id) = claude
        .iter()
        .find(|c| c.kind == "background" && Some(c.session_id.as_str()) == session_id)
        .and_then(|c| c.id.clone())
    {
        return Holder::ClaudeBackground { short_id };
    }
    Holder::Nobody
}

/// [`decide`] over one session's `list-panes --json` output, from its
/// pane `caller`.
#[cfg(test)]
pub(crate) fn decide_over(
    listing: &str,
    caller: &str,
    session_id: Option<&str>,
    label: &str,
    repo: &Path,
    candidates: std::collections::BTreeSet<String>,
) -> Holder {
    let panes: Vec<open_zellij::ZellijPane> = serde_json::from_str(listing).unwrap();
    let scan = SessionScan {
        panes: panes
            .into_iter()
            .map(|pane| SessionPane {
                session: "work".into(),
                pane,
            })
            .collect(),
        unanswered: Vec::new(),
        listing_failed: false,
    };
    let caller = CallerIdentity::Pane {
        session: "work".into(),
        pane_id: caller.into(),
    };
    decide(
        session_id,
        label,
        repo,
        candidates,
        &caller,
        Some(&scan),
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::open_zellij::ZellijPane;

    const SESSION: &str = "01a04034-cebe-7c12-8595-99c9b9b9de9c";

    fn repo() -> &'static Path {
        Path::new("/r")
    }

    /// A `list-panes --json --command --tab` element as zellij 0.45.0
    /// emits it, trimmed to the fields that vary here.
    fn pane_json(id: u32, title: &str, exited: bool, tab: u32) -> String {
        format!(
            r#"{{"id":{id},"is_plugin":false,"is_focused":false,"exited":{exited},
                "title":"{title}","terminal_command":"clank agent start codex --repo /r",
                "tab_id":{tab},"tab_name":"t{tab}","pane_x":0,"pane_y":0,"pane_columns":80,"pane_rows":24}}"#
        )
    }

    /// `repo`'s TUI pane in `tab`.
    fn tui_json(id: u32, tab: u32, repo: &str) -> String {
        format!(
            r#"{{"id":{id},"title":"","terminal_command":"clank tui --repo {repo}","tab_id":{tab},"tab_name":"t{tab}"}}"#
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

    fn scan_of(panes: Vec<SessionPane>, unanswered: &[&str]) -> SessionScan {
        SessionScan {
            panes,
            unanswered: unanswered.iter().map(|s| s.to_string()).collect(),
            listing_failed: false,
        }
    }

    fn caller(session: &str, id: u32) -> CallerIdentity {
        CallerIdentity::Pane {
            session: session.into(),
            pane_id: format!("terminal_{id}"),
        }
    }

    fn holds(session: Option<&str>, caller: &CallerIdentity, scan: &SessionScan) -> Holder {
        decide(
            session,
            "codex",
            repo(),
            ["claude", "codex"].map(String::from).into(),
            caller,
            Some(scan),
            &[],
        )
    }

    #[test]
    fn a_lone_self_match_is_nobody() {
        // The launching pane carries the agent's own name; a scan that
        // does not know who is asking finds itself and refuses every
        // fresh launch (codex on f125770).
        let scan = scan_of(
            listing(
                "clank-r",
                &[tui_json(1, 0, "/r"), pane_json(3, "codex", false, 0)],
            ),
            &[],
        );
        assert_eq!(
            holds(Some(SESSION), &caller("clank-r", 3), &scan),
            Holder::Nobody
        );
    }

    #[test]
    fn self_plus_another_in_the_workspace_names_the_other() {
        let scan = scan_of(
            listing(
                "big",
                &[
                    tui_json(1, 4, "/r"),
                    pane_json(3, "codex", false, 4),
                    pane_json(9, "👀 codex", false, 4),
                ],
            ),
            &[],
        );
        let pane = Holder::Pane {
            session: "big".into(),
            tab: "t4".into(),
        };
        assert_eq!(holds(Some(SESSION), &caller("big", 3), &scan), pane);
        assert_eq!(
            holds(None, &caller("big", 3), &scan),
            pane,
            "a fresh agent too"
        );
    }

    #[test]
    fn the_same_pane_id_in_another_session_is_not_the_caller() {
        let scan = scan_of(
            listing(
                "big",
                &[tui_json(1, 0, "/r"), pane_json(3, "codex", false, 0)],
            ),
            &[],
        );
        assert!(matches!(
            holds(Some(SESSION), &caller("clank-r", 3), &scan),
            Holder::Pane { .. }
        ));
    }

    #[test]
    fn an_unidentified_caller_does_not_consult_panes() {
        assert_eq!(
            decide(
                Some(SESSION),
                "codex",
                repo(),
                ["codex".to_string()].into(),
                &CallerIdentity::Unidentified,
                None,
                &[],
            ),
            Holder::Nobody
        );
    }

    #[test]
    fn an_exited_pane_holds_nothing() {
        // zellij keeps a pane open after its process ends; refusing over
        // that corpse is the failure this exists to remove (codex on
        // d38a63d).
        let dead = scan_of(
            listing(
                "big",
                &[tui_json(1, 0, "/r"), pane_json(9, "codex", true, 0)],
            ),
            &[],
        );
        assert_eq!(
            holds(Some(SESSION), &CallerIdentity::NotInZellij, &dead),
            Holder::Nobody
        );
    }

    /// The same session holds two repos' tabs, each with a `codex`: the
    /// other repo's is no holder of this one's.
    #[test]
    fn another_repos_tab_with_the_same_label_does_not_count() {
        let scan = scan_of(
            listing(
                "shared",
                &[
                    tui_json(1, 0, "/r"),
                    tui_json(5, 1, "/r/.clank/worktrees/x"),
                    pane_json(9, "codex", false, 1),
                ],
            ),
            &[],
        );
        assert_eq!(
            holds(None, &CallerIdentity::NotInZellij, &scan),
            Holder::Nobody
        );
    }

    /// No workspace is resolved, so no pane refuses the start: no TUI
    /// pane, two of them, or one beside a session that did not answer.
    #[test]
    fn an_unresolved_workspace_refuses_nothing() {
        let live = pane_json(9, "codex", false, 0);
        for scan in [
            scan_of(listing("s", std::slice::from_ref(&live)), &[]),
            scan_of(
                listing(
                    "s",
                    &[tui_json(1, 0, "/r"), tui_json(2, 3, "/r"), live.clone()],
                ),
                &[],
            ),
            scan_of(
                listing("s", &[tui_json(1, 0, "/r"), live.clone()]),
                &["stale"],
            ),
        ] {
            assert_eq!(
                holds(None, &CallerIdentity::NotInZellij, &scan),
                Holder::Nobody
            );
        }
    }

    /// Two entries of real `claude agents --json` output (2026-09-04),
    /// one of each kind, ids shortened.
    const CLAUDE_AGENTS: &str = r#"[
        {"id":"1f47fd71","cwd":"/r","kind":"background","startedAt":"1783234258327",
         "sessionId":"1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69","name":"Bind it yes","state":"blocked"},
        {"pid":"57431","cwd":"/r","kind":"interactive","startedAt":"1783641752694",
         "sessionId":"7f4ce6a6-93b6-4a9b-8b8b-73dbd6e36bd4","name":"driver","status":"busy"}
    ]"#;

    fn background(session: &str, scan: Option<&SessionScan>) -> Holder {
        let claude = parse_claude_agents(CLAUDE_AGENTS.as_bytes());
        decide(
            Some(session),
            "codex",
            repo(),
            ["codex".to_string()].into(),
            &CallerIdentity::NotInZellij,
            scan,
            &claude,
        )
    }

    #[test]
    fn a_background_claude_session_is_attached_to() {
        assert_eq!(
            parse_claude_agents(CLAUDE_AGENTS.as_bytes()).len(),
            2,
            "both kinds parse"
        );
        assert_eq!(
            background("1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69", None),
            Holder::ClaudeBackground {
                short_id: "1f47fd71".into()
            }
        );
    }

    #[test]
    fn an_interactive_claude_entry_is_not_background() {
        assert_eq!(
            background("7f4ce6a6-93b6-4a9b-8b8b-73dbd6e36bd4", None),
            Holder::Nobody
        );
    }

    #[test]
    fn a_session_absent_from_the_listing_or_an_unreadable_listing_is_nobody() {
        assert_eq!(background(SESSION, None), Holder::Nobody);
        assert!(
            parse_claude_agents(b"'claude agents' requires an interactive terminal").is_empty()
        );
        assert!(parse_claude_agents(b"").is_empty());
    }

    #[test]
    fn a_live_pane_wins_over_a_background_entry() {
        // A pane still open elsewhere is the holder even if claude
        // also lists the session; attaching would not free the pane.
        let scan = scan_of(
            listing(
                "big",
                &[tui_json(1, 0, "/r"), pane_json(9, "codex", false, 0)],
            ),
            &[],
        );
        assert!(matches!(
            background("1f47fd71-2eb5-4f8c-9244-f74e5e4c7c69", Some(&scan)),
            Holder::Pane { .. }
        ));
    }
}
