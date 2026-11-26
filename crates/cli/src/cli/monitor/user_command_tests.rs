use super::*;
use crate::cli::monitor::fixtures::*;
use crate::cli::user_command::store::{PaneSeen, Record, State, Store};

const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#;

fn claude() -> AgentLabel {
    AgentLabel::parse("claude").unwrap()
}

fn sid() -> clank_core::SessionId {
    clank_core::SessionId::parse("session-one").unwrap()
}

/// An agent client holding a Stop of `session`, and the queue its
/// verdicts land in.
fn holding(session: &str) -> (Client, tokio::sync::mpsc::Receiver<String>) {
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let client = Client {
        who: Served::Agent { author: claude() },
        holder: None,
        out,
        writer: None,
        shown: Default::default(),
        conn: 7,
        admitted: Default::default(),
        unavailable: false,
        report: Some(Box::new(Reporting {
            report: crate::cli::turn_wire::Report::TurnEnd {
                stop: clank_core::turn_policy::TurnEndReport {
                    tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                    hook_started: None,
                    tool: clank_core::Tool::Claude,
                    session: clank_core::SessionId::parse(session).unwrap(),
                    place: Default::default(),
                },
                deadline: u64::MAX,
                questions: Vec::new(),
            },
            stepped: false,
            reader: None,
        })),
        farewell: None,
    };
    (client, queued)
}

struct World {
    repo: std::path::PathBuf,
    home: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let repo = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(repo.join(".clank/config.json"), ROSTER).unwrap();
    crate::agent_store::bind_session_to_agent(&repo, &claude(), clank_core::Tool::Claude, &sid())
        .unwrap();
    crate::agent_store::set_auto_mode(&repo, &claude(), clank_core::AutoMode::On).unwrap();
    World {
        repo,
        home: tempfile::tempdir().unwrap(),
        _dir: dir,
    }
}

impl World {
    fn store(&self) -> Store {
        Store::at(self.home.path().join(".clank/user-commands"))
    }

    /// File `record` as `r1`, admitted now with its pane already seen, so
    /// no pass goes looking for one.
    fn file(&self, mut record: Record) {
        let now = crate::cli::recovery::unix_now();
        let tag = repo_tag(&self.repo);
        let id = crate::cli::user_command::request::Id::parse("r1").unwrap();
        let held = self.store().lock(&tag, &claude(), &id).unwrap();
        record.admitted_at = now;
        record.pane = Some(PaneSeen {
            session: "zs".into(),
            pane: "terminal_1".into(),
            server: Some("zs".into()),
        });
        held.save(&record).unwrap();
    }
}

/// Through the loop's own round: a parked Stop stays parked with no
/// request and with one an older clank filed, and a request ends it with
/// the user-command verdict; the agent is then left settling, and the
/// round spawns no attempt while it does.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_ends_a_park_and_an_older_one_does_not() {
    let w = world();
    let state = crate::repo_state::RepoState::empty(w.repo.clone());
    let (mut uc, _wakes) = crate::cli::user_command::live::UserCommands::new(&w.repo, w.store());
    let (client, mut queue) = holding("session-one");
    let mut clients = vec![client];
    let mut turns = crate::cli::turn_custody::Turns::new(w.repo.clone());
    let mut recovery =
        crate::cli::recovery::Driver::new(w.repo.clone(), w.home.path().to_path_buf());
    let mut owned = OwnedTasks::default();
    let mut round = |clients: &mut Vec<Client>,
                     uc: &mut crate::cli::user_command::live::UserCommands,
                     turns: &mut crate::cli::turn_custody::Turns,
                     owned: &mut OwnedTasks| {
        in_clients(clients, |clients| {
            in_custody(turns, &mut recovery, |custody| {
                user_command_round(
                    &w.repo,
                    uc,
                    Some(&state),
                    clients,
                    &mut Sources::hosting(Default::default()),
                    custody,
                    owned,
                )
            })
        })
        .1
    };
    let _ = in_turns(&mut turns, |custody| {
        serve_reports(
            &w.repo,
            &mut clients,
            Some(&state),
            None,
            &mut Reading::afresh(),
            &mut Sources::hosting(Default::default()),
            custody,
        )
    });
    assert!(queue.try_recv().unwrap().contains("park"), "parked first");
    round(&mut clients, &mut uc, &mut turns, &mut owned);
    assert_eq!(clients.len(), 1, "no request, still parked");

    w.file(
        serde_json::from_slice(include_bytes!(
            "../../../tests/fixtures/formats/user-command-keyed-waiting.json"
        ))
        .unwrap(),
    );
    round(&mut clients, &mut uc, &mut turns, &mut owned);
    assert_eq!(clients.len(), 1, "an older request releases nothing");
    let tag = repo_tag(&w.repo);
    let id = crate::cli::user_command::request::Id::parse("r1").unwrap();
    assert_eq!(
        w.store().read(&tag, &claude(), &id).unwrap().unwrap().state,
        State::Refused
    );

    std::fs::remove_file(w.store().repo_dir(&tag).join("claude/r1.json")).unwrap();
    w.file(Record::admitted(
        sid(),
        crate::cli::user_command::request::Command::for_test("git push origin feature"),
        0,
    ));
    round(&mut clients, &mut uc, &mut turns, &mut owned);
    assert!(clients.is_empty(), "the park was answered and retired");
    let verdict = queue.try_recv().unwrap();
    assert!(verdict.contains("user_command"), "{verdict}");
    assert!(
        uc.settling(crate::cli::recovery::unix_now())
            .contains(&claude()),
        "the released agent settles before it is looked at as idle"
    );
    round(&mut clients, &mut uc, &mut turns, &mut owned);
    assert!(owned.departing.is_empty(), "no attempt while it settles");
    let record = w.store().read(&tag, &claude(), &id).unwrap().unwrap();
    assert_eq!(record.state, State::WaitingForIdle);
    assert_eq!(
        record.reason.as_deref(),
        Some("its released turn is ending")
    );
}

/// The loop's look: a Stop of the bound session held here is a park; one
/// of another session is not; a settling release and an agent recovery
/// does not follow yet both wait.
#[test]
fn the_loops_look_at_an_agent() {
    use crate::cli::user_command::dispatch::Look;
    let w = world();
    let mut turns = crate::cli::turn_custody::Turns::new(w.repo.clone());
    let mut recovery =
        crate::cli::recovery::Driver::new(w.repo.clone(), w.home.path().to_path_buf());
    let none = Default::default();
    let mut look = |clients: Vec<Client>,
                    recovery: &mut crate::cli::recovery::Driver,
                    settling: &std::collections::BTreeSet<AgentLabel>| {
        in_custody(&mut turns, recovery, |custody| {
            custody.look_for_user_command(
                &w.repo,
                &Clients::of(clients),
                settling,
                &claude(),
                &sid(),
            )
        })
    };
    let (mine, _q) = holding("session-one");
    assert_eq!(look(vec![mine], &mut recovery, &none), Look::Parked);
    let (other, _q) = holding("session-two");
    assert!(matches!(
        look(vec![other], &mut recovery, &none),
        Look::Wait(_)
    ));
    let settling = [claude()].into_iter().collect();
    assert_eq!(
        look(vec![], &mut recovery, &settling),
        Look::Wait("its released turn is ending".into())
    );
    assert!(matches!(look(vec![], &mut recovery, &none), Look::Wait(_)));
}
