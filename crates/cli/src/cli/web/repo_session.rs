//! The repo half of a web session. Only this process reads the repo,
//! harness bindings, generated pages, or agent panes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use clank_web::bridge::{Action, ActionResult, Backend, MAX_CONTROL_BYTES, Snapshot, Update};
use clank_web::request::{BrowserReply, BrowserRequest, Content};
use tokio::sync::{broadcast, watch};

use super::{AgentAsk, Blocking, Feed, Pages, Panes, QueueAsk, Reopener, StashAsk};

pub(crate) struct RepoSession {
    repo: PathBuf,
    home: Option<PathBuf>,
    panes: Arc<dyn Panes>,
    feed: Feed,
    reopen: Option<Reopener>,
    blocking: Arc<Blocking>,
    cancelled: watch::Receiver<bool>,
}

impl RepoSession {
    pub(crate) fn new(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        feed: Feed,
        reopen: Option<Reopener>,
        blocking: Arc<Blocking>,
        cancelled: watch::Receiver<bool>,
    ) -> Self {
        Self {
            repo,
            home,
            panes,
            feed,
            reopen,
            blocking,
            cancelled,
        }
    }

    async fn execute(&self, request: BrowserRequest) -> BrowserReply {
        if *self.cancelled.borrow() {
            return plain(503, "the repo connector is stopping");
        }
        match request {
            BrowserRequest::Say {
                pane,
                label,
                session,
                text,
            } => self.input(pane, label, session, Some(text)).await,
            BrowserRequest::Stop {
                pane,
                label,
                session,
            } => self.input(pane, label, session, None).await,
            BrowserRequest::Queue { ask } => {
                let Ok(ask) = serde_json::from_value::<QueueAsk>(ask) else {
                    return plain(400, "invalid queue action");
                };
                let repo = self.repo.clone();
                answer_optional(
                    self.blocking
                        .run(move || super::perform_queue_action(&repo, ask))
                        .await,
                    "the queue action panicked",
                )
            }
            BrowserRequest::Stash { ask } => {
                let Ok(ask) = serde_json::from_value::<StashAsk>(ask) else {
                    return plain(400, "invalid stash action");
                };
                let repo = self.repo.clone();
                let runtime = tokio::runtime::Handle::current();
                answer_optional(
                    self.blocking
                        .run(move || super::perform_stash_action(&repo, ask, &runtime))
                        .await,
                    "the stash action panicked",
                )
            }
            BrowserRequest::Agent { ask } => {
                let Ok(ask) = serde_json::from_value::<AgentAsk>(ask) else {
                    return plain(400, "invalid agent action");
                };
                let repo = self.repo.clone();
                let home = self.home.clone();
                answer_unit(
                    self.blocking
                        .run(move || super::perform_agent_action(&repo, home.as_deref(), ask))
                        .await,
                    "the agent action panicked",
                )
            }
            BrowserRequest::AgentCandidates => {
                let home = self.home.clone();
                match self
                    .blocking
                    .run(move || super::library_candidates(home.as_deref()))
                    .await
                {
                    Some(Ok(value)) => json(200, value.to_string()),
                    Some(Err(error)) => conflict(error),
                    None => plain(500, "the agent library read panicked"),
                }
            }
            BrowserRequest::BlockAnswer {
                agent,
                name,
                question,
                answer,
            } => {
                let repo = self.repo.clone();
                answer_unit(
                    self.blocking
                        .run(move || {
                            crate::cli::block::answer_block(
                                &repo,
                                &agent,
                                &name,
                                &answer,
                                Some(&question),
                            )
                            .map(|_| ())
                        })
                        .await,
                    "the block answer panicked",
                )
            }
            BrowserRequest::Reopen { label } => self.reopen(label).await,
            BrowserRequest::Site { path } => {
                let pages = Pages {
                    repo: self.repo.clone(),
                    site: super::site_dir(&self.repo),
                };
                match self
                    .blocking
                    .run(move || super::site_content(&pages, &path))
                    .await
                {
                    Some((status, body, mime)) => {
                        let content = match mime {
                            "text/html; charset=utf-8" => Content::Html,
                            "text/css; charset=utf-8" => Content::Css,
                            _ => Content::Plain,
                        };
                        BrowserReply {
                            status,
                            content,
                            body,
                        }
                    }
                    None => plain(500, "the page render panicked"),
                }
            }
        }
    }

