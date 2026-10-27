use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::cli::fork::ForkSpec;

pub(crate) const SOURCE: &str = "ses_1a2b3c4d5e6fSourceAAAAAAAA";
pub(crate) const CHILD: &str = "ses_1a2b3c4d5e6eChildBBBBBBBBB";
const SOURCE_DIR: &str = "/source/repo";

type Answer = dyn Fn(&str, &str, &Option<Value>) -> Result<Value, CallError> + Send + Sync;

#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub method: String,
    pub session: String,
    pub body: Option<Value>,
    pub backend: Backend,
}

pub(crate) struct Fake {
    pub calls: Mutex<Vec<Call>>,
    answer: Box<Answer>,
}

impl Fake {
    pub(crate) fn new(
        answer: impl Fn(&str, &str, &Option<Value>) -> Result<Value, CallError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            answer: Box::new(answer),
        }
    }

    /// OpenCode as observed live: the child is created in the source's
    /// directory, `session.move` relocates it, `session.get` reads it back.
    pub(crate) fn opencode() -> Self {
        let dirs = Mutex::new(HashMap::from([(
            SOURCE.to_string(),
            SOURCE_DIR.to_string(),
        )]));
        Self::new(move |method, session, body| {
            let mut dirs = dirs.lock().unwrap();
            match method {
                "session.fork" => {
                    let dir = dirs[session].clone();
                    dirs.insert(CHILD.into(), dir);
                    Ok(json!({ "id": CHILD }))
                }
                "session.move" => {
                    let to = body.as_ref().unwrap()["directory"].as_str().unwrap();
                    dirs.insert(session.into(), to.into());
                    Ok(json!(true))
                }
                "session.get" => Ok(json!({
                    "id": session,
                    "location": { "directory": dirs[session] },
                })),
                other => panic!("unexpected {other}"),
            }
        })
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| format!("{} {}", c.method, c.session))
            .collect()
    }
}

impl Api for Fake {
    fn call(
        &self,
        backend: &Backend,
        method: &str,
        session: &str,
        body: Option<Value>,
    ) -> Result<Value, CallError> {
        self.calls.lock().unwrap().push(Call {
            method: method.into(),
            session: session.into(),
            body: body.clone(),
            backend: backend.clone(),
        });
        (self.answer)(method, session, &body)
    }
}

fn no_calls() -> Fake {
    Fake::new(|method, _, _| panic!("no OpenCode call expected, got {method}"))
}

fn label() -> AgentLabel {
    AgentLabel::parse("tester").unwrap()
}

pub(crate) fn opencode_spec() -> ForkSpec {
    ForkSpec {
        tool: Tool::OpenCode,
        from_session: Some(SOURCE.into()),
        prompt: "orient".into(),
        grok: None,
        opencode: None,
    }
}

fn fork_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    save_fork_spec(&repo, &label(), &opencode_spec()).unwrap();
    (dir, repo)
}

fn backend(repo: &Path) -> Backend {
    Backend {
        program: "opencode".into(),
        env: BTreeMap::new(),
        cwd: repo.to_path_buf(),
    }
}

fn run(repo: &Path, api: &dyn Api) -> anyhow::Result<Session> {
    prepare(repo, &label(), &backend(repo), None, api)
}

fn intent(repo: &Path) -> Option<Option<String>> {
    load_fork_spec(repo, &label())
        .unwrap()
        .expect("intent kept")
        .opencode
        .map(|i| i.child_id)
}

fn bound(repo: &Path) -> Option<String> {
    crate::agent_store::load_agent_config(repo, &label())
        .unwrap()
        .and_then(|c| c.session)
        .map(|s| s.id.as_str().to_string())
}

fn unknown(what: &str) -> CallError {
    CallError::Unknown(anyhow::anyhow!("{what}"))
}

