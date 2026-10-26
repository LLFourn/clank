//! `clank as <label>` — bind the calling agent's session to a
//! clank label, and name its pane for it.
//!
//! Reads `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` /
//! `OPENCODE_SESSION_ID` from env,
//! writes `.clank/agents/<label>/config.json` with the session
//! field set, and clears the same session id from any OTHER
//! agent's config (one session can't be bound to two labels at
//! once).
//!
//! The bind operation itself lives in
//! [`crate::agent_store::bind_session_to_agent`] — shared with
//! `clank init` phase 2 so both callers preserve the same
//! uniqueness invariant without duplicating the stale-clear
//! logic.
//!
//! Inside zellij the pane is then renamed to the label, which is its
//! identity to clank. Whether that may happen is decided BEFORE the
//! bind, so a refusal changes nothing.

use std::collections::BTreeSet;
use std::path::Path;

use super::open_zellij::{self, CallerIdentity, Lookup, Workspace, ZellijPane};
use super::{AsArgs, resolve_repo};
use crate::agent_env::detect_session_from_env;
use crate::agent_store::bind_session_to_agent;
use anyhow::Context;
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::Tool;

pub async fn run(args: AsArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = AgentLabel::parse(&args.label)
        .map_err(|e| anyhow::anyhow!("invalid label `{}`: {e}", args.label))?;

    let (tool, session_id) = detect_session_from_env()?.ok_or_else(|| {
        anyhow::anyhow!(
            "no session detected in env (CLAUDE_CODE_SESSION_ID / \
             CODEX_THREAD_ID / OPENCODE_SESSION_ID not set). `clank as` \
             must be run from inside a claude, codex, grok, or opencode \
             session."
        )
    })?;
    run_with(&repo, &label, tool, &session_id, &mut Live)
}

/// What `clank as` reads and does outside its binding, injected so each
/// outcome is testable without zellij.
trait AsIo {
    fn caller(&mut self) -> CallerIdentity;
    /// This session's panes; `None` when zellij did not answer.
    fn listing(&mut self) -> Option<Vec<ZellijPane>>;
    fn owned(&mut self, repo: &Path) -> anyhow::Result<Option<BTreeSet<String>>>;
    fn rename(&mut self, pane: &str, name: &str) -> Result<(), String>;
    fn own(&mut self, repo: &Path, label: &str) -> anyhow::Result<()>;
    fn say(&mut self, line: &str);
}

struct Live;

