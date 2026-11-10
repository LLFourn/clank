//! A lost park through the monitor's own deliverer seams: [`tend`]
//! registering and reaping waits, and [`ended`] judging a close. A loss
//! dates the agent's having no waiter; it is never a turn's end.

use super::*;
use crate::cli::agent_state::Activity;
use crate::cli::turn_custody::Turns;
use clank_core::Tool;

/// One agent's waits held as the monitor holds them, its record read by
/// recovery, and both driven through the loop's seams.
struct Handing {
    repo: tempfile::TempDir,
    tool: Tool,
    record: std::path::PathBuf,
    turns: Turns,
    recovery: crate::cli::recovery::Driver,
    clients: Vec<Client>,
    sockets: std::collections::BTreeMap<u64, tokio::sync::mpsc::Receiver<String>>,
}

const CODEX_ANSWERED: [&str; 3] = [
    r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#,
    r#"{"type":"event_msg","payload":{"type":"item_completed","turn_id":"T","item":{"type":"AgentMessage"}}}"#,
    r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"FINISHED"}]}}"#,
];

fn answered() -> Vec<String> {
    CODEX_ANSWERED
        .iter()
        .map(|l| l.replace("\"T\"", "\"t1\""))
        .collect()
}

fn agent() -> AgentLabel {
    AgentLabel::parse("agent").unwrap()
}

impl Handing {
    fn new(tool: Tool) -> Self {
        crate::cli::recovery::tests::enable(tool, Some(crate::cli::recovery::Transport::Typed));
        let repo = tempfile::tempdir().unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &agent(),
            crate::cli::teams_config::AgentDescription {
                tool,
                launch: None,
                initial_prompt: None,
            },
            crate::cli::teams_config::RosterRole::Commit,
        )
        .unwrap();
        crate::cli::agent::set_repo_master(repo.path(), &agent()).unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &agent(),
            tool,
            &clank_core::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        crate::agent_store::set_auto_mode(repo.path(), &agent(), clank_core::AutoMode::On).unwrap();
        let record = match tool {
            Tool::OpenCode => crate::cli::recovery::observed_path(
                &crate::agent_store::agents_root(repo.path()).join("agent"),
            ),
            _ => repo.path().join("record.jsonl"),
        };
        Self::resumed(repo, tool, record)
    }

    /// The same agent, record and disk under a monitor started afresh.
    fn resumed(repo: tempfile::TempDir, tool: Tool, record: std::path::PathBuf) -> Self {
        Handing {
            turns: Turns::new(repo.path().to_path_buf()),
            recovery: crate::cli::recovery::Driver::new("/".into(), "/nonexistent-home".into()),
            repo,
            tool,
            record,
            clients: Vec::new(),
            sockets: Default::default(),
        }
    }

    fn restart(self) -> Self {
        Self::resumed(self.repo, self.tool, self.record)
    }

    fn append(&mut self, lines: &[String], now: u64) {
        use std::io::Write as _;
        std::fs::create_dir_all(self.record.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.record)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        self.changed(now);
    }

    fn changed(&mut self, now: u64) {
        self.recovery.changed(
            crate::cli::recovery::RecordEvent::Changed {
                watch: crate::cli::recovery::WatchKey {
                    dir: self.record.parent().unwrap().to_path_buf(),
                    recursive: false,
                },
                path: self.record.clone(),
            },
            now,
        );
    }

    /// An agent wait admitted, as automatic Grok's registers: no
    /// turn-end, held until it is woken or its socket closes.
    fn wait(&mut self) -> u64 {
        let (out, socket) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let conn = next_conn();
        self.clients.push(Client {
            who: Served::Agent { author: agent() },
            holder: None,
            out,
            writer: None,
            farewell: None,
            shown: Default::default(),
            conn,
            admitted: Default::default(),
            report: None,
        });
        self.sockets.insert(conn, socket);
        self.tend();
        conn
    }

    /// Wait `conn`'s socket closes under it.
    fn lose(&mut self, conn: u64) {
        self.sockets.remove(&conn);
        self.tend();
    }

    /// Wait `conn` is woken: the monitor retires it, and then its socket
    /// closes.
    fn woken(&mut self, conn: u64) {
        self.clients.retain(|c| c.conn != conn);
        self.sockets.remove(&conn);
        self.tend();
    }

    /// The top of a loop round.
    fn tend(&mut self) {
        let _ = tend(
            self.repo.path(),
            &mut self.clients,
            None,
            &[],
            &Default::default(),
            &mut self.turns,
            &mut self.recovery,
        );
    }

    fn owed(&self, owed: bool) -> crate::cli::turn_custody::Gathered {
        let items = if owed {
            vec![clank_core::wait::WaitItem::Message {
                presentation: None,
                requires_reply: false,
                reply_route: None,
                handle: "m1".into(),
                from: None,
                repo: None,
                subject: "Rebase please".into(),
                body: "onto master".into(),
                selector: Some("messages@m1".into()),
            }]
        } else {
            vec![]
        };
        crate::cli::turn_custody::Gathered::of(self.repo.path(), &agent(), Some((&items, None)))
    }

    /// A round and its fold, owing work or not.
    fn fold(&mut self, owed: bool, now: u64) {
        self.tend();
        let g = self.owed(owed);
        let delivering = !self.clients.is_empty();
        let session = serde_json::from_value(serde_json::json!({
            "id": "session-one",
            "tool": self.tool.as_str(),
            "updated_at": "2026-10-28T00:00:00Z",
            "transcript": self.record,
        }))
        .unwrap();
        let facts = crate::cli::recovery::Facts {
            author: agent(),
            agent_dir: self.dir(),
            session,
            auto: true,
            ledger: crate::cli::ledger::Ledger::owing_if(owed && !delivering),
            role: "master".into(),
            work: if owed { vec!["m1".into()] } else { vec![] },
            blind_nudge: None,
            stall: None,
            attended: false,
            blocked_on: None,
            asking: false,
            projection: self.turns.projection(&agent(), &g),
        };
        self.recovery.reconcile(vec![facts], now);
    }

    fn unwatched(&mut self) -> Option<clank_core::turn_policy::Unwatched> {
        let g = self.owed(false);
        self.turns.projection(&agent(), &g).unwatched
    }

    fn dir(&self) -> std::path::PathBuf {
        crate::agent_store::agents_root(self.repo.path()).join("agent")
    }

    fn activity(&self) -> Activity {
        crate::cli::agent_state::load(&self.dir()).unwrap().activity
    }

    fn working(&self) -> bool {
        matches!(self.activity(), Activity::Working(_))
    }

    fn nudges_at(&mut self, now: u64) -> usize {
        self.recovery
            .due(now, |_, _| crate::cli::recovery::Gate::Go)
            .iter()
            .filter(|a| matches!(a, crate::cli::recovery::Act::Dispatch { .. }))
            .count()
    }

    /// Codex answered turn `t1` with no end marker while its wait was
    /// held, and the wait's socket closed.
    fn incident() -> Self {
        let mut a = Handing::new(Tool::Codex);
        a.append(&answered(), 0);
        let conn = a.wait();
        a.fold(true, 0);
        a.lose(conn);
        a
    }
}

