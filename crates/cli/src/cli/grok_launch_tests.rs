use super::super::fork::{ForkSpec, fork_spec_path};
use super::*;
use clank_core::ids::SessionId;
use clank_core::vocab::Tool;

const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";

// Real production pipe/process seam, without models, credentials or terminals.
const FAKE: &str = r#"
import json, os, sys, pathlib, urllib.parse, time
root=pathlib.Path(os.environ['FAKE_ROOT'])
mode=os.environ.get('FAKE_MODE','good')
def record(v):
    with (root/'audit.jsonl').open('a') as f:f.write(json.dumps(v)+'\n')
record({'argv':sys.argv[1:],'pid':os.getpid(),'identity':{k:os.environ.get(k) for k in ['CLANK_AGENT','GROK_AGENT','GROK_SESSION_ID','CODEX_THREAD_ID','CLAUDE_CODE_SESSION_ID','OPENCODE_SESSION_ID','CLANK_BOOTSTRAP_SESSION_ID']}})
for line in sys.stdin:
    req=json.loads(line);record(req);rid=req['id']
    if mode=='hang':time.sleep(90)
    if mode=='flood':sys.stdout.write('x'*1100000);sys.stdout.flush();time.sleep(90)
    if req['method']=='initialize':result={'protocolVersion':2 if mode=='version' else 1}
    else:
        if mode=='unsupported':
            print(json.dumps({'jsonrpc':'2.0','id':rid,'error':{'code':-32601,'message':'method not found'}}),flush=True);continue
        if mode=='malformed':print('not json',flush=True);continue
        if mode=='wrong_id':rid+=1
        p=req['params'];cwd=p['newCwd'];sid=p['newSessionId']
        if mode=='stderr':sys.stderr.write('e'*150000);sys.stderr.flush()
        directory=root/'sessions'/urllib.parse.quote(cwd,safe='')/sid
        directory.mkdir(parents=True)
        summary={'info':{'id':sid,'cwd':cwd},'parent_session_id':p['sourceSessionId']}
        if mode=='bad_summary':summary['info']['cwd']=p['sourceCwd']
        (directory/'summary.json').write_text(json.dumps(summary))
        result={'newSessionId':sid,'newCwd':cwd,'parentSessionId':p['sourceSessionId']}
        if mode=='bad_response':result['newCwd']=p['sourceCwd']
    print(json.dumps({'jsonrpc':'2.0','id':rid,'result':result}),flush=True)
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    repo: PathBuf,
    provider: Provider,
    label: AgentLabel,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        let repo = base.join("child");
        let root = base.join("grok");
        for p in [&source, &repo, &root] {
            std::fs::create_dir_all(p).unwrap();
        }
        let script = base.join("fake.py");
        std::fs::write(&script, FAKE).unwrap();
        let provider = Provider {
            program: "/usr/bin/python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: BTreeMap::from([
                ("FAKE_ROOT".into(), root.to_string_lossy().into_owned()),
                ("FAKE_MODE".into(), mode.into()),
            ]),
            root,
        };
        let label = AgentLabel::parse("grok").unwrap();
        let f = Self {
            _temp: temp,
            source,
            repo,
            provider,
            label,
        };
        f.summary(&f.source, SOURCE, None);
        save_fork_spec(
            &f.repo,
            &f.label,
            &ForkSpec {
                opencode: None,
                tool: Tool::Grok,
                from_session: Some(SOURCE.into()),
                prompt: "orient child".into(),
                grok: Some(ForkIntent {
                    source_cwd: f.source.clone(),
                    child_id: CHILD.into(),
                    prepared: false,
                }),
            },
        )
        .unwrap();
        f
    }
    fn summary(&self, cwd: &Path, id: &str, parent: Option<&str>) {
        let dir = session_dir(&self.provider.root, cwd, id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_vec(&json!({"info":{"id":id,"cwd":cwd},"parent_session_id":parent}))
                .unwrap(),
        )
        .unwrap();
    }
    fn spec(&self) -> ForkSpec {
        load_fork_spec(&self.repo, &self.label).unwrap().unwrap()
    }
    fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(self.provider.root.join("audit.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn prepare(&self) -> anyhow::Result<Prepared> {
        prepare(&self.repo, &self.label, &self.provider).await
    }
}

