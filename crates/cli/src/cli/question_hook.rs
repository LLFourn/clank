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
///
/// Only an `AskUserQuestion` PreToolUse, PostToolUse or PostToolUseFailure
/// with a `tool_use_id` is evidence; anything else is `None` before the repo
/// or the binding is looked up, so it can fail no routing.
pub(crate) fn evidence(
    hook: &Hook,
) -> Result<Option<(std::path::PathBuf, super::monitor::Control)>> {
    use super::question_state::{Key, Observation, input_hash};
    if hook.tool_name != "AskUserQuestion" {
        return Ok(None);
    }
    let (asked, id) = match (hook.hook_event_name.as_str(), &hook.tool_use_id) {
        ("PreToolUse", Some(id)) => (true, id),
        ("PostToolUse" | "PostToolUseFailure", Some(id)) => (false, id),
        _ => return Ok(None),
    };
    let key = Key {
        session: hook.session_id.clone(),
        prompt: hook.prompt_id.clone(),
        tool_use: id.clone(),
    };
    let observation = match asked {
        true => Observation::Asked {
            key,
            header: header_of(&hook.tool_input),
            input: input_hash(&hook.tool_input),
        },
        false => Observation::Result { key },
    };
    let repo = super::resolve_cwd_repo(Path::new(&hook.cwd))?;
    let session = clank_core::SessionId::parse(&hook.session_id)?;
    let author = crate::agent_env::resolve_identity_for_hook_in(&repo, Tool::Claude, &session)?;
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

/// One observation on its way to the monitor: what `question-hook
/// --deliver` reads on its stdin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Delivery {
    pub(crate) repo: std::path::PathBuf,
    pub(crate) request: super::monitor::Control,
}

/// Starts carrying a [`Delivery`] to the monitor and returns without
/// waiting for it to arrive.
pub(crate) trait HandOff {
    fn hand_off(&self, delivery: &Delivery) -> Result<()>;
}

/// This binary as `question-hook --deliver`, in a session of its own with
/// no stdout or stderr of the hook's, left running when the hook exits.
struct Detached;

impl HandOff for Detached {
    /// The delivery is written into the child's stdin pipe before the child
    /// exists and the write end closed, so nothing here waits on the child.
    fn hand_off(&self, delivery: &Delivery) -> Result<()> {
        use crate::proc_run::{Group, Spec, Stdin, preloaded, start};
        let stdin = preloaded(&serde_json::to_vec(delivery)?)?;
        start(
            Spec::new(std::env::current_exe()?)
                .args(["question-hook", "--deliver"])
                .stdin(Stdin::Fd(stdin))
                .group(Group::Session),
        )?
        .detach();
        Ok(())
    }
}

pub(crate) async fn run(args: super::QuestionHookArgs) -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if args.deliver {
        let me = rustix::process::getuid().as_raw();
        let runtime = super::monitor::runtime_dir(me);
        delivered(&input, super::attend::submit, &runtime, me).await;
        return Ok(());
    }
    println!(
        "{}",
        answer(&input, &Detached, &mut std::io::stderr()).await
    );
    Ok(())
}

/// Submit the [`Delivery`] in `input` through `submit`, and what the
/// monitor said.
/// Where a detached delivery's failures are kept. Its stdout and stderr are
/// null so it can never write into the hook's output, and an evidence
/// failure leaves recovery without its question gate, so it must be
/// findable.
fn delivery_log(runtime: &Path) -> std::path::PathBuf {
    runtime.join("question-delivery.log")
}