/// A wait lost with its turn left running in the record is a lost wait
/// and nothing more: the loss is dated, and the turn is still as the
/// record has it. No Stop said it ended, so it is not idle and is not
/// nudged as idle, under this monitor or the next.
#[tokio::test]
async fn a_lost_wait_alone_ends_no_turn() {
    let mut a = Handing::incident();
    assert!(a.unwatched().is_some());
    a.fold(true, 10);
    assert!(a.working(), "{:?}", a.activity());
    assert_eq!(a.nudges_at(10 + 180), 0);

    let mut b = Handing::incident().restart();
    b.fold(true, 10);
    assert!(b.working(), "after a restart: {:?}", b.activity());
    assert_eq!(b.nudges_at(10 + 180), 0);
}

/// A wait the monitor woke, or one a newer wait replaced, leaves nothing
/// when its socket closes; a new wait clears a lost one.
#[tokio::test]
async fn only_the_newest_wait_lost_unanswered_is_recorded() {
    let mut a = Handing::new(Tool::Codex);
    a.append(&answered(), 0);
    let conn = a.wait();
    a.woken(conn);
    assert_eq!(a.unwatched(), None, "woken");

    let mut b = Handing::new(Tool::Codex);
    b.append(&answered(), 0);
    let (older, newer) = (b.wait(), b.wait());
    b.fold(true, 0);
    b.lose(older);
    assert_eq!(b.unwatched(), None, "an older wait");
    b.lose(newer);
    assert!(b.unwatched().is_some());
    b.wait();
    assert_eq!(b.unwatched(), None, "a new wait");
}