#[test]
fn a_fork_is_forked_moved_and_confirmed_before_its_child_is_bound() {
    let (_d, repo) = fork_repo();
    let observed = Fake::opencode();
    let recorded_at_move = std::sync::Arc::new(Mutex::new(None));
    let api = Fake::new({
        let repo = repo.clone();
        let recorded = recorded_at_move.clone();
        move |method, session, body| {
            if method == "session.move" {
                *recorded.lock().unwrap() = Some(intent(&repo));
                assert_eq!(bound(&repo), None, "bound before placement");
            }
            observed.call(&backend(&repo), method, session, body.clone())
        }
    });
    let session = run(&repo, &api).unwrap();
    assert_eq!(session.id.as_str(), CHILD);
    assert_eq!(session.tool, Tool::OpenCode);
    assert_eq!(
        *recorded_at_move.lock().unwrap(),
        Some(Some(Some(CHILD.into()))),
        "the child is recorded before it is moved"
    );
    assert_eq!(
        api.methods(),
        [
            format!("session.fork {SOURCE}"),
            format!("session.move {CHILD}"),
            format!("session.get {CHILD}"),
        ]
    );
    assert_eq!(
        api.calls.lock().unwrap()[1].body,
        Some(json!({ "directory": repo }))
    );
    assert_eq!(bound(&repo).as_deref(), Some(CHILD));
    assert_eq!(
        intent(&repo),
        Some(Some(CHILD.into())),
        "the launch, not preparation, consumes the intent"
    );
}

#[test]
fn a_failed_move_keeps_the_child_and_the_next_start_moves_it_without_forking() {
    let (_d, repo) = fork_repo();
    let opencode = Fake::opencode();
    let failing = Fake::new(move |method, session, body| {
        if method == "session.move" {
            return Err(unknown("move refused"));
        }
        opencode.call(&backend(Path::new("/")), method, session, body.clone())
    });
    let err = run(&repo, &failing).unwrap_err();
    assert!(
        format!("{err:#}").contains("without forking again"),
        "{err:#}"
    );
    assert_eq!(bound(&repo), None);
    assert_eq!(intent(&repo), Some(Some(CHILD.into())));

    let (moved_to, api) = placing();
    assert_eq!(run(&repo, &api).unwrap().id.as_str(), CHILD);
    assert_eq!(
        api.methods(),
        [
            format!("session.move {CHILD}"),
            format!("session.get {CHILD}")
        ]
    );
    assert_eq!(
        moved_to.lock().unwrap().as_deref(),
        Some(repo.to_str().unwrap())
    );
    assert_eq!(bound(&repo).as_deref(), Some(CHILD));
}

/// An OpenCode that already holds CHILD, still in the source's directory,
/// and refuses to fork again.
fn placing() -> (std::sync::Arc<Mutex<Option<String>>>, Fake) {
    let dir = std::sync::Arc::new(Mutex::new(Some(SOURCE_DIR.to_string())));
    let seen = dir.clone();
    let api = Fake::new(move |method, session, body| {
        assert_eq!(session, CHILD, "{method} on the source");
        let mut dir = dir.lock().unwrap();
        match method {
            "session.move" => {
                *dir = body.as_ref().unwrap()["directory"]
                    .as_str()
                    .map(String::from);
                Ok(json!(true))
            }
            "session.get" => Ok(json!({ "location": { "directory": *dir } })),
            other => panic!("a known child is never forked again: {other}"),
        }
    });
    (seen, api)
}

#[test]
fn a_failed_bind_after_a_good_move_reuses_the_same_child() {
    let (_d, repo) = fork_repo();
    let config = crate::agent_store::agent_config_path(&repo, &label());
    let opencode = Fake::opencode();
    let api = Fake::new({
        let config = config.clone();
        move |method, session, body| {
            if method == "session.get" {
                std::fs::create_dir_all(&config).unwrap();
            }
            opencode.call(&backend(Path::new("/")), method, session, body.clone())
        }
    });
    let err = run(&repo, &api).unwrap_err();
    assert!(format!("{err:#}").contains("could not be bound"), "{err:#}");
    assert_eq!(intent(&repo), Some(Some(CHILD.into())));
    std::fs::remove_dir(&config).unwrap();

    let (_, api) = placing();
    assert_eq!(run(&repo, &api).unwrap().id.as_str(), CHILD);
    assert_eq!(bound(&repo).as_deref(), Some(CHILD));
}

