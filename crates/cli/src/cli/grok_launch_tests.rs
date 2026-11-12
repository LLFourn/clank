use super::super::fork::{ForkSpec, fork_spec_path};
use super::*;
use clank_core::ids::SessionId;
use clank_core::vocab::Tool;

const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    repo: PathBuf,
    provider: Provider,
    label: AgentLabel,
    mode: String,
    copies: std::cell::Cell<usize>,
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
        let provider = Provider {
            program: "/must-not-spawn/in-state-tests".into(),
            args: vec![],
            env: BTreeMap::new(),
            root,
        };
        let label = AgentLabel::parse("grok").unwrap();
        let f = Self {
            _temp: temp,
            source,
            repo,
            provider,
            label,
            mode: mode.into(),
            copies: std::cell::Cell::new(0),
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
    async fn prepare(&self) -> anyhow::Result<Prepared> {
        prepare_with(
            &self.repo,
            &self.label,
            &self.provider,
            async |source: &str, intent: &ForkIntent| {
                self.copies.set(self.copies.get() + 1);
                assert_eq!(source, SOURCE);
                assert_eq!(intent.source_cwd, self.source);
                if self.mode == "transport_error" {
                    anyhow::bail!("provider copy failed");
                }
                let cwd = if self.mode == "bad_summary" {
                    &self.source
                } else {
                    &self.repo
                };
                let dir = session_dir(&self.provider.root, &self.repo, &intent.child_id);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("summary.json"),
                    serde_json::to_vec(&json!({
                        "info":{"id":intent.child_id,"cwd":cwd},"parent_session_id":source
                    }))
                    .unwrap(),
                )
                .unwrap();
                Ok(ForkResponse {
                    new_session_id: intent.child_id.clone(),
                    new_cwd: if self.mode == "bad_response" {
                        self.source.clone()
                    } else {
                        self.repo.clone()
                    },
                    parent_session_id: source.into(),
                })
            },
        )
        .await
    }
}

#[tokio::test]
async fn child_copy_preserves_source_and_consumes_only_on_matching_bind() {
    let f = Fixture::new("good");
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
    assert_eq!(f.copies.get(), 1);
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
    let count = f.copies.get();
    assert_eq!(f.prepare().await.unwrap().id, CHILD);
    assert_eq!(f.copies.get(), count);
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
    assert_eq!(f.copies.get(), 0);
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
    assert_eq!(f.copies.get(), 0);
}
#[tokio::test]
async fn rpc_failures_retain_unprepared_intent() {
    for mode in ["transport_error", "bad_response", "bad_summary"] {
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
    assert_eq!(f.copies.get(), 0);
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
    assert_eq!(f.copies.get(), 0);
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
    let prepared = prepare(&f.repo, &f.label, &f.provider).await.unwrap();
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