impl AsIo for Live {
    fn caller(&mut self) -> CallerIdentity {
        open_zellij::caller_identity()
    }
    fn listing(&mut self) -> Option<Vec<ZellijPane>> {
        open_zellij::snapshot_panes()
    }
    fn owned(&mut self, repo: &Path) -> anyhow::Result<Option<BTreeSet<String>>> {
        super::pane_owners::load(repo)
    }
    fn rename(&mut self, pane: &str, name: &str) -> Result<(), String> {
        open_zellij::rename_pane_checked(pane, name)
    }
    fn own(&mut self, repo: &Path, label: &str) -> anyhow::Result<()> {
        super::pane_owners::update(repo, &BTreeSet::from([label.to_string()]), &BTreeSet::new())
            .map(drop)
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

/// What may happen to the calling pane once the session is bound.
enum PaneStep {
    /// Not inside zellij: there is no pane to touch.
    Untouched,
    /// Inside zellij, but the pane is not renamed, for this reason.
    NotRenamed(String),
    Rename(String),
}

/// Decide what may be done to the calling pane, refusing when another
/// live pane in the repo's workspace is already `label`.
fn pane_step(
    repo: &Path,
    label: &AgentLabel,
    roster: &BTreeSet<String>,
    io: &mut impl AsIo,
) -> anyhow::Result<PaneStep> {
    let pane_id = match io.caller() {
        CallerIdentity::NotInZellij => return Ok(PaneStep::Untouched),
        CallerIdentity::Unidentified => {
            return Ok(PaneStep::NotRenamed(
                "zellij did not say which pane this is (ZELLIJ_PANE_ID is unset)".into(),
            ));
        }
        CallerIdentity::Pane { pane_id, .. } => pane_id,
    };
    let Some(panes) = io.listing() else {
        return Ok(PaneStep::NotRenamed(
            "zellij did not answer the pane listing".into(),
        ));
    };
    let mut candidates = roster.clone();
    candidates.extend(io.owned(repo).ok().flatten().unwrap_or_default());
    candidates.insert(label.as_str().to_string());
    let Some(ws) = Workspace::anchored(&panes, repo, candidates) else {
        return Ok(PaneStep::NotRenamed(format!(
            "this session does not hold exactly one TUI pane of {}, so its workspace is unknown",
            repo.display()
        )));
    };
    if open_zellij::owned_tab(&panes, &pane_id) != Some(ws.tab()) {
        return Ok(PaneStep::NotRenamed(format!(
            "this pane is not in {}'s tab, the one holding its TUI pane",
            repo.display()
        )));
    }
    let others = panes
        .iter()
        .filter(|p| p.pane_id() != pane_id && !p.has_exited());
    match ws.lookup(others, label.as_str()) {
        Lookup::Known(found) => {
            if let Some(other) = found.first() {
                anyhow::bail!(
                    "`{label}` already has a live pane in this tab ({}); nothing was bound. \
                     Use that pane, or close it and run `clank as {label}` again.",
                    other.pane_id()
                );
            }
        }
        Lookup::Uncertain(doubts) => anyhow::bail!(
            "whether `{label}` already has a live pane in this tab cannot be known: {}; \
             nothing was bound. Rename that pane to one agent's label, then run \
             `clank as {label}` again.",
            open_zellij::describe_ambiguities(&doubts)
        ),
    }
    Ok(PaneStep::Rename(pane_id))
}

fn run_with(
    repo: &Path,
    label: &AgentLabel,
    tool: Tool,
    session_id: &SessionId,
    io: &mut impl AsIo,
) -> anyhow::Result<()> {
    let roster = super::pane_owners::roster_labels(repo);
    let step = pane_step(repo, label, &roster, io)?;

    // `clank as` binds a session — pure per-agent STATE. Under
    // the team model, binding a session for a label that isn't in
    // the repo's team just produces orphan state (ignored by
    // gates / lists). No sentinel gate anymore.
    let outcome = bind_session_to_agent(repo, label, tool, session_id)?;

    io.say(&format!(
        "bound {tool} session {sid} to agent `{label}`",
        tool = tool.as_str(),
        sid = session_id.as_str(),
        label = outcome.label.as_str(),
    ));
    for other in outcome.cleared_from {
        io.say(&format!(
            "  (cleared stale binding on `{}`)",
            other.as_str()
        ));
    }
    let pane = match step {
        PaneStep::Untouched => return Ok(()),
        PaneStep::NotRenamed(why) => {
            io.say(&format!("this pane was not renamed: {why}"));
            return Ok(());
        }
        PaneStep::Rename(pane) => pane,
    };
    if let Err(e) = io.rename(&pane, &open_zellij::agent_pane_title(label.as_str())) {
        anyhow::bail!("the session is bound, but this pane was not renamed to `{label}`: {e}");
    }
    io.say(&format!("renamed this pane to `{label}`"));
    // Owning a label off the roster would have the TUI close this pane
    // as a departed agent's on its next pass.
    if roster.contains(label.as_str()) {
        io.own(repo, label.as_str()).with_context(|| {
            format!(
                "the session is bound and the pane renamed, but clank could not record that \
                 it owns `{label}`'s pane, so the TUI will not close it if `{label}` leaves \
                 the roster"
            )
        })?;
    } else {
        io.say(&format!(
            "`{label}` is not on the roster, so the TUI leaves this pane alone"
        ));
    }
    Ok(())
}

// Concurrency note: two `clank as` runs in two shells using the
// SAME session id could race past each other's stale-scan and
// both succeed. Result: two labels hold the same session id; the
// resolver picks whichever it scans first. Acceptable for v1 —
// the race window is tiny and the recovery is `clank as <real>`
// once you notice. Revisit (with a `.clank/.lock` advisory file)
// if it becomes a real problem.

#[cfg(test)]
mod tests {
    use super::*;

    const BOUND: &str = "01a16cdd-5f28-7bd2-b895-7122737638ce";
    const MINE: &str = "01a16cfb-9a15-7801-a3de-671cc0308ebb";

    /// A scripted zellij: who the caller is, what the session lists, and
    /// whether a rename is accepted. Every rename and ownership write is
    /// recorded.
    struct Fake {
        caller: CallerIdentity,
        listing: Option<String>,
        rename: Result<(), String>,
        renamed: Vec<(String, String)>,
        /// The ownership record as `owned` reads it.
        record: Option<BTreeSet<String>>,
        owned: Vec<String>,
        said: Vec<String>,
    }

    impl Fake {
        fn in_pane(listing: Option<String>) -> Self {
            Self {
                caller: CallerIdentity::Pane {
                    session: "s".into(),
                    pane_id: "terminal_5".into(),
                },
                listing,
                rename: Ok(()),
                renamed: Vec::new(),
                record: None,
                owned: Vec::new(),
                said: Vec::new(),
            }
        }
    }

    impl AsIo for Fake {
        fn caller(&mut self) -> CallerIdentity {
            self.caller.clone()
        }
        fn listing(&mut self) -> Option<Vec<ZellijPane>> {
            self.listing
                .as_deref()
                .map(|json| serde_json::from_str(json).unwrap())
        }
        fn owned(&mut self, _: &Path) -> anyhow::Result<Option<BTreeSet<String>>> {
            Ok(self.record.clone())
        }
        fn rename(&mut self, pane: &str, name: &str) -> Result<(), String> {
            self.renamed.push((pane.into(), name.into()));
            self.rename.clone()
        }
        fn own(&mut self, _: &Path, label: &str) -> anyhow::Result<()> {
            self.owned.push(label.into());
            Ok(())
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.into());
        }
    }

    /// A git repo whose roster is `claude` (master) and `painter`, with
    /// `painter` bound to the session `BOUND`.
    fn repo() -> tempfile::TempDir {
        use crate::cli::teams_config::{AgentDescription, RosterRole};
        let dir = tempfile::tempdir().unwrap();
        crate::git_plumbing::init_repo(dir.path()).unwrap();
        for (label, role) in [
            ("claude", RosterRole::Master),
            ("painter", RosterRole::Commit),
        ] {
            crate::cli::agent::add_repo_roster_agent(
                dir.path(),
                &AgentLabel::parse(label).unwrap(),
                AgentDescription {
                    tool: Tool::Codex,
                    launch: None,
                    initial_prompt: None,
                },
                role,
            )
            .unwrap();
        }
        bind_session_to_agent(
            dir.path(),
            &AgentLabel::parse("painter").unwrap(),
            Tool::Codex,
            &SessionId::parse(BOUND).unwrap(),
        )
        .unwrap();
        dir
    }

    /// One tab holding `repo`'s TUI pane, the caller's pane `terminal_5`
    /// and `others`.
    fn tab(repo: &Path, others: &[&str]) -> String {
        let mut panes = vec![
            format!(
                r#"{{"id":1,"title":"","terminal_command":"clank tui --repo {}","tab_id":0}}"#,
                repo.display()
            ),
            r#"{"id":5,"title":"bash","tab_id":0}"#.to_string(),
        ];
        panes.extend(others.iter().map(|s| s.to_string()));
        format!("[{}]", panes.join(","))
    }

    fn painter() -> AgentLabel {
        AgentLabel::parse("painter").unwrap()
    }

    fn bound_to(repo: &Path, label: &str) -> Option<String> {
        crate::agent_store::load_agent_config(repo, &AgentLabel::parse(label).unwrap())
            .unwrap()
            .and_then(|c| c.session)
            .map(|s| s.id.as_str().to_string())
    }

    fn run(repo: &Path, label: &AgentLabel, io: &mut Fake) -> anyhow::Result<()> {
        run_with(
            repo,
            label,
            Tool::Codex,
            &SessionId::parse(MINE).unwrap(),
            io,
        )
    }

    /// In the repo's tab, the calling pane is renamed to the label and the
    /// label recorded as owned.
    #[test]
    fn it_renames_its_pane_and_owns_the_label() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(repo.path(), &[])));
        run(repo.path(), &painter(), &mut io).unwrap();
        assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(MINE));
        assert_eq!(
            io.renamed,
            [("terminal_5".to_string(), "painter".to_string())]
        );
        assert_eq!(io.owned, ["painter"]);
    }

    /// Another live pane already named `💤 painter` in the tab: refused
    /// before the bind, so painter keeps its old session and nothing is
    /// renamed or owned.
    #[test]
    fn a_duplicate_is_refused_before_binding() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(
            repo.path(),
            &[r#"{"id":7,"title":"💤 painter","tab_id":0}"#],
        )));
        let err = run(repo.path(), &painter(), &mut io).unwrap_err();
        assert!(format!("{err:#}").contains("terminal_7"), "{err:#}");
        assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(BOUND));
        assert!(io.renamed.is_empty() && io.owned.is_empty());
    }

    /// A live pane titled `💤 painter` while `💤 painter` is recorded as
    /// owned may be painter's or that label's: whether painter already has
    /// a pane cannot be known, so nothing is bound, renamed or owned.
    #[test]
    fn a_possible_duplicate_is_refused_before_binding() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(
            repo.path(),
            &[r#"{"id":7,"title":"💤 painter","tab_id":0}"#],
        )));
        io.record = Some(BTreeSet::from(["💤 painter".to_string()]));
        let err = format!("{:#}", run(repo.path(), &painter(), &mut io).unwrap_err());
        assert!(
            err.contains("cannot be known") && err.contains("terminal_7"),
            "{err}"
        );
        assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(BOUND));
        assert!(io.renamed.is_empty() && io.owned.is_empty());
    }

    /// A rename zellij refuses leaves the binding, says why, and fails.
    #[test]
    fn a_failed_rename_keeps_the_binding_and_fails() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(repo.path(), &[])));
        io.rename = Err("no such pane".into());
        let err = run(repo.path(), &painter(), &mut io).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("bound") && err.contains("no such pane"),
            "{err}"
        );
        assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(MINE));
        assert!(io.owned.is_empty());
    }

    /// No TUI pane in the session (unresolved workspace), or the caller in
    /// another tab: bound, not renamed, and the reason said.
    #[test]
    fn an_unresolved_workspace_binds_without_renaming() {
        let repo = repo();
        let elsewhere = format!(
            r#"[{{"id":1,"title":"","terminal_command":"clank tui --repo {}","tab_id":0}},
                {{"id":5,"title":"bash","tab_id":3}}]"#,
            repo.path().display()
        );
        for listing in [
            Some(r#"[{"id":5,"title":"bash","tab_id":0}]"#.to_string()),
            Some(elsewhere),
            None,
        ] {
            let mut io = Fake::in_pane(listing);
            run(repo.path(), &painter(), &mut io).unwrap();
            assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(MINE));
            assert!(io.renamed.is_empty() && io.owned.is_empty());
            assert!(
                io.said.iter().any(|l| l.contains("not renamed")),
                "{:?}",
                io.said
            );
        }
    }

    /// Outside zellij nothing is listed or renamed.
    #[test]
    fn outside_zellij_it_touches_no_pane() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(repo.path(), &[])));
        io.caller = CallerIdentity::NotInZellij;
        run(repo.path(), &painter(), &mut io).unwrap();
        assert_eq!(bound_to(repo.path(), "painter").as_deref(), Some(MINE));
        assert!(io.renamed.is_empty() && io.owned.is_empty());
        assert!(!io.said.iter().any(|l| l.contains("renamed")));
    }

    /// A label off the roster is renamed but never owned: owning it would
    /// have the TUI close the pane as a departed agent's.
    #[test]
    fn a_label_off_the_roster_is_renamed_but_not_owned() {
        let repo = repo();
        let mut io = Fake::in_pane(Some(tab(repo.path(), &[])));
        run(repo.path(), &AgentLabel::parse("guest").unwrap(), &mut io).unwrap();
        assert_eq!(
            io.renamed,
            [("terminal_5".to_string(), "guest".to_string())]
        );
        assert!(io.owned.is_empty());
    }
}