/// Appends to [`delivery_log`] only inside a runtime directory that passes
/// the monitor's own check, creating a missing one private; a symlinked,
/// foreign or permissive one is refused, as is a log that is a symlink.
fn log_failure(runtime: &Path, me: u32, why: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if super::monitor::ensure_dir(runtime, me).is_err() {
        return;
    }
    let line = format!(
        "{} clank: question evidence was not recorded: {why}\n",
        super::github::log::now_epoch()
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(delivery_log(runtime))
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// The `--deliver` child: submit, and keep any failure in
/// [`delivery_log`] under `runtime`.
async fn delivered(
    input: &str,
    submit: impl AsyncFnOnce(&Path, super::monitor::Control) -> Result<String, super::attend::NotDone>,
    runtime: &Path,
    me: u32,
) {
    if let Err(e) = deliver(input, submit).await {
        log_failure(runtime, me, &e);
    }
}

async fn deliver(
    input: &str,
    submit: impl AsyncFnOnce(&Path, super::monitor::Control) -> Result<String, super::attend::NotDone>,
) -> Result<String, String> {
    let Delivery { repo, request } =
        serde_json::from_str(input).map_err(|e| format!("unreadable delivery: {e}"))?;
    submit(&repo, request).await.map_err(|e| e.to_string())
}

/// What the hook prints for `input`: the broker's reply, or `{}` for
/// Claude's own prompt. Evidence is handed to `child` first and never
/// waited for; a diagnostic about it goes to `err`.
async fn answer(input: &str, child: &impl HandOff, err: &mut impl std::io::Write) -> Value {
    if let Ok(hook) = serde_json::from_str::<Hook>(input) {
        match evidence(&hook) {
            Ok(Some((repo, request))) => {
                if let Err(e) = child.hand_off(&Delivery { repo, request }) {
                    let _ = writeln!(err, "clank: question evidence was not recorded: {e:#}");
                }
            }
            Err(e) => {
                let _ = writeln!(err, "clank: question evidence could not be routed: {e:#}");
            }
            Ok(None) => {}
        }
    }
    broker(input).await.unwrap_or_else(|_| json!({}))
}

/// The question broker's reply to `input`, if one is listening.
async fn broker(input: &str) -> Result<Value> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let hook: Hook = serde_json::from_str(input)?;
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
    tokio::time::timeout(std::time::Duration::from_secs(10), socket.write_all(&bytes)).await??;
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
    Ok(reply.output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::turn_child::tests::{SESSION, repo_with};

    /// Records each hand-off; the child it stands for never runs, so
    /// nothing it would deliver ever arrives.
    #[derive(Default)]
    struct Recorded(std::sync::Mutex<Vec<Delivery>>);

    impl HandOff for Recorded {
        fn hand_off(&self, delivery: &Delivery) -> Result<()> {
            self.0.lock().unwrap().push(delivery.clone());
            Ok(())
        }
    }

    fn native(cwd: &Path, event: &str) -> String {
        serde_json::to_string(&Hook {
            cwd: cwd.to_str().unwrap().into(),
            session_id: SESSION.into(),
            prompt_id: "turn".into(),
            hook_event_name: event.into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: json!({"questions":[{"header":"Choose","question":"Choose one"}]}),
            tool_use_id: (event != "PermissionRequest").then(|| "call".into()),
            tool_response: None,
        })
        .unwrap()
    }

    /// What the hook prints and says on stderr for `input`, and what it
    /// handed off; the hook must finish well inside the monitor's
    /// `ATTACH_BUDGET`, which is all a monitor start may take.
    async fn hook_on(input: &str) -> (Value, String, Vec<Delivery>) {
        let child = Recorded::default();
        let mut err = Vec::new();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            answer(input, &child, &mut err),
        )
        .await
        .expect("the hook answered without waiting on a monitor");
        (
            out,
            String::from_utf8(err).unwrap(),
            child.0.into_inner().unwrap(),
        )
    }

    /// The question broker's socket for `repo`, answering one frame with
    /// `output`; removed when dropped.
    struct Broker(std::path::PathBuf);

    impl Broker {
        fn saying(repo: &Path, output: Value) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            super::super::monitor::ensure_runtime_dir().unwrap();
            let repo = super::super::resolve_cwd_repo(repo).unwrap();
            let path = super::super::monitor::runtime_path(&repo, "claude-questions.sock");
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = tokio::io::BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                let reply = serde_json::to_string(&HookReply { proto: 1, output }).unwrap();
                socket
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            });
            Broker(path)
        }
    }

    impl Drop for Broker {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn the_brokers_reply_is_printed_without_waiting_for_the_evidence() {
        let (repo, author) = repo_with(Tool::Claude, false);
        let said = json!({"hookSpecificOutput": {"decision": "the broker's"}});
        let _broker = Broker::saying(repo.path(), said.clone());
        let (out, err, handed) = hook_on(&native(repo.path(), "PreToolUse")).await;
        assert_eq!((out, err.as_str()), (said, ""));
        let [Delivery { request, .. }] = handed.as_slice() else {
            panic!("one hand-off: {handed:?}")
        };
        let super::super::monitor::Control::Questions {
            author: by,
            observation: super::super::question_state::Observation::Asked { header, .. },
            ..
        } = request
        else {
            panic!("{request:?}")
        };
        assert_eq!((by.as_str(), header.as_str()), (author.as_str(), "Choose"));
    }

    #[tokio::test]
    async fn with_no_broker_claudes_own_prompt_is_answered_at_once() {
        let (repo, _) = repo_with(Tool::Claude, false);
        let (out, err, handed) = hook_on(&native(repo.path(), "PreToolUse")).await;
        assert_eq!((out, err.as_str(), handed.len()), (json!({}), "", 1));
    }

    #[tokio::test]
    async fn a_permission_request_is_never_routed() {
        let (bound, _) = repo_with(Tool::Claude, false);
        let unbound = tempfile::tempdir().unwrap();
        for cwd in [bound.path(), unbound.path()] {
            let (out, err, handed) = hook_on(&native(cwd, "PermissionRequest")).await;
            assert_eq!(
                (out, err.as_str(), handed.len()),
                (json!({}), "", 0),
                "{cwd:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_pre_tool_use_that_cannot_be_routed_says_so() {
        let unbound = tempfile::tempdir().unwrap();
        let (out, err, handed) = hook_on(&native(unbound.path(), "PreToolUse")).await;
        assert_eq!((out, handed.len()), (json!({}), 0));
        assert!(
            err.starts_with("clank: question evidence could not be routed:"),
            "{err}"
        );
    }

    /// The child submits what it is handed through `attend`'s own
    /// submission, here to a monitor answering in this process.
    #[tokio::test]
    async fn a_failed_delivery_is_kept_in_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let never =
            async |_: &Path, _| Ok::<String, super::super::attend::NotDone>("unreachable".into());
        let me = rustix::process::getuid().as_raw();
        delivered("not a delivery", never, dir.path(), me).await;
        let refused = async |_: &Path, _| {
            Err::<String, _>(super::super::attend::NotDone::NotTaken(
                "no monitor could be started".into(),
            ))
        };
        let request = serde_json::to_string(&Delivery {
            repo: dir.path().to_path_buf(),
            request: super::super::monitor::Control::Questions {
                author: "claude".into(),
                session: clank_core::SessionId::parse("00000000-0000-0000-0000-000000000000")
                    .unwrap(),
                observation: super::super::question_state::Observation::Result {
                    key: super::super::question_state::Key {
                        session: "00000000-0000-0000-0000-000000000000".into(),
                        prompt: "p".into(),
                        tool_use: "t".into(),
                    },
                },
            },
        })
        .unwrap();
        delivered(&request, refused, dir.path(), me).await;
        let kept = std::fs::read_to_string(delivery_log(dir.path())).unwrap();
        assert!(kept.contains("unreadable delivery"), "{kept}");
        assert!(kept.contains("no monitor could be started"), "{kept}");
        assert_eq!(kept.lines().count(), 2, "appended, not replaced: {kept}");
    }

    #[test]
    fn a_missing_runtime_directory_is_made_private_and_logged_in() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("clank-run");
        log_failure(&runtime, rustix::process::getuid().as_raw(), "no monitor");
        let mode = std::fs::metadata(&runtime).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            std::fs::read_to_string(delivery_log(&runtime))
                .unwrap()
                .contains("no monitor")
        );
    }

    #[test]
    fn an_untrusted_runtime_directory_is_never_written() {
        use std::os::unix::fs::PermissionsExt;
        let me = rustix::process::getuid().as_raw();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        log_failure(&linked, me, "no monitor");
        assert!(!delivery_log(&target).exists(), "followed the symlink");

        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        log_failure(&open, me, "no monitor");
        assert!(!delivery_log(&open).exists(), "wrote a world-writable dir");

        let private = dir.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let aimed = dir.path().join("aimed");
        std::os::unix::fs::symlink(&aimed, delivery_log(&private)).unwrap();
        log_failure(&private, me, "no monitor");
        assert!(!aimed.exists(), "followed a symlinked log");
    }

    #[tokio::test]
    async fn a_delivery_is_submitted_to_the_monitor() {
        use super::super::monitor::{ATTACH_BUDGET, Hello, Note, PROTO, Welcome, Who, greet_at};
        use std::io::{BufRead, Write};
        let (repo, author) = repo_with(Tool::Claude, false);
        let (repo_at, request) =
            evidence(&serde_json::from_str(&native(repo.path(), "PreToolUse")).unwrap())
                .unwrap()
                .unwrap();
        let input = serde_json::to_string(&Delivery {
            repo: repo_at,
            request,
        })
        .unwrap();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let root = repo.path().to_path_buf();
        let monitor = std::thread::spawn(move || {
            let mut line = String::new();
            std::io::BufReader::new(&theirs)
                .read_line(&mut line)
                .unwrap();
            let hello: Hello = serde_json::from_str(&line).unwrap();
            let Who::Control { request } = hello.who else {
                panic!("{line}")
            };
            let what = super::super::monitor::question_control(&root, &request).unwrap();
            for frame in [
                serde_json::to_string(&Welcome::Ok { proto: PROTO }).unwrap(),
                serde_json::to_string(&Note::Done { what }).unwrap(),
            ] {
                writeln!(&theirs, "{frame}").unwrap();
            }
        });
        let said = deliver(&input, async move |repo, control| {
            super::super::attend::submit_by(repo, control, async move |repo, who| {
                let repo = repo.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    greet_at(
                        "test-socket",
                        &repo,
                        who,
                        std::time::Instant::now() + ATTACH_BUDGET,
                        move |_, _| ours.try_clone(),
                    )
                    .into()
                })
                .await
                .unwrap()
            })
            .await
        })
        .await;
        monitor.join().unwrap();
        assert_eq!(said.as_deref(), Ok("recorded"));
        let dir = crate::agent_store::agents_root(repo.path()).join(author.as_str());
        assert_eq!(
            super::super::question_state::asking(&dir, SESSION)
                .unwrap()
                .header,
            "Choose"
        );
    }

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