    async fn input(
        &self,
        pane: String,
        label: String,
        expected_session: Option<String>,
        text: Option<String>,
    ) -> BrowserReply {
        let repo = self.repo.clone();
        let panes = self.panes.clone();
        let result = self
            .blocking
            .run(move || {
                if let Err(error) =
                    check_target(&repo, &*panes, &pane, &label, expected_session.as_deref())
                {
                    return Err((409, format!("{error:#}")));
                }
                let result = match text {
                    Some(text) => panes.say(&pane, &text),
                    None => panes.stop(&pane),
                };
                result.map_err(|error| (502, format!("{error:#}")))
            })
            .await;
        match result {
            Some(Ok(())) => plain(204, ""),
            Some(Err((409, why))) => json(409, serde_json::json!({"why":why}).to_string()),
            Some(Err((status, why))) => plain(status, why),
            None => plain(500, "the pane action panicked"),
        }
    }

    async fn reopen(&self, label: String) -> BrowserReply {
        let Some(reopen) = self.reopen.as_ref() else {
            return json(
                503,
                serde_json::json!({"why":"the TUI pane reconciler is unavailable"}).to_string(),
            );
        };
        let parsed = match clank_core::ids::AgentLabel::parse(&label) {
            Ok(label) => label,
            Err(error) => {
                return json(
                    409,
                    serde_json::json!({"why":format!("invalid agent label: {error}")}).to_string(),
                );
            }
        };
        let repo = self.repo.clone();
        let present = self
            .blocking
            .run(move || -> anyhow::Result<bool> {
                Ok(crate::agent_store::load_repo_config_required(&repo)?
                    .agents
                    .contains_key(&parsed))
            })
            .await;
        match present {
            Some(Ok(true)) => {}
            Some(Ok(false)) => {
                return json(
                    409,
                    serde_json::json!({"why":format!("`{label}` is no longer on the roster")})
                        .to_string(),
                );
            }
            Some(Err(error)) => return conflict(error),
            None => return plain(500, "the roster read panicked"),
        }
        if self.feed.has_live_pane(&label) {
            return json(
                409,
                serde_json::json!({"why":format!("`{label}` already has a live pane")}).to_string(),
            );
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), reopen(label)).await {
            Ok(Ok(Ok(message))) => json(200, serde_json::json!({"message":message}).to_string()),
            Ok(Ok(Err(why))) => json(409, serde_json::json!({"why":why}).to_string()),
            Ok(Err(_)) => json(
                503,
                serde_json::json!({"why":"the TUI pane reconciler stopped"}).to_string(),
            ),
            Err(_) => json(
                504,
                serde_json::json!({"why":"the TUI pane reconciler did not answer"}).to_string(),
            ),
        }
    }
}

#[async_trait]
impl Backend for RepoSession {
    async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
        self.feed.bridge_connect()
    }

    async fn dispatch(&self, action: Action) -> ActionResult {
        let request = match BrowserRequest::from_action(action) {
            Ok(request) => request,
            Err(error) => {
                return ActionResult::Refused {
                    why: format!("{error:#}"),
                };
            }
        };
        let mut reply = self.execute(request).await;
        let mut value = reply.clone().into_value();
        if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() >= MAX_CONTROL_BYTES / 2) {
            reply = plain(413, "repo response is too large for the control channel");
            value = reply.into_value();
        }
        ActionResult::Completed { value }
    }

    async fn upload(&self, media_type: String, bytes: Vec<u8>) -> ActionResult {
        if *self.cancelled.borrow() {
            return ActionResult::Refused {
                why: "the repo connector is stopping".into(),
            };
        }
        let repo = self.repo.clone();
        let result = self
            .blocking
            .run(move || store_attachment(&repo, &media_type, &bytes))
            .await;
        let reply = match result {
            Some(Ok(path)) => json(200, serde_json::json!({"path":path}).to_string()),
            Some(Err(error)) => plain(500, format!("cannot keep the attachment: {error:#}")),
            None => plain(500, "the attachment write panicked"),
        };
        ActionResult::Completed {
            value: reply.into_value(),
        }
    }
}