#[test]
fn a_child_stored_elsewhere_is_not_bound() {
    let (_d, repo) = fork_repo();
    let api = Fake::new(|method, _, _| match method {
        "session.fork" => Ok(json!({ "id": CHILD })),
        "session.move" => Ok(json!(true)),
        _ => Ok(json!({ "location": { "directory": SOURCE_DIR } })),
    });
    let err = run(&repo, &api).unwrap_err();
    assert!(format!("{err:#}").contains(SOURCE_DIR), "{err:#}");
    assert_eq!(bound(&repo), None);
    assert_eq!(intent(&repo), Some(Some(CHILD.into())));
}

#[test]
fn concurrent_starts_fork_once_and_end_on_the_same_child() {
    let (_d, repo) = fork_repo();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let gate = Mutex::new(Some((entered_tx, release_rx)));
    let opencode = Fake::opencode();
    let api = Fake::new(move |method, session, body| {
        if method == "session.fork"
            && let Some((entered, release)) = gate.lock().unwrap().take()
        {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        opencode.call(&backend(Path::new("/")), method, session, body.clone())
    });
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| run(&repo, &api));
        entered.recv().unwrap();
        let b = s.spawn(|| run(&repo, &api));
        std::thread::sleep(std::time::Duration::from_millis(100));
        release.send(()).unwrap();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(a.unwrap().id.as_str(), CHILD);
    assert_eq!(b.unwrap().id.as_str(), CHILD);
    let forks = api
        .methods()
        .iter()
        .filter(|m| m.starts_with("session.fork"))
        .count();
    assert_eq!(forks, 1);
}

#[test]
fn a_lost_fork_reply_is_never_replayed() {
    let (_d, repo) = fork_repo();
    let api = Fake::new(|_, _, _| Err(unknown("connection reset")));
    let err = run(&repo, &api).unwrap_err();
    assert!(
        format!("{err:#}").contains("clank fork recover tester"),
        "{err:#}"
    );
    assert_eq!(intent(&repo), Some(None));

    let err = run(&repo, &no_calls()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("may exist"), "{msg}");
    assert!(
        msg.contains("--child <id>") && msg.contains("--forget"),
        "{msg}"
    );
    assert_eq!(bound(&repo), None);
    assert_eq!(intent(&repo), Some(None));
}

#[test]
fn a_fork_opencode_refused_clears_the_request() {
    let (_d, repo) = fork_repo();
    let api = Fake::new(|_, _, _| Err(CallError::Refused(anyhow::anyhow!("no such session"))));
    let err = run(&repo, &api).unwrap_err();
    assert!(format!("{err:#}").contains("no such session"), "{err:#}");
    assert_eq!(intent(&repo), None);
    assert_eq!(bound(&repo), None);
}

#[test]
fn a_reply_naming_the_source_is_refused_before_any_move() {
    let (_d, repo) = fork_repo();
    let api = Fake::new(|_, _, _| Ok(json!({ "id": SOURCE })));
    assert!(run(&repo, &api).is_err());
    assert_eq!(api.methods(), [format!("session.fork {SOURCE}")]);
    assert_eq!(intent(&repo), Some(None));
    assert_eq!(bound(&repo), None);
}

#[test]
fn a_bound_child_resumes_with_no_call() {
    let (_d, repo) = fork_repo();
    run(&repo, &Fake::opencode()).unwrap();
    assert_eq!(run(&repo, &no_calls()).unwrap().id.as_str(), CHILD);
}

fn lost(repo: &Path) {
    let api = Fake::new(|_, _, _| Err(unknown("connection reset")));
    run(repo, &api).unwrap_err();
}

