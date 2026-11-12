//! Thin Claude question adapter: native evidence in, broker reply out.
use anyhow::{Result, ensure};
use clank_core::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Hook {
    pub cwd: String,
    pub session_id: String,
    pub prompt_id: String,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: Option<String>,
    pub tool_response: Option<Value>,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct Frame {
    pub(crate) proto: u32,
    pub(crate) input: Hook,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct HookReply {
    pub(crate) proto: u32,
    pub(crate) output: Value,
}

/// Convert native evidence to a control message. No question state is read or
/// written here, including when no browser broker is running.
pub(crate) fn evidence(
    hook: &Hook,
) -> Result<Option<(std::path::PathBuf, super::monitor::Control)>> {
    use super::question_state::{Key, Observation, input_hash};
    if hook.tool_name != "AskUserQuestion" {
        return Ok(None);
    }
    let repo = super::resolve_cwd_repo(Path::new(&hook.cwd))?;
    let session = clank_core::SessionId::parse(&hook.session_id)?;
    let author = crate::agent_env::resolve_identity_for_hook_in(&repo, Tool::Claude, &session)?;
    let key = |id: &String| Key {
        session: hook.session_id.clone(),
        prompt: hook.prompt_id.clone(),
        tool_use: id.clone(),
    };
    let observation = match (hook.hook_event_name.as_str(), &hook.tool_use_id) {
        ("PreToolUse", Some(id)) => Observation::Asked {
            key: key(id),
            header: header_of(&hook.tool_input),
            input: input_hash(&hook.tool_input),
        },
        ("PostToolUse" | "PostToolUseFailure", Some(id)) => Observation::Result { key: key(id) },
        _ => return Ok(None),
    };
    Ok(Some((
        repo,
        super::monitor::Control::Questions {
            author: author.to_string(),
            session,
            observation,
        },
    )))
}

/// One line saying what is asked: the first question's header, or else its
/// first line.
fn header_of(input: &Value) -> String {
    let first = input.pointer("/questions/0");
    let pick = |key: &str| {
        first
            .and_then(|q| q.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.lines().next().unwrap_or_default().trim().to_string())
            .filter(|s| !s.is_empty())
    };
    pick("header")
        .or_else(|| pick("question"))
        .unwrap_or_else(|| "a question".into())
}

pub(crate) async fn run() -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if let Ok(hook) = serde_json::from_str::<Hook>(&input) {
        match evidence(&hook) {
            Ok(Some((repo, request))) => {
                if let Err(e) = super::attend::submit(&repo, request).await {
                    eprintln!("clank: question evidence was not recorded: {e:?}");
                }
            }
            Err(e) => eprintln!("clank: question evidence could not be routed: {e:#}"),
            Ok(None) => {}
        }
    }
    let result = async {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let hook: Hook = serde_json::from_str(&input)?;
        let repo = crate::cli::resolve_cwd_repo(Path::new(&hook.cwd))?;
        let path = crate::cli::monitor::runtime_path(&repo, "claude-questions.sock");
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.file_type().is_socket() && meta.uid() == rustix::process::geteuid().as_raw(),
            "untrusted question socket"
        );
        let parked = hook.hook_event_name == "PermissionRequest";
        let frame = Frame {
            proto: 1,
            input: hook,
        };
        let mut socket = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::UnixStream::connect(path),
        )
        .await??;
        let mut bytes = serde_json::to_vec(&frame)?;
        bytes.push(b'\n');
        tokio::time::timeout(std::time::Duration::from_secs(10), socket.write_all(&bytes))
            .await??;
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        if parked {
            reader.read_line(&mut line).await?;
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                reader.read_line(&mut line),
            )
            .await??;
        }
        let reply: HookReply = serde_json::from_str(&line)?;
        ensure!(reply.proto == 1, "unsupported question reply");
        Ok::<_, anyhow::Error>(reply.output)
    }
    .await;
    println!("{}", result.unwrap_or_else(|_| json!({})));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_written_only_when_the_monitor_consumes_it_without_a_viewer() {
        let (repo, author) = super::super::turn_child::tests::repo_with(Tool::Claude, false);
        let mut hook = Hook {
            cwd: repo.path().to_str().unwrap().into(),
            session_id: super::super::turn_child::tests::SESSION.into(),
            prompt_id: "turn".into(),
            hook_event_name: "PreToolUse".into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: json!({"questions":[{"header":"Choose","question":"Choose one"}]}),
            tool_use_id: Some("call".into()),
            tool_response: None,
        };
        let dir = crate::agent_store::agents_root(repo.path()).join(author.as_str());
        let (_, request) = evidence(&hook).unwrap().unwrap();
        assert!(
            !super::super::question_state::path(&dir).exists(),
            "the adapter writes nothing"
        );
        super::super::monitor::question_control(repo.path(), &request).unwrap();
        assert_eq!(
            super::super::question_state::asking(&dir, &hook.session_id)
                .unwrap()
                .header,
            "Choose"
        );
        hook.hook_event_name = "PostToolUse".into();
        let (_, finished) = evidence(&hook).unwrap().unwrap();
        super::super::monitor::question_control(repo.path(), &finished).unwrap();
        assert!(super::super::question_state::asking(&dir, &hook.session_id).is_none());
        let before = std::fs::read(super::super::question_state::path(&dir)).unwrap();
        std::fs::write(super::super::question_state::path(&dir), "not json").unwrap();
        assert!(
            super::super::monitor::question_control(repo.path(), &request)
                .unwrap_err()
                .contains("unreadable")
        );
        std::fs::write(super::super::question_state::path(&dir), &before).unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &author,
            Tool::Claude,
            &clank_core::SessionId::parse("replacement").unwrap(),
        )
        .unwrap();
        assert!(super::super::monitor::question_control(repo.path(), &request).is_err());
        assert_eq!(
            std::fs::read(super::super::question_state::path(&dir)).unwrap(),
            before
        );
    }
}