fn store_attachment(repo: &Path, media_type: &str, bytes: &[u8]) -> anyhow::Result<String> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= clank_web::bridge::MAX_UPLOAD_BYTES,
        "invalid attachment size"
    );
    let dir = repo.join(".clank/attachments");
    std::fs::create_dir_all(&dir)?;
    let ext = super::extension_for(media_type);
    for _ in 0..64 {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("random attachment name unavailable: {error}"))?;
        let name = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = dir.join(format!("{name}.{ext}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                if let Err(error) = file.write_all(bytes) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error.into());
                }
                super::prune_attachments(&dir);
                return Ok(path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("the attachment path is not text"))?
                    .to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("cannot find a free attachment name")
}

fn check_target(
    repo: &Path,
    panes: &dyn Panes,
    pane: &str,
    label: &str,
    expected_session: Option<&str>,
) -> anyhow::Result<()> {
    let table = panes
        .table()
        .ok_or_else(|| anyhow::anyhow!("zellij did not answer"))?;
    anyhow::ensure!(
        table
            .iter()
            .any(|row| row.id == pane && row.label == label && !row.exited),
        "the pane for `{label}` changed before the action arrived"
    );
    let label = clank_core::ids::AgentLabel::parse(label)?;
    crate::agent_store::resolve_tool(repo, &label)?;
    let current = crate::agent_store::load_agent_config(repo, &label)?
        .and_then(|cfg| cfg.session)
        .map(|session| session.id.to_string());
    anyhow::ensure!(
        current.as_deref() == expected_session,
        "the agent session changed before the action arrived"
    );
    Ok(())
}

fn plain(status: u16, body: impl Into<String>) -> BrowserReply {
    BrowserReply {
        status,
        content: Content::Plain,
        body: body.into(),
    }
}

fn json(status: u16, body: String) -> BrowserReply {
    BrowserReply {
        status,
        content: Content::Json,
        body,
    }
}

fn conflict(error: anyhow::Error) -> BrowserReply {
    json(
        409,
        serde_json::json!({"why":format!("{error:#}")}).to_string(),
    )
}

fn answer_unit(result: Option<anyhow::Result<()>>, panic: &str) -> BrowserReply {
    match result {
        Some(Ok(())) => plain(204, ""),
        Some(Err(error)) => conflict(error),
        None => plain(500, panic),
    }
}