#[tokio::test]
async fn child_copy_preserves_source_and_consumes_only_on_matching_bind() {
    let mut f = Fixture::new("good");
    for key in crate::agent_env::SESSION_IDENTITY_VARS {
        f.provider.env.insert((*key).into(), "foreign".into());
    }
    let sid = SessionId::parse(SOURCE).unwrap();
    crate::agent_store::bind_session_to_agent(&f.source, &f.label, Tool::Grok, &sid).unwrap();
    let config = crate::agent_store::agent_config_path(&f.source, &f.label);
    let original = std::fs::read(&config).unwrap();
    let p = f.prepare().await.unwrap();
    assert_eq!(p.id, CHILD);
    assert!(p.resume);
    assert_eq!(p.prompt, "orient child");
    assert!(f.spec().grok.unwrap().prepared);
    assert_eq!(std::fs::read(&config).unwrap(), original);
    let a = f.audit();
    assert_eq!(a[0]["argv"], json!(["agent", "--no-leader", "stdio"]));
    assert!(
        a[0]["identity"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_null)
    );
    assert_eq!(a[2]["method"], "_x.ai/session/fork");
    assert_eq!(
        a[2]["params"]["sourceCwd"],
        f.source.to_string_lossy().as_ref()
    );
    assert_eq!(a[2]["params"]["newCwd"], f.repo.to_string_lossy().as_ref());
    assert!(
        crate::agent_store::bind_session_to_agent(&f.repo, &f.label, Tool::Grok, &sid).is_err()
    );
    assert!(
        crate::agent_store::load_agent_config(&f.repo, &f.label)
            .unwrap()
            .is_none()
    );
    assert!(fork_spec_path(&f.repo, &f.label).exists());
    crate::agent_store::bind_session_to_agent(
        &f.repo,
        &f.label,
        Tool::Grok,
        &SessionId::parse(CHILD).unwrap(),
    )
    .unwrap();
    assert!(!fork_spec_path(&f.repo, &f.label).exists());
    assert_eq!(std::fs::read(&config).unwrap(), original);
}
#[tokio::test]
async fn prepared_retry_reuses_exact_child_without_helper() {
    let f = Fixture::new("good");
    f.prepare().await.unwrap();
    let count = f.audit().len();
    std::fs::remove_file(&f.provider.args[0]).unwrap();
    assert_eq!(f.prepare().await.unwrap().id, CHILD);
    assert_eq!(f.audit().len(), count);
}
#[tokio::test]
async fn legacy_spec_upgrades_once_using_exact_source_id() {
    let f = Fixture::new("good");
    let mut s = f.spec();
    s.grok = None;
    save_fork_spec(&f.repo, &f.label, &s).unwrap();
    let p = f.prepare().await.unwrap();
    assert_ne!(p.id, SOURCE);
    uuid(&p.id).unwrap();
    assert_eq!(f.spec().grok.unwrap().source_cwd, f.source);
    assert_eq!(f.prepare().await.unwrap().id, p.id);
}
#[tokio::test]
async fn absent_source_starts_fresh_with_pinned_id_and_keeps_intent() {
    let f = Fixture::new("good");
    std::fs::remove_dir_all(session_dir(&f.provider.root, &f.source, SOURCE)).unwrap();
    let p = f.prepare().await.unwrap();
    assert!(!p.resume);
    assert_eq!(p.id, CHILD);
    assert!(f.spec().from_session.is_none());
    assert!(!f.provider.root.join("audit.jsonl").exists());
    f.summary(&f.repo, CHILD, None);
    assert!(f.prepare().await.unwrap().resume);
}
#[tokio::test]
async fn incomplete_source_is_not_a_fresh_fallback() {
    let f = Fixture::new("good");
    std::fs::remove_file(session_dir(&f.provider.root, &f.source, SOURCE).join("summary.json"))
        .unwrap();
    assert!(f.prepare().await.is_err());
    assert_eq!(f.spec().from_session.as_deref(), Some(SOURCE));
    assert!(!f.provider.root.join("audit.jsonl").exists());
}
#[tokio::test]
async fn rpc_failures_retain_unprepared_intent() {
    for mode in [
        "version",
        "unsupported",
        "malformed",
        "wrong_id",
        "bad_response",
        "bad_summary",
    ] {
        let f = Fixture::new(mode);
        assert!(f.prepare().await.is_err(), "{mode}");
        assert!(!f.spec().grok.unwrap().prepared, "{mode}");
        assert!(
            crate::agent_store::load_agent_config(&f.repo, &f.label)
                .unwrap()
                .is_none()
        );
    }
}
#[tokio::test]
async fn unconfirmed_child_is_never_overwritten_or_reused() {
    let f = Fixture::new("good");
    f.summary(&f.repo, CHILD, Some(SOURCE));
    let e = f.prepare().await.err().unwrap().to_string();
    assert!(e.contains("unconfirmed"), "{e}");
    assert!(!f.provider.root.join("audit.jsonl").exists());
}
#[tokio::test]
async fn prepared_child_mismatch_or_missing_copy_is_rejected() {
    let f = Fixture::new("good");
    f.prepare().await.unwrap();
    f.summary(&f.repo, CHILD, None);
    assert!(f.prepare().await.is_err());
    std::fs::remove_dir_all(session_dir(&f.provider.root, &f.repo, CHILD)).unwrap();
    assert!(f.prepare().await.is_err());
    assert!(fork_spec_path(&f.repo, &f.label).exists());
}
#[tokio::test]
async fn concurrent_preparation_fails_without_starting_helper() {
    let f = Fixture::new("good");
    let _lock = lock(&f.repo, &f.label).unwrap();
    assert!(f.prepare().await.is_err());
    assert!(!f.provider.root.join("audit.jsonl").exists());
}
#[tokio::test]
async fn helper_drains_stderr_and_is_reaped() {
    let f = Fixture::new("stderr");
    f.prepare().await.unwrap();
    let pid = f.audit()[0]["pid"].as_i64().unwrap() as i32;
    assert_eq!(
        crate::signal::probe_process(pid),
        crate::signal::Probe::Gone
    );
}
#[tokio::test]
async fn deadline_and_output_cap_end_helper() {
    for mode in ["hang", "flood"] {
        let f = Fixture::new(mode);
        let i = f.spec().grok.unwrap();
        let now = std::time::Instant::now();
        assert!(
            fork_rpc(&f.provider, &f.repo, SOURCE, &i, Duration::from_secs(3))
                .await
                .is_err()
        );
        assert!(now.elapsed() < Duration::from_secs(5));
        let pid = f.audit()[0]["pid"].as_i64().unwrap() as i32;
        assert_eq!(
            crate::signal::probe_process(pid),
            crate::signal::Probe::Gone
        );
        assert!(!f.spec().grok.unwrap().prepared);
    }
}
#[test]
fn resume_requires_session_stored_in_target_cwd() {
    let f = Fixture::new("good");
    assert!(validate_resume(&f.provider, &f.repo, SOURCE).is_err());
    validate_resume(&f.provider, &f.source, SOURCE).unwrap();
}
#[test]
fn profile_cannot_override_identity_or_cwd() {
    for arg in [
        "--resume",
        "--session-id=other",
        "--cwd=/other",
        "--fork-session",
        "--worktree",
    ] {
        let l = LaunchConfig {
            command: None,
            args: vec![arg.into()],
            env: BTreeMap::new(),
        };
        assert!(validate_profile(Some(&l)).is_err());
    }
}