#[test]
fn an_adopted_child_is_placed_and_confirmed_before_it_is_bound() {
    let (_d, repo) = fork_repo();
    lost(&repo);
    recover(&repo, &label(), Recovery::Adopt(CHILD.into())).unwrap();
    assert_eq!(bound(&repo), None);
    assert_eq!(intent(&repo), Some(Some(CHILD.into())));

    let failing = Fake::new(|_, _, _| Err(unknown("move refused")));
    run(&repo, &failing).unwrap_err();
    assert_eq!(failing.methods(), [format!("session.move {CHILD}")]);
    assert_eq!(bound(&repo), None);

    let (_, api) = placing();
    assert_eq!(run(&repo, &api).unwrap().id.as_str(), CHILD);
    assert_eq!(
        api.methods(),
        [
            format!("session.move {CHILD}"),
            format!("session.get {CHILD}")
        ]
    );
    assert_eq!(bound(&repo).as_deref(), Some(CHILD));
}

#[test]
fn a_failed_confirmation_during_recovery_binds_nothing() {
    let (_d, repo) = fork_repo();
    lost(&repo);
    recover(&repo, &label(), Recovery::Adopt(CHILD.into())).unwrap();
    let api = Fake::new(|method, _, _| match method {
        "session.move" => Ok(json!(true)),
        _ => Err(unknown("get failed")),
    });
    run(&repo, &api).unwrap_err();
    assert_eq!(bound(&repo), None);
    assert_eq!(intent(&repo), Some(Some(CHILD.into())));
}

#[test]
fn forget_clears_only_this_agents_request() {
    let (_d, repo) = fork_repo();
    let other = AgentLabel::parse("other").unwrap();
    let mut theirs = opencode_spec();
    theirs.opencode = Some(OpenCodeForkIntent { child_id: None });
    save_fork_spec(&repo, &other, &theirs).unwrap();
    let theirs_before = std::fs::read(crate::cli::fork::fork_spec_path(&repo, &other)).unwrap();
    lost(&repo);
    recover(&repo, &label(), Recovery::Forget).unwrap();
    assert_eq!(intent(&repo), None);
    assert_eq!(
        std::fs::read(crate::cli::fork::fork_spec_path(&repo, &other)).unwrap(),
        theirs_before
    );
    run(&repo, &Fake::opencode()).unwrap();
    assert_eq!(bound(&repo).as_deref(), Some(CHILD));
}

#[test]
fn recovery_refuses_what_it_cannot_settle() {
    let refused = |repo: &Path, recovery: Recovery| {
        let path = crate::cli::fork::fork_spec_path(repo, &label());
        let before = std::fs::read(&path).ok();
        let err = recover(repo, &label(), recovery).unwrap_err();
        assert_eq!(std::fs::read(&path).ok(), before, "{err:#}");
        format!("{err:#}")
    };

    let none = tempfile::tempdir().unwrap();
    assert!(refused(none.path(), Recovery::Forget).contains("no OpenCode fork intent"));

    let (_d, codex) = fork_repo();
    let mut spec = opencode_spec();
    spec.tool = Tool::Codex;
    save_fork_spec(&codex, &label(), &spec).unwrap();
    assert!(refused(&codex, Recovery::Adopt(CHILD.into())).contains("no OpenCode fork intent"));

    let (_d, unrequested) = fork_repo();
    assert!(refused(&unrequested, Recovery::Forget).contains("never requested"));

    let (_d, source) = fork_repo();
    lost(&source);
    assert!(refused(&source, Recovery::Adopt(SOURCE.into())).contains("source"));

    let (_d, recorded) = fork_repo();
    let api = Fake::new(|method, _, _| match method {
        "session.fork" => Ok(json!({ "id": CHILD })),
        _ => Err(unknown("down")),
    });
    run(&recorded, &api).unwrap_err();
    for r in [Recovery::Forget, Recovery::Adopt(CHILD.into())] {
        assert!(refused(&recorded, r).contains("already has child"));
    }

    let (_d, bound_repo) = fork_repo();
    lost(&bound_repo);
    crate::agent_store::bind_session_to_agent(
        &bound_repo,
        &label(),
        Tool::OpenCode,
        &SessionId::parse(CHILD).unwrap(),
    )
    .unwrap();
    assert!(refused(&bound_repo, Recovery::Forget).contains("is bound to"));
}

