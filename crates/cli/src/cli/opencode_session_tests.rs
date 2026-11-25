use std::collections::BTreeMap;
use std::sync::Mutex;

use super::*;
use clank_core::vocab::Tool;

pub(crate) const RECORDED: &str = include_str!("../../tests/fixtures/opencode-2.1.4-session.json");

fn recorded() -> Value {
    serde_json::from_str(RECORDED).unwrap()
}

fn recorded_row(key: &str) -> String {
    recorded()[key]["stdout"].as_str().unwrap().to_string()
}

type Reply = dyn Fn(&Value) -> Result<Value, CallError> + Send + Sync;

/// OpenCode 2.1.4 as recorded: `session.create` stores the id it is given
/// in the directory it is given and answers the stored record,
/// `session.get` answers it with its metadata, `session.update` replaces
/// the metadata whole, and the store answers each id's row as the recorded
/// query did.
pub(crate) struct OpenCode {
    sessions: Mutex<BTreeMap<String, String>>,
    pub metadata: Mutex<BTreeMap<String, Value>>,
    unreadable: Option<String>,
    reply: Box<Reply>,
    announced: Box<Reply>,
    pub calls: Mutex<Vec<String>>,
    pub markers: Mutex<Vec<(String, Value)>>,
}

impl OpenCode {
    pub(crate) fn recorded() -> Self {
        Self::replying(|body| {
            let mut data = recorded()["session_create"]["stdout"]["data"].clone();
            data["id"] = body["id"].clone();
            data["location"]["directory"] = body["location"]["directory"].clone();
            Ok(data)
        })
    }

    pub(crate) fn replying(
        reply: impl Fn(&Value) -> Result<Value, CallError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sessions: Mutex::default(),
            metadata: Mutex::default(),
            unreadable: None,
            reply: Box::new(reply),
            announced: Box::new(|_| Ok(Value::Null)),
            calls: Mutex::default(),
            markers: Mutex::default(),
        }
    }

    /// OpenCode as recorded, answering `session.update` with `answer`.
    pub(crate) fn announcing(
        answer: impl Fn(&Value) -> Result<Value, CallError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            announced: Box::new(answer),
            ..Self::recorded()
        }
    }

    /// A store that cannot be read, as when `sqlite3` is missing.
    pub(crate) fn unreadable(why: &str) -> Self {
        Self {
            unreadable: Some(why.into()),
            ..Self::recorded()
        }
    }

    /// OpenCode already stores `id` in `dir`, with `metadata` if any.
    pub(crate) fn holding(self, id: &str, dir: &Path, metadata: Option<Value>) -> Self {
        self.sessions
            .lock()
            .unwrap()
            .insert(id.into(), dir.to_string_lossy().into());
        if let Some(m) = metadata {
            self.metadata.lock().unwrap().insert(id.into(), m);
        }
        self
    }

    pub(crate) fn has(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    pub(crate) fn created(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("session.create"))
            .count()
    }
}

impl Api for OpenCode {
    fn call(
        &self,
        _: &Backend,
        method: &str,
        session: &str,
        body: Option<Value>,
    ) -> Result<Value, CallError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{method} {session}"));
        if method == "session.get" {
            let dir = self.sessions.lock().unwrap().get(session).cloned();
            let Some(dir) = dir else {
                let missing = &recorded()["session_get_unknown"]["stdout"];
                return Err(CallError::Refused(anyhow::anyhow!("{missing}")));
            };
            let mut info = json!({ "id": session, "location": { "directory": dir } });
            if let Some(m) = self.metadata.lock().unwrap().get(session) {
                info["metadata"] = m.clone();
            }
            return Ok(info);
        }
        let body = body.expect("every modelled call but a get has a body");
        if method == "session.update" {
            assert!(
                self.has(session),
                "OpenCode 2.1.4 refuses to update {session}"
            );
            (self.announced)(&body)?;
            self.metadata
                .lock()
                .unwrap()
                .insert(session.into(), body["metadata"].clone());
            self.markers
                .lock()
                .unwrap()
                .push((session.into(), body["metadata"].clone()));
            return Ok(Value::Null);
        }
        assert_eq!(method, "session.create", "no other call is modelled");
        let reply = (self.reply)(&body)?;
        if let (Some(id), Some(dir)) = (
            reply.get("id").and_then(Value::as_str),
            reply.pointer("/location/directory").and_then(Value::as_str),
        ) {
            self.sessions.lock().unwrap().insert(id.into(), dir.into());
        }
        Ok(reply)
    }

    fn stored(&self, _: &Backend, session: &str) -> Stored {
        if let Some(why) = &self.unreadable {
            return Stored::Unknown(why.clone());
        }
        parse_row(&recorded_row(if self.has(session) {
            "store_row_after_create"
        } else {
            "store_row_for_unknown"
        }))
    }
}

fn label() -> AgentLabel {
    AgentLabel::parse("kimi").unwrap()
}