#[test]
fn fork_seeding_defers_grok_store_lookup_to_configured_preparation_owner() {
    let empty = tempfile::tempdir().unwrap();
    let (id, kind) = super::super::fork::classify_binding(
        Tool::Grok,
        Some(SOURCE.into()),
        Some(empty.path()),
        false,
    );
    assert_eq!(id.as_deref(), Some(SOURCE));
    assert_eq!(kind, super::super::fork::SeedSource::Resume);
    let (id, _) = super::super::fork::classify_binding(
        Tool::Grok,
        Some(SOURCE.into()),
        Some(empty.path()),
        true,
    );
    assert!(id.is_none());
    let launch = LaunchConfig {
        command: None,
        args: Vec::new(),
        env: BTreeMap::from([("GROK_HOME".into(), "/configured-grok-store".into())]),
    };
    assert_eq!(
        Provider::resolve(Some(&launch), Path::new("/repo"))
            .unwrap()
            .root,
        Path::new("/configured-grok-store")
    );
}

#[tokio::test]
#[ignore = "requires CLANK_GROK_TEST_BIN and a credential-free test process"]
async fn native_offline_fork_uses_production_preparation_owner() {
    let unexpected: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key != "PATH" && key != "CLANK_GROK_TEST_BIN" && key != "__CF_USER_TEXT_ENCODING"
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "run this test in env -i; unexpected environment keys: {unexpected:?}"
    );
    let mut f = Fixture::new("good");
    f.provider.program = std::env::var("CLANK_GROK_TEST_BIN").expect("explicit Grok binary");
    f.provider.args.clear();
    let home = f._temp.path().join("isolated-home");
    std::fs::create_dir_all(&home).unwrap();
    f.provider.env = BTreeMap::from([
        ("HOME".into(), home.to_string_lossy().into_owned()),
        (
            "GROK_HOME".into(),
            f.provider.root.to_string_lossy().into_owned(),
        ),
        ("TERM".into(), "dumb".into()),
    ]);
    let source_file = session_dir(&f.provider.root, &f.source, SOURCE).join("summary.json");
    let summary = json!({"info":{"id":SOURCE,"cwd":f.source},"session_summary":"fabricated empty fixture","created_at":"2026-10-03T00:00:00Z","updated_at":"2026-10-03T00:00:00Z","num_messages":0,"num_chat_messages":0,"current_model_id":"grok-code-fast-1","chat_format_version":1});
    let bytes = serde_json::to_vec(&summary).unwrap();
    std::fs::write(&source_file, &bytes).unwrap();
    let dir = source_file.parent().unwrap();
    std::fs::write(dir.join("chat_history.jsonl"), b"").unwrap();
    std::fs::write(dir.join("updates.jsonl"), b"").unwrap();
    let prepared = f.prepare().await.unwrap();
    assert!(prepared.resume);
    assert_eq!(prepared.id, CHILD);
    assert!(f.spec().grok.unwrap().prepared);
    assert_eq!(std::fs::read(&source_file).unwrap(), bytes);
    assert!(
        crate::agent_store::load_agent_config(&f.source, &f.label)
            .unwrap()
            .is_none()
    );
    assert!(
        crate::agent_store::load_agent_config(&f.repo, &f.label)
            .unwrap()
            .is_none()
    );
}
