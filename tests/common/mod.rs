//! Test harness: in-process Trinity daemon on an ephemeral port, with
//! restart support so recovery tests can validate startup from the same DB.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use trinity::daemon::{self, AppState, DaemonShutdown};

pub struct TestApp {
    pub base: String,
    pub client: reqwest::Client,
    pub state: AppState,
    pub tmp: TempDir,
    pub repo: PathBuf,
    pub db_path: PathBuf,
    server: JoinHandle<()>,
    shutdown: Option<DaemonShutdown>,
}

impl TestApp {
    pub async fn spawn() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("trinity.sqlite");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        Self::spawn_with(tmp, db, repo).await
    }

    async fn spawn_with(tmp: TempDir, db: PathBuf, repo: PathBuf) -> Self {
        let (state, shutdown) = daemon::build_state(&db).await.expect("build_state");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = daemon::http::router(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        TestApp {
            base: format!("http://127.0.0.1:{port}"),
            client,
            state,
            tmp,
            repo,
            db_path: db,
            server,
            shutdown: Some(shutdown),
        }
    }

    /// Tear down and rebuild the daemon against the same DB / tempdir. Used
    /// by restart-recovery tests.
    pub async fn restart(mut self) -> Self {
        if let Some(s) = self.shutdown.take() {
            s.stop().await;
        }
        self.server.abort();
        // Give axum a moment to release the port.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let tmp = self.tmp;
        let db = self.db_path;
        let repo = self.repo;
        Self::spawn_with(tmp, db, repo).await
    }

    pub async fn call(
        &self,
        tool: &str,
        cwd: &Path,
        label: Option<&str>,
        mut arguments: Value,
    ) -> ToolCallOutcome {
        // Mirror the shim's per-tool argument-rewrite: if the caller supplied
        // a `label` here (the optional helper arg, not the deleted envelope
        // field) and the tool takes one, merge it into arguments unless
        // already present.
        if let (Some(label), Some(key)) = (label, label_arg_for(tool))
            && let Value::Object(ref mut map) = arguments
            && !map.contains_key(key)
        {
            map.insert(key.to_string(), json!(label));
        }
        let body = json!({
            "cwd": cwd,
            "tool": tool,
            "arguments": arguments,
        });
        let resp = self
            .client
            .post(format!("{}/internal/tool_call", self.base))
            .header("origin", "http://127.0.0.1")
            .json(&body)
            .send()
            .await
            .expect("tool_call send");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            #[derive(Deserialize)]
            struct Envelope {
                result: Value,
            }
            let env: Envelope = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("decode envelope ({status}): {e}: {text}"));
            ToolCallOutcome::Ok(env.result)
        } else {
            ToolCallOutcome::Err(status, text)
        }
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .expect("get send")
    }

    pub async fn post_form(&self, path: &str, body: &str) -> reqwest::Response {
        self.client
            .post(format!("{}{}", self.base, path))
            .header("origin", "http://127.0.0.1")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.to_string())
            .send()
            .await
            .expect("post send")
    }
}

#[allow(clippy::large_enum_variant)]
pub enum ToolCallOutcome {
    Ok(Value),
    Err(reqwest::StatusCode, String),
}

impl ToolCallOutcome {
    pub fn unwrap(self) -> Value {
        match self {
            ToolCallOutcome::Ok(v) => v,
            ToolCallOutcome::Err(s, t) => panic!("expected ok, got {s}: {t}"),
        }
    }
    pub fn expect_err(self) -> (reqwest::StatusCode, String) {
        match self {
            ToolCallOutcome::Ok(v) => panic!("expected error, got ok: {v}"),
            ToolCallOutcome::Err(s, t) => (s, t),
        }
    }
}

fn init_repo(repo: &Path) {
    run_git(repo, &["init", "-q"]);
    run_git(repo, &["config", "user.email", "test@trinity"]);
    run_git(repo, &["config", "user.name", "trinity-test"]);
    std::fs::write(repo.join(".gitkeep"), b"").unwrap();
    run_git(repo, &["add", ".gitkeep"]);
    run_git(repo, &["commit", "-q", "-m", "init"]);
}

pub fn run_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    if !output.status.success() {
        panic!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Mirror the production shim's per-tool label-arg mapping. Keeps
/// `TestApp::call(..., Some(label), ...)` working as a shorthand for tests
/// that don't want to repeat `label` / `author_label` inside `arguments`.
fn label_arg_for(tool: &str) -> Option<&'static str> {
    match tool {
        "put_feedback" => Some("author_label"),
        "register_plan_file"
        | "register_implementation_commit"
        | "get_current_feedback"
        | "get_review_context" => Some("label"),
        _ => None,
    }
}

pub fn make_commit(repo: &Path, file: &str, contents: &str) -> String {
    std::fs::write(repo.join(file), contents).unwrap();
    run_git(repo, &["add", file]);
    run_git(repo, &["commit", "-q", "-m", &format!("add {file}")]);
    run_git(repo, &["rev-parse", "HEAD"])
}