fn backend(repo: &Path) -> Backend {
    Backend {
        program: "opencode".into(),
        env: BTreeMap::new(),
        cwd: repo.to_path_buf(),
        server: vec!["--standalone".into()],
    }
}

fn bound(repo: &Path) -> Option<Session> {
    crate::agent_store::binding(repo, &label()).unwrap()
}

#[test]
fn the_recorded_rows_read_as_stored_and_as_missing() {
    assert_eq!(
        parse_row(&recorded_row("store_row_after_create")),
        Stored::Never
    );
    assert_eq!(
        parse_row(&recorded_row("store_row_for_unknown")),
        Stored::Missing
    );
    assert_eq!(
        parse_row("1793065434866|interrupted\n"),
        Stored::At {
            unix_secs: 1_793_065_434,
            outcome: "interrupted".into()
        }
    );
    assert!(matches!(parse_row("x|y"), Stored::Unknown(_)));
}

#[test]
fn a_store_that_is_not_there_says_it_cannot_tell() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_string_lossy().into_owned();
    let var = |k: &str| (k == "HOME").then(|| home.clone());
    assert!(matches!(
        Store::of(&[], &var).lookup("ses_def05a43cffecoSf06vq7L9uBv"),
        Stored::Unknown(why) if why.contains("not found")
    ));
    let db = dir.path().join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(&db).unwrap();
    assert!(
        matches!(Store::of(&[], &var).lookup("ses_x"), Stored::Unknown(_)),
        "a directory is not a store"
    );
}

#[test]
fn the_store_is_the_one_the_launch_selects() {
    let at = |vars: &[(&str, &str)], server: &[&str]| {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let server: Vec<String> = server.iter().map(|s| s.to_string()).collect();
        Store::of(&server, &|k| vars.get(k).cloned()).0
    };
    assert_eq!(
        at(&[("HOME", "/h")], &[]).unwrap(),
        Path::new("/h/.local/share/opencode/opencode.db")
    );
    assert_eq!(
        at(
            &[("HOME", "/h"), ("XDG_DATA_HOME", "/x")],
            &["--standalone"]
        )
        .unwrap(),
        Path::new("/x/opencode/opencode.db"),
        "a private server reads the same store"
    );
    assert_eq!(
        at(&[("HOME", "/h"), ("OPENCODE_DB", "other.db")], &[]).unwrap(),
        Path::new("/h/.local/share/opencode/other.db")
    );
    assert_eq!(
        at(&[("HOME", "/h"), ("OPENCODE_DB", "/abs/o.db")], &[]).unwrap(),
        Path::new("/abs/o.db")
    );
    assert!(at(&[("HOME", "/h"), ("OPENCODE_DB", ":memory:")], &[]).is_err());
    for server in [&["--server", "http://h:1"][..], &["--server=http://h:1"]] {
        assert!(at(&[("HOME", "/h")], server).is_err(), "{server:?}");
    }
    assert!(at(&[], &[]).is_err());
}

#[test]
fn the_server_a_profile_selects_is_the_one_its_api_calls_reach() {
    let launch = |args: &[&str]| clank_core::agent_config::LaunchConfig {
        args: args.iter().map(|a| a.to_string()).collect(),
        ..Default::default()
    };
    let server = |args: &[&str]| super::super::opencode_fork::server_args(Some(&launch(args)));
    assert_eq!(
        server(&["--model", "m", "--server", "http://h:1", "--auto"]),
        ["--server", "http://h:1"]
    );
    assert_eq!(
        server(&["--server=http://h:1", "--standalone"]),
        ["--server=http://h:1", "--standalone"]
    );
    assert!(server(&["--model", "m"]).is_empty());
}

#[test]
fn a_fresh_binding_names_a_session_opencode_created_in_the_repo() {
    let repo = tempfile::tempdir().unwrap();
    let oc = OpenCode::recorded();
    let s = bind_created(repo.path(), &label(), &backend(repo.path()), None, &oc).unwrap();
    assert_eq!(bound(repo.path()).as_ref(), Some(&s));
    assert_eq!(s.tool, Tool::OpenCode);
    assert!(oc.has(s.id.as_str()), "{s:?}");
    assert_eq!(
        oc.stored(&backend(repo.path()), s.id.as_str()),
        Stored::Never
    );
    assert_eq!(oc.created(), 1);
    assert!(
        oc.markers.lock().unwrap().is_empty(),
        "a private server is not told"
    );
}

fn shared(repo: &Path) -> Backend {
    Backend {
        server: Vec::new(),
        ..backend(repo)
    }
}

