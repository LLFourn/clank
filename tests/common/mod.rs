//! Shared test harness: in-process Trinity daemon on an ephemeral port,
//! a configured `reqwest::Client`, and helpers for the same wire protocol
//! the stdio shim uses.

// Several helpers here are used by some test files but not all; cargo's
// per-binary dead-code analysis is overzealous against `tests/common/mod.rs`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use trinity::daemon::{self, AppState};

pub struct TestApp {
    pub base: String,
    pub client: reqwest::Client,
    pub state: AppState,
    pub tmp: TempDir,
    pub repo: PathBuf,
    _server: JoinHandle<()>,
}

impl TestApp {
    pub async fn spawn() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("trinity.sqlite");
        let state = daemon::build_state(&db).await.expect("build_state");
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

        // Initialize a throwaway git repo inside the tempdir so tools can derive repo_root.
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["config", "user.email", "test@trinity"]);
        run_git(&repo, &["config", "user.name", "trinity-test"]);
        // An initial commit so root-commit edge cases don't bite later.
        std::fs::write(repo.join(".gitkeep"), b"").unwrap();
        run_git(&repo, &["add", ".gitkeep"]);
        run_git(&repo, &["commit", "-q", "-m", "init"]);

        TestApp {
            base: format!("http://127.0.0.1:{port}"),
            client,
            state,
            tmp,
            repo,
            _server: server,
        }
    }

    /// Issue a tool call on the daemon's internal HTTP API the same way the
    /// stdio shim does.
    pub async fn call(
        &self,
        tool: &str,
        cwd: &Path,
        label: Option<&str>,
        arguments: Value,
    ) -> ToolCallOutcome {
        let mut body = json!({
            "cwd": cwd,
            "tool": tool,
            "arguments": arguments,
        });
        if let Some(label) = label {
            body.as_object_mut()
                .unwrap()
                .insert("label".into(), json!(label));
        }
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

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    if !output.status.success() {
        panic!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