#[test]
fn backend_selecting_profiles_are_refused() {
    let with = |args: &[&str]| LaunchConfig {
        command: None,
        args: args.iter().map(|a| a.to_string()).collect(),
        env: BTreeMap::new(),
    };
    for args in [
        &["--server", "http://h:1"][..],
        &["--server=http://h:1"],
        &["--standalone"],
    ] {
        assert!(validate_profile(Some(&with(args))).is_err(), "{args:?}");
    }
    validate_profile(Some(&with(&["--model", "m", "--agent", "a"]))).unwrap();
    validate_profile(None).unwrap();
}

#[test]
fn only_a_missing_session_reads_as_a_refusal() {
    assert!(names_missing_session(
        &json!({ "_tag": "SessionNotFoundError", "sessionID": SOURCE })
    ));
    assert!(names_missing_session(
        &json!({ "error": { "name": "SessionNotFoundError" } })
    ));
    assert!(!names_missing_session(
        &json!({ "error": { "name": "UnknownError" } })
    ));
    assert!(!names_missing_session(&json!("SessionNotFoundError")));
}

/// An executable standing in for `opencode` at the process boundary: it
/// records its argv and answers with the given output and exit code.
fn answering(stdout: &str, stderr: &str, code: i32) -> (tempfile::TempDir, Backend) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("opencode");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' \"$OUT\"\nprintf '%s' \"$ERR\" >&2\nexit \"$CODE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = Backend {
        program: program.to_string_lossy().into_owned(),
        env: BTreeMap::from([
            ("OUT".to_string(), stdout.to_string()),
            ("ERR".to_string(), stderr.to_string()),
            ("CODE".to_string(), code.to_string()),
        ]),
        cwd: dir.path().to_path_buf(),
    };
    (dir, backend)
}

fn call(backend: &Backend, method: &str) -> Result<Value, CallError> {
    Cli.call(backend, method, CHILD, Some(json!({ "directory": "/w" })))
}

#[test]
fn an_empty_successful_answer_is_a_move_and_nothing_else() {
    let (dir, backend) = answering("", "", 0);
    assert_eq!(call(&backend, "session.move").ok(), Some(Value::Null));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("opencode.args")).unwrap(),
        format!("api\nsession.move\n--param\nsessionID={CHILD}\n-d\n{{\"directory\":\"/w\"}}\n")
    );
    for method in ["session.fork", "session.get"] {
        assert!(
            matches!(call(&backend, method), Err(CallError::Unknown(_))),
            "{method}"
        );
    }
}

#[test]
fn a_data_answer_is_returned_for_fork_and_get() {
    let (_d, backend) = answering(
        r#"{"data":{"id":"ses_x","location":{"directory":"/abs/path"}}}"#,
        "",
        0,
    );
    for method in ["session.fork", "session.get"] {
        let data = call(&backend, method).ok().unwrap();
        assert_eq!(data["location"]["directory"], "/abs/path", "{method}");
    }
}

#[test]
fn a_missing_session_is_refused_and_any_other_failure_is_unknown() {
    let (_d, missing) = answering(
        &format!(
            r#"{{"_tag":"SessionNotFoundError","sessionID":"{SOURCE}","message":"Session not found"}}"#
        ),
        "HTTP 404 Not Found",
        1,
    );
    assert!(matches!(
        call(&missing, "session.fork"),
        Err(CallError::Refused(_))
    ));
    let (_d, broken) = answering("", "connection reset", 1);
    for method in ["session.fork", "session.move", "session.get"] {
        assert!(
            matches!(call(&broken, method), Err(CallError::Unknown(_))),
            "{method}"
        );
    }
}