/// OpenCode's service may have seen the creation before the binding;
/// only the marker written after it tells its plugin to wait.
#[test]
fn a_shared_server_is_told_of_the_binding_only_once_it_is_written() {
    let repo = tempfile::tempdir().unwrap();
    let at_announce = std::sync::Arc::new(Mutex::new(None));
    let seen = at_announce.clone();
    let path = repo.path().to_path_buf();
    let oc = OpenCode::announcing(move |_| {
        *seen.lock().unwrap() = Some(crate::agent_store::binding(&path, &label()).unwrap());
        Ok(Value::Null)
    });
    let s = bind_created(repo.path(), &label(), &shared(repo.path()), None, &oc).unwrap();
    assert_eq!(*at_announce.lock().unwrap(), Some(Some(s.clone())));
    let markers = oc.markers.lock().unwrap();
    let [(id, marker)] = &markers[..] else {
        panic!("one marker: {markers:?}")
    };
    assert_eq!(id, s.id.as_str());
    assert_eq!(marker["clank"]["bound"], "kimi");
    assert_eq!(marker, &bound_marker(&label()));
}

#[test]
fn a_binding_opencode_was_not_told_of_is_undone() {
    let repo = tempfile::tempdir().unwrap();
    for failure in [
        CallError::Refused(anyhow::anyhow!("no server")),
        CallError::Unknown(anyhow::anyhow!("timed out")),
    ] {
        let failure = Mutex::new(Some(failure));
        let oc = OpenCode::announcing(move |_| Err(failure.lock().unwrap().take().unwrap()));
        let err = bind_created(repo.path(), &label(), &shared(repo.path()), None, &oc).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("`kimi` was not launched") && err.contains("the binding is undone"),
            "{err}"
        );
        assert_eq!(bound(repo.path()), None, "{err}");
    }
}

#[test]
fn a_create_opencode_does_not_confirm_binds_nothing() {
    let repo = tempfile::tempdir().unwrap();
    let elsewhere = OpenCode::replying(|body| {
        let mut data = body.clone();
        data["location"]["directory"] = json!("/somewhere/else");
        Ok(data)
    });
    let another = OpenCode::replying(|body| {
        let mut data = body.clone();
        data["id"] = json!("ses_def05a43cffecoSf06vq7L9uBv");
        Ok(data)
    });
    let refused = OpenCode::replying(|_| Err(CallError::Refused(anyhow::anyhow!("no server"))));
    let unknown = OpenCode::replying(|_| Err(CallError::Unknown(anyhow::anyhow!("timed out"))));
    for (oc, says) in [
        (&elsewhere, "does not confirm it"),
        (&another, "does not confirm it"),
        (&refused, "OpenCode did not create a session"),
        (&unknown, "the next start creates another"),
    ] {
        let err = bind_created(repo.path(), &label(), &backend(repo.path()), None, oc).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("`kimi` was not launched") && err.contains(says),
            "{err}"
        );
        assert_eq!(bound(repo.path()), None, "{err}");
    }
}

/// 2.1.4's `session.update` replaces the metadata whole: the marker is set
/// on what the session already holds, and nothing else changes.
#[test]
fn announcing_keeps_every_other_metadata_key() {
    let repo = tempfile::tempdir().unwrap();
    let oc = OpenCode::recorded();
    let id = mint_session_id();
    oc.sessions
        .lock()
        .unwrap()
        .insert(id.as_str().into(), repo.path().to_string_lossy().into());
    let before = json!({ "other": { "plugin": 1 }, "clank": { "note": "kept", "bound": "old" } });
    oc.metadata
        .lock()
        .unwrap()
        .insert(id.as_str().into(), before);
    announce(&oc, &shared(repo.path()), &label(), &id)
        .ok()
        .unwrap();
    assert_eq!(
        oc.metadata.lock().unwrap()[id.as_str()],
        json!({ "other": { "plugin": 1 }, "clank": { "note": "kept", "bound": "kimi" } })
    );
}

#[test]
fn only_a_marker_still_standing_is_announced_again() {
    let repo = tempfile::tempdir().unwrap();
    let oc = OpenCode::recorded();
    let id = mint_session_id();
    oc.sessions
        .lock()
        .unwrap()
        .insert(id.as_str().into(), repo.path().to_string_lossy().into());
    let again = || {
        reannounce_pending(&oc, &shared(repo.path()), &label(), &id)
            .ok()
            .unwrap()
    };
    assert!(!again(), "a session never marked is not");
    oc.metadata
        .lock()
        .unwrap()
        .insert(id.as_str().into(), json!({ "keep": 1, "clank": {} }));
    assert!(!again(), "a consumed marker is not");
    oc.metadata.lock().unwrap().insert(
        id.as_str().into(),
        json!({ "keep": 1, "clank": { "bound": "kimi" } }),
    );
    assert!(again());
    assert_eq!(
        oc.markers.lock().unwrap().last().unwrap().1,
        json!({ "keep": 1, "clank": { "bound": "kimi" } })
    );
    assert!(
        !reannounce_pending(&oc, &backend(repo.path()), &label(), &id)
            .ok()
            .unwrap(),
        "a private server is never told"
    );
}