fn answer_optional(
    result: Option<anyhow::Result<Option<serde_json::Value>>>,
    panic: &str,
) -> BrowserReply {
    match result {
        Some(Ok(None)) => plain(204, ""),
        Some(Ok(Some(value))) => json(200, value.to_string()),
        Some(Err(error)) => conflict(error),
        None => plain(500, panic),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use clank_core::agent_config::{AgentConfig, Session};
    use clank_core::ids::{AgentLabel, SessionId};
    use clank_core::vocab::Tool;
    use clank_web::bridge::{ActionResult, Backend};
    use clank_web::request::{BrowserReply, BrowserRequest};

    use super::{Blocking, Feed, Panes, RepoSession};
    use crate::cli::open_zellij::{PaneMeta, SubscribeChild};

    struct FakePanes {
        table: Mutex<Vec<PaneMeta>>,
        sent: Mutex<Vec<String>>,
    }

    impl Panes for FakePanes {
        fn session(&self) -> &str {
            "fixture"
        }
        fn table(&self) -> Option<Vec<PaneMeta>> {
            Some(self.table.lock().unwrap().clone())
        }
        fn subscribe(&self, _: &[String]) -> Option<SubscribeChild> {
            None
        }
        fn say(&self, _: &str, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.into());
            Ok(())
        }
        fn stop(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn reply(result: ActionResult) -> BrowserReply {
        match result {
            ActionResult::Completed { value } => BrowserReply::from_value(value).unwrap(),
            ActionResult::Refused { why } => panic!("unexpected refusal: {why}"),
        }
    }

    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        repo
    }

    fn backend(repo: &Path, home: Option<&Path>) -> RepoSession {
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        RepoSession::new(
            repo.to_path_buf(),
            home.map(Path::to_path_buf),
            Arc::new(FakePanes {
                table: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            }),
            Feed::new(),
            None,
            Arc::new(Blocking::default()),
            cancelled,
        )
    }

    async fn ask(backend: &RepoSession, request: BrowserRequest) -> BrowserReply {
        reply(backend.dispatch(request.into_action()).await)
    }

    #[tokio::test]
    async fn queue_edits_use_the_named_handle_and_refuse_stale_revisions() {
        let repo = git_repo();
        let backend = backend(repo.path(), None);
        let queue = |ask| BrowserRequest::Queue { ask };
        let added = ask(&backend, queue(serde_json::json!({"do":"add","name":"later","priority":500,"body":"First draft\n"}))).await;
        assert_eq!(added.status, 200);
        let added: serde_json::Value = serde_json::from_str(&added.body).unwrap();
        assert_eq!(added["handle"], "500-later");
        assert_eq!(added["page"], "queue/500-later.html");
        let opened = ask(
            &backend,
            queue(serde_json::json!({"do":"open","handle":"500-later"})),
        )
        .await;
        assert_eq!(opened.status, 200);
        let opened: serde_json::Value = serde_json::from_str(&opened.body).unwrap();
        let original = opened["body"].as_str().unwrap();
        let revision = opened["revision"].as_str().unwrap();
        assert_eq!(
            ask(&backend, queue(serde_json::json!({"do":"save","handle":"500-later","original":original,"revision":revision,"body":"# later\nRevised\n"}))).await.status,
            204
        );
        let stale = ask(
            &backend,
            queue(serde_json::json!({"do":"save","handle":"500-later","original":original,"revision":revision,"body":"# later\nOld tab\n"})),
        )
        .await;
        assert_eq!(stale.status, 409);
        assert!(stale.body.contains("changed since it was opened"));
        let moved = ask(
            &backend,
            queue(serde_json::json!({"do":"priority","handle":"500-later","priority":100})),
        )
        .await;
        assert_eq!(moved.status, 200);
        let moved: serde_json::Value = serde_json::from_str(&moved.body).unwrap();
        assert_eq!(moved["handle"], "100-later");
        assert_eq!(moved["page"], "queue/100-later.html");
        assert!(repo.path().join(".clank/queue/100-later.md").exists());
        assert_eq!(
            ask(
                &backend,
                queue(serde_json::json!({"do":"unqueue","handle":"500-later"}))
            )
            .await
            .status,
            409,
            "moving a handle must not select another file"
        );
        assert_eq!(
            ask(
                &backend,
                queue(serde_json::json!({"do":"unqueue","handle":"100-later"}))
            )
            .await
            .status,
            204
        );
        assert!(repo.path().join(".clank/drafts/later.md").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stash_restore_requires_the_opened_revision_and_restores_locally() {
        let repo = git_repo();
        let path = repo.path();
        let git = |args: &[&str]| -> String {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        std::fs::create_dir_all(path.join(".clank/plans")).unwrap();
        std::fs::write(
            path.join(".clank/.gitignore"),
            "/stash/\n/shelved/\n/cache/\n",
        )
        .unwrap();
        std::fs::write(
            path.join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        std::fs::write(path.join("base.rs"), "// base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "base"]);
        std::fs::write(
            path.join(".clank/plans/foo.md"),
            "# foo\n\nthe stashed plan\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "-m", "[foo] intro"]);
        let sha = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/clank/stash/foo", &sha]);
        git(&["reset", "--hard", "HEAD^"]);
        let record_path = path.join(".clank/stash/foo.json");
        std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
        let record = crate::cli::stash::StashRecord {
            shas: vec![sha],
            git_ref: "refs/clank/stash/foo".into(),
            waiting_for: None,
        };
        std::fs::write(&record_path, serde_json::to_string(&record).unwrap()).unwrap();
        let backend = backend(path, None);
        let stash = |ask| BrowserRequest::Stash { ask };
        let opened = ask(
            &backend,
            stash(serde_json::json!({"do":"open","stem":"foo"})),
        )
        .await;
        assert_eq!(opened.status, 200);
        let opened: serde_json::Value = serde_json::from_str(&opened.body).unwrap();
        assert_eq!(opened["body"], "# foo\n\nthe stashed plan\n");
        let revision = opened["revision"].as_str().unwrap();
        std::fs::write(
            &record_path,
            serde_json::to_string(&crate::cli::stash::StashRecord {
                waiting_for: Some("later".into()),
                ..record
            })
            .unwrap(),
        )
        .unwrap();
        let stale = ask(
            &backend,
            stash(serde_json::json!({"do":"restore","stem":"foo","revision":revision})),
        )
        .await;
        assert_eq!(stale.status, 409);
        assert!(stale.body.contains("changed since it was opened"));
        let fresh = ask(
            &backend,
            stash(serde_json::json!({"do":"open","stem":"foo"})),
        )
        .await;
        let fresh: serde_json::Value = serde_json::from_str(&fresh.body).unwrap();
        let restored = ask(
            &backend,
            stash(serde_json::json!({"do":"restore","stem":"foo","revision":fresh["revision"]})),
        )
        .await;
        assert_eq!(restored.status, 204, "{}", restored.body);
        assert_eq!(
            std::fs::read_to_string(path.join(".clank/plans/foo.md")).unwrap(),
            "# foo\n\nthe stashed plan\n"
        );
        assert_eq!(
            ask(
                &backend,
                stash(
                    serde_json::json!({"do":"restore","stem":"foo","revision":fresh["revision"]})
                )
            )
            .await
            .status,
            409
        );
    }

    #[tokio::test]
    async fn block_answers_require_the_current_question_and_are_single_use() {
        let repo = git_repo();
        let question = repo.path().join(".clank/agents/claude/blocks/why.md");
        std::fs::create_dir_all(question.parent().unwrap()).unwrap();
        std::fs::write(&question, "Which option?").unwrap();
        let backend = backend(repo.path(), None);
        let answer = |question: &str, answer: &str| BrowserRequest::BlockAnswer {
            agent: "claude".into(),
            name: "why".into(),
            question: question.into(),
            answer: answer.into(),
        };
        assert_eq!(
            ask(&backend, answer("Which option?", " ")).await.status,
            409
        );
        assert_eq!(
            ask(&backend, answer("Old question", "yes")).await.status,
            409
        );
        assert_eq!(
            ask(&backend, answer("Which option?", "yes")).await.status,
            204
        );
        assert_eq!(
            ask(&backend, answer("Which option?", "no")).await.status,
            409
        );
        assert_eq!(
            crate::cli::block::scan_blocks(repo.path())[0]
                .answer
                .as_deref(),
            Some("yes")
        );
    }

    #[tokio::test]
    async fn roster_actions_keep_the_agents_existing_binding() {
        let repo = git_repo();
        let home = tempfile::tempdir().unwrap();
        let backend = backend(repo.path(), Some(home.path()));
        let agent = |ask| BrowserRequest::Agent { ask };
        for (label, tool) in [("codex", "codex"), ("grok", "grok")] {
            assert_eq!(
                ask(
                    &backend,
                    agent(
                        serde_json::json!({"do":"add","label":label,"tool":tool,"review":"commit"})
                    )
                )
                .await
                .status,
                204
            );
        }
        let codex = AgentLabel::parse("codex").unwrap();
        let config = AgentConfig {
            session: Some(Session {
                id: SessionId::parse("bound-session").unwrap(),
                tool: Tool::Codex,
                updated_at: "2026-09-26T00:00:00Z".into(),
                transcript: None,
            }),
            ..Default::default()
        };
        crate::agent_store::save_agent_config(repo.path(), &codex, &config).unwrap();
        assert_eq!(
            ask(
                &backend,
                agent(serde_json::json!({"do":"promote","label":"codex"}))
            )
            .await
            .status,
            204
        );
        assert_eq!(
            ask(
                &backend,
                agent(serde_json::json!({"do":"review","label":"grok","review":"gate"}))
            )
            .await
            .status,
            204
        );
        let roster = crate::agent_store::load_repo_config_required(repo.path()).unwrap();
        assert_eq!(
            roster.agents[&codex].role,
            crate::cli::teams_config::RosterRole::Master
        );
        assert_eq!(
            roster.agents[&AgentLabel::parse("grok").unwrap()].role,
            crate::cli::teams_config::RosterRole::Gate
        );
        assert_eq!(
            crate::agent_store::load_agent_config(repo.path(), &codex)
                .unwrap()
                .unwrap()
                .session
                .unwrap()
                .id,
            SessionId::parse("bound-session").unwrap()
        );
        assert_eq!(
            ask(
                &backend,
                agent(serde_json::json!({"do":"remove","label":"codex"}))
            )
            .await
            .status,
            409
        );
        assert_eq!(
            ask(
                &backend,
                BrowserRequest::Reopen {
                    label: "grok".into()
                }
            )
            .await
            .status,
            503,
            "an unavailable TUI reconciler must be explicit"
        );
    }

    #[tokio::test]
    async fn input_rechecks_the_pane_and_exact_binding_before_delivery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(
            dir.path().join(".clank/config.json"),
            r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
        )
        .unwrap();
        let panes = Arc::new(FakePanes {
            table: Mutex::new(vec![PaneMeta {
                id: "terminal_1".into(),
                label: "grok".into(),
                columns: 80,
                rows: 24,
                exited: false,
            }]),
            sent: Mutex::new(Vec::new()),
        });
        let feed = Feed::new();
        let (_cancel, cancelled) = tokio::sync::watch::channel(false);
        let backend = RepoSession::new(
            dir.path().to_path_buf(),
            None,
            panes.clone(),
            feed,
            None,
            Arc::new(Blocking::default()),
            cancelled,
        );
        let ask = |session| {
            BrowserRequest::Say {
                pane: "terminal_1".into(),
                label: "grok".into(),
                session,
                text: "hello".into(),
            }
            .into_action()
        };
        assert_eq!(reply(backend.dispatch(ask(None)).await).status, 204);

        let label = AgentLabel::parse("grok").unwrap();
        let config = AgentConfig {
            session: Some(Session {
                id: SessionId::parse("session_1").unwrap(),
                tool: Tool::Grok,
                updated_at: "2026-01-01T00:00:00Z".into(),
                transcript: None,
            }),
            ..Default::default()
        };
        crate::agent_store::save_agent_config(dir.path(), &label, &config).unwrap();
        assert_eq!(reply(backend.dispatch(ask(None)).await).status, 409);
        assert_eq!(
            reply(backend.dispatch(ask(Some("session_1".into()))).await).status,
            204
        );

        panes.table.lock().unwrap()[0].id = "terminal_2".into();
        assert_eq!(
            reply(backend.dispatch(ask(Some("session_1".into()))).await).status,
            409
        );
        assert_eq!(panes.sent.lock().unwrap().as_slice(), &["hello", "hello"]);

        std::fs::create_dir_all(dir.path().join(".clank/html")).unwrap();
        std::fs::write(dir.path().join(".clank/html/index.html"), "<h1>local</h1>").unwrap();
        let site = reply(
            backend
                .dispatch(
                    BrowserRequest::Site {
                        path: "index.html".into(),
                    }
                    .into_action(),
                )
                .await,
        );
        assert_eq!(site.status, 200);
        assert_eq!(site.content, clank_web::request::Content::Html);
        assert_eq!(site.body, "<h1>local</h1>");

        let uploaded = reply(
            backend
                .upload("image/png".into(), b"local bytes".to_vec())
                .await,
        );
        assert_eq!(uploaded.status, 200);
        let path = serde_json::from_str::<serde_json::Value>(&uploaded.body).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(path.starts_with(dir.path().join(".clank/attachments").to_str().unwrap()));
        assert!(path.ends_with(".png"));
        assert_eq!(std::fs::read(path).unwrap(), b"local bytes");
    }
}
