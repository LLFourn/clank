//! Native Grok handoff, deliberately separate from the hook-owned Stop wait.
//!
//! PreToolUse reserves one generation and rewrites a background `wait`.
//! PostToolUse confirms the provider's typed BackgroundTaskStarted result.
//! The same waiter process then runs the existing Clank wait. It cannot deliver
//! before confirmation, so even immediately pending work has a background
//! completion channel. Stop only checks/repairs; it never waits for work or spawns.
//!
//! One fixed slot per repository/label lives in the existing uid runtime dir.
//! A short metadata lock serializes transitions; a lifetime lease excludes a
//! second worker. Metadata contenders park in the kernel; the lifetime lease
//! remains nonblocking. A Unix datagram wakes the worker after
//! a persisted transition, with read-after-bind closing the startup race.
//! Only registration has a one-shot deadline. Idle delivery adds no timer,
//! task-list polling, filesystem watcher or helper process.

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::{AutoMode, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{StopHookArgs, WaitArgs, WaitCommandArgs};
use crate::agent_env::{Ambient, Process, resolve_identity_for_hook_in};
use crate::agent_store::{binding_matches, load_agent_config};

const MAX_INPUT: u64 = 1024 * 1024;
const MAX_RECORD: u64 = 64 * 1024;
const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const ARM: &str = "Run `clank wait` as one background terminal task (block_until_ms: 0, with no positive timeout), then finish the turn. Do not poll it or launch a second automatic wait.";

/// Native envelope only. Compatibility aliases are intentionally not used as
/// evidence of a supported native lifecycle. Missing required fields fail loud.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    hook_event_name: String,
    session_id: SessionId,
    cwd: PathBuf,
    prompt_id: Option<String>,
    reason: Option<String>,
    subagent_type: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Value,
    #[serde(default)]
    tool_result: Value,
    #[serde(default)]
    tool_input_truncated: bool,
    #[serde(default)]
    tool_result_truncated: bool,
    background_tasks: Option<Vec<Task>>,
}

#[derive(Debug, Deserialize)]
struct Task {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: String,
    command: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Specific {
    hook_event_name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_context: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct Output {
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    specific: Option<Specific>,
}

impl Output {
    fn decision(decision: &'static str, reason: impl Into<String>) -> Self {
        Self {
            decision: Some(decision),
            reason: Some(reason.into()),
            specific: None,
        }
    }
    fn context(reason: impl Into<String>) -> Self {
        Self {
            specific: Some(Specific {
                hook_event_name: "PostToolUse",
                additional_context: Some(reason.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    repo: PathBuf,
    label: AgentLabel,
    session: SessionId,
}

impl Owner {
    fn key(&self) -> String {
        // Session belongs in the record, not the path: resume/replacement must
        // not accumulate one socket/lock/record set per historical session.
        let mut hash = blake3::Hasher::new();
        hash.update(self.repo.as_os_str().as_encoded_bytes());
        hash.update(&[0]);
        hash.update(self.label.as_str().as_bytes());
        hash.finalize().to_hex()[..24].to_owned()
    }
    fn automatic(&self, env: &dyn Ambient) -> Result<bool> {
        let cfg = load_agent_config(&self.repo, &self.label)?;
        if !cfg
            .as_ref()
            .and_then(|cfg| cfg.session.as_ref())
            .is_some_and(|session| session.tool == Tool::Grok && session.id == self.session)
        {
            bail!(
                "this Grok session no longer owns `{}`; the previous handoff cannot deliver",
                self.label
            );
        }
        crate::agent_store::resolve_role(&self.repo, &self.label)?;
        Ok(
            super::team::resolve_effective_auto_mode(cfg.as_ref(), env.home().as_deref())
                == AutoMode::On,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Phase {
    Offered,
    Registered { task: String },
    Delivered { task: String },
    Cancelled,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    owner: Owner,
    generation: String,
    command: String,
    prompt: String,
    tool_use: String,
    tool_name: String,
    phase: Phase,
    repair_prompt: Option<String>,
    repairs: u8,
    /// Positive evidence of a legacy/manual provider task. Only a later native
    /// task snapshot can clear it; missing evidence cannot mean it was killed.
    #[serde(default)]
    manual_task: Option<String>,
}

struct Runtime {
    root: PathBuf,
    executable: PathBuf,
}

struct Lease(File);

impl Drop for Lease {
    fn drop(&mut self) {
        // Close alone can leave flock held by a child forked concurrently in
        // this process, until it execs and CLOEXEC takes effect. Explicit unlock
        // releases our open-file-description lease even in that short window.
        // SAFETY: this owned fd successfully acquired the lock in Runtime::lock.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl Runtime {
    fn production() -> Result<Self> {
        super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
        Ok(Self {
            root: super::monitor::runtime_dir(unsafe { libc::getuid() }),
            executable: std::env::current_exe()?,
        })
    }
    fn path(&self, key: &str, extension: &str) -> Result<PathBuf> {
        ensure!(is_hex(key, 24), "invalid Grok handoff key");
        Ok(self.root.join(format!("g_{key}.{extension}")))
    }
    fn lock_file(&self, key: &str, kind: &str) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path(key, kind)?)?;
        ensure!(
            file.metadata()?.is_file(),
            "Grok handoff lock is not a regular file"
        );
        Ok(file)
    }
    fn lock(&self, key: &str, kind: &str) -> Result<Option<Lease>> {
        let file = self.lock_file(key, kind)?;
        // SAFETY: valid owned descriptor, nonblocking advisory lease.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(Lease(file)))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }
    fn metadata_lock(&self, key: &str) -> Result<Lease> {
        let file = self.lock_file(key, "lock")?;
        // Every holder covers only synchronous local read/transition/publication.
        // Never hold this lock across await, provider execution, or lease waits.
        // The lifetime lease is tried nonblocking even under this lock, avoiding
        // a metadata/worker lock cycle. Normal contention is not a failed hook.
        loop {
            // SAFETY: this owned descriptor is a checked regular lock file.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Lease(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("locking Grok handoff transition");
            }
        }
    }
    fn read(&self, key: &str) -> Result<Option<Record>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path(key, "json")?)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.is_file(),
            "Grok handoff record is not a regular file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECORD + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD,
            "Grok handoff record exceeds its bound"
        );
        let record: Record = serde_json::from_slice(&bytes)?;
        ensure!(
            record.owner.key() == key && is_hex(&record.generation, 32),
            "Grok handoff identity mismatch"
        );
        Ok(Some(record))
    }
    fn write(&self, key: &str, record: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD,
            "Grok handoff record exceeds its bound"
        );
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&bytes)?;
        // Ephemeral runtime state needs atomic visibility, not durable recovery.
        temp.persist(self.path(key, "json")?)
            .map_err(|error| error.error)?;
        self.notify(key);
        Ok(())
    }
    fn notify(&self, key: &str) {
        // Never block a config write/hook on an idle consumer. A queued message
        // means it will re-read the persisted latch; missing socket is startup
        // or completion, covered by read-after-bind and the next guard.
        if let (Ok(socket), Ok(path)) = (
            std::os::unix::net::UnixDatagram::unbound(),
            self.path(key, "sock"),
        ) {
            if socket.set_nonblocking(true).is_ok() {
                let _ = socket.send_to(&[1], path);
            }
        }
    }
    fn command(&self, key: &str, generation: &str) -> Result<String> {
        let executable = self
            .executable
            .to_str()
            .context("Clank executable path is not UTF-8")?;
        let command = format!(
            "exec {} wait --handoff-key {key} --handoff-generation {generation}",
            shell_words::quote(executable)
        );
        ensure!(
            command.len() <= 1000,
            "Clank executable path exceeds Grok's command metadata bound"
        );
        Ok(command)
    }
    fn command_args(&self, command: &str) -> Option<(String, String)> {
        let words = shell_words::split(command).ok()?;
        let words = words.strip_prefix(&["exec".to_owned()]).unwrap_or(&words);
        if words.len() == 6
            && words[0] == self.executable.to_str()?
            && words[1] == "wait"
            && words[2] == "--handoff-key"
            && is_hex(&words[3], 24)
            && words[4] == "--handoff-generation"
            && is_hex(&words[5], 32)
        {
            Some((words[3].clone(), words[5].clone()))
        } else {
            None
        }
    }
    fn arm_request(&self, command: &str) -> bool {
        let Ok(words) = shell_words::split(command) else {
            return false;
        };
        let words = words.strip_prefix(&["exec".to_owned()]).unwrap_or(&words);
        words.len() == 2
            && (words[0] == "clank" || Some(words[0].as_str()) == self.executable.to_str())
            && words[1] == "wait"
    }
    fn cancel(&self, owner: &Owner) -> Result<()> {
        self.cancel_with(owner, false)
    }
    fn cancel_with(&self, owner: &Owner, new_session: bool) -> Result<()> {
        let key = owner.key();
        let _lock = self.metadata_lock(&key)?;
        if let Some(mut record) = self.read(&key)?.filter(|r| &r.owner == owner) {
            record.phase = Phase::Cancelled;
            if new_session {
                record.repairs = 0;
                record.repair_prompt = None;
                record.manual_task = None;
            }
            self.write(&key, &record)?;
        }
        Ok(())
    }
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn generation() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("handoff generation: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn background(input: &Value) -> bool {
    // Respect the native single-knob precedence. Never invent an aliased input
    // field or accept a positive wrapper timeout for an indefinite handoff.
    if input
        .get("timeout")
        .is_some_and(|v| !v.is_null() && v.as_u64() != Some(0))
    {
        return false;
    }
    if let Some(value) = input.get("block_until_ms").filter(|v| !v.is_null()) {
        return value.as_u64() == Some(0);
    }
    input.get("is_background").and_then(Value::as_bool) == Some(true)
        || input.get("background").and_then(Value::as_bool) == Some(true)
        || input.get("timeout").and_then(Value::as_u64) == Some(0)
}

fn pre(runtime: &Runtime, owner: &Owner, input: &Input, env: &dyn Ambient) -> Result<Output> {
    let Some(command) = input.tool_input.get("command").and_then(Value::as_str) else {
        return Ok(Output::default());
    };
    if let Some((key, generation)) = runtime.command_args(command) {
        let same_call = key == owner.key()
            && runtime.read(&key)?.is_some_and(|r| {
                &r.owner == owner
                    && r.generation == generation
                    && matches!(r.phase, Phase::Offered)
                    && input
                        .prompt_id
                        .as_ref()
                        .is_none_or(|prompt| prompt == &r.prompt)
                    && input.tool_use_id.as_ref() == Some(&r.tool_use)
                    && input.tool_name.as_ref() == Some(&r.tool_name)
                    && !input.tool_input_truncated
                    && background(&input.tool_input)
            });
        return Ok(if same_call {
            Output::default()
        } else {
            Output::decision(
                "deny",
                "A tagged automatic Grok wait may run only for the native tool call that reserved it. Do not replay a previous command or start a second task.",
            )
        });
    }
    if !runtime.arm_request(command) {
        return Ok(Output::default());
    }
    if !matches!(
        input.tool_name.as_deref(),
        Some("run_terminal_command" | "bash" | "Bash")
    ) {
        return Ok(Output::decision(
            "deny",
            "The automatic Grok handoff requires the native terminal tool. This tool name is unsupported; use `run_terminal_command` or check Grok's native lifecycle capability.",
        ));
    }
    if !owner.automatic(env)? {
        return Ok(Output::default());
    }
    if input.tool_input_truncated || !background(&input.tool_input) {
        return Ok(Output::decision("deny", ARM));
    }
    let tool_use = input
        .tool_use_id
        .as_ref()
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .context("native Grok toolUseId is absent or invalid; automatic handoff is unsupported")?;
    let key = owner.key();
    let _lock = runtime.metadata_lock(&key)?;
    let previous = runtime.read(&key)?;
    // Grok's actual Pre/PostToolUse envelopes omit promptId. The native
    // toolUseId binds registration and replay; Stop's prompt is only a turn
    // hint for delivery/recovery, never a replacement for tool-call identity.
    let prompt = input
        .prompt_id
        .as_ref()
        .filter(|p| !p.is_empty())
        .cloned()
        .or_else(|| {
            previous
                .as_ref()
                .filter(|r| &r.owner == owner)
                .and_then(|r| r.repair_prompt.clone())
        })
        .unwrap_or_else(|| tool_use.clone());
    if let Some(task) = previous
        .as_ref()
        .filter(|r| &r.owner == owner)
        .and_then(|r| r.manual_task.as_deref())
    {
        return Ok(Output::decision(
            "deny",
            format!(
                "Native task `{task}` is an existing manual clank wait. Explicitly terminate that task, then end your turn for a fresh Stop snapshot, or use `clank auto off` to keep manual operation. Clank will not start a duplicate or cancel a manual task for you."
            ),
        ));
    }
    if previous.as_ref().is_some_and(|r| {
        &r.owner == owner && matches!(r.phase, Phase::Offered | Phase::Registered { .. })
    }) {
        return Ok(Output::decision(
            "deny",
            "An automatic Grok handoff is already offered or registered. Keep the existing task; do not launch a duplicate.",
        ));
    }
    // A replaced worker must release its lease before another process owns it.
    let Some(_vacant) = runtime.lock(&key, "lease")? else {
        if let Some(mut old) = previous {
            old.phase = Phase::Cancelled;
            runtime.write(&key, &old)?;
        }
        return Ok(Output::decision(
            "deny",
            "The previous automatic Grok waiter is being cancelled. Do not replace its binding or start a second waiter.",
        ));
    };
    let generation = generation()?;
    let record = Record {
        owner: owner.clone(),
        command: runtime.command(&key, &generation)?,
        generation,
        prompt: prompt.clone(),
        tool_use: tool_use.clone(),
        tool_name: input
            .tool_name
            .clone()
            .expect("native terminal name checked above"),
        phase: Phase::Offered,
        repair_prompt: previous.as_ref().and_then(|r| r.repair_prompt.clone()),
        repairs: previous.as_ref().map_or(0, |r| r.repairs),
        manual_task: None,
    };
    runtime.write(&key, &record)?;
    let mut updated = input.tool_input.clone();
    updated["command"] = Value::String(record.command);
    Ok(Output {
        specific: Some(Specific {
            hook_event_name: "PreToolUse",
            updated_input: Some(updated),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn post(runtime: &Runtime, owner: &Owner, input: &Input) -> Result<Output> {
    let Some(command) = input.tool_input.get("command").and_then(Value::as_str) else {
        return Ok(Output::default());
    };
    let Some((key, generation)) = runtime.command_args(command) else {
        return Ok(Output::default());
    };
    ensure!(
        key == owner.key(),
        "Grok handoff command belongs to another repository or label"
    );
    let _lock = runtime.metadata_lock(&key)?;
    let Some(mut record) = runtime
        .read(&key)?
        .filter(|r| &r.owner == owner && r.generation == generation)
    else {
        return Ok(Output::context(
            "The automatic Grok handoff was superseded; it cannot deliver work. Do not restore an old binding.",
        ));
    };
    if !matches!(record.phase, Phase::Offered) {
        return Ok(Output::default());
    }
    let result = &input.tool_result;
    let valid = input.hook_event_name == "post_tool_use"
        && !input.tool_input_truncated
        && !input.tool_result_truncated
        && input
            .prompt_id
            .as_ref()
            .is_none_or(|prompt| prompt == &record.prompt)
        && input.tool_use_id.as_ref() == Some(&record.tool_use)
        && input.tool_name.as_ref() == Some(&record.tool_name)
        && result.get("type").and_then(Value::as_str) == Some("BackgroundTaskStarted")
        && result.get("task_type").and_then(Value::as_str) == Some("bash")
        && result.get("status").and_then(Value::as_str) == Some("running")
        && result.get("command").and_then(Value::as_str) == Some(record.command.as_str());
    let task = result
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= 256);
    if valid && task.is_some() {
        record.phase = Phase::Registered {
            task: task.unwrap().to_owned(),
        };
        record.repairs = 0;
        record.repair_prompt = None;
        runtime.write(&key, &record)?;
        Ok(Output::default())
    } else {
        let message = "Grok did not confirm the owned wait as a native background task. Automatic delivery is disabled for this attempt; pending work is preserved. Check native hook support or run `clank auto off` before manual `clank wait`.";
        record.phase = Phase::Failed {
            message: message.to_owned(),
        };
        runtime.write(&key, &record)?;
        Ok(Output::context(message))
    }
}

fn manual_wait_command(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    let words = words.strip_prefix(&["exec".to_owned()]).unwrap_or(&words);
    matches!(words, [program, subcommand, ..] if Path::new(program).file_name() == Some(std::ffi::OsStr::new("clank")) && subcommand == "wait")
        && !words.iter().any(|word| word.starts_with("--handoff-"))
}

fn stop(runtime: &Runtime, owner: &Owner, input: &Input, env: &dyn Ambient) -> Result<Output> {
    if input.reason.as_deref() != Some("end_turn") {
        return Ok(Output::default());
    }
    if !owner.automatic(env)? {
        runtime.cancel(owner)?;
        return Ok(Output::default());
    }
    let prompt = input
        .prompt_id
        .as_ref()
        .filter(|p| !p.is_empty())
        .context("native Grok promptId is absent; Stop cannot correlate a handoff")?;
    let key = owner.key();
    let _lock = runtime.metadata_lock(&key)?;
    let previous = runtime.read(&key)?;
    if let Some(record) = previous.as_ref().filter(|r| &r.owner == owner) {
        // PostToolUse is the authoritative registration proof, not a substring
        // of a capped Stop task description. The lease proves our process lives.
        if let Phase::Registered { task } = &record.phase {
            let starting = input.background_tasks.as_ref().is_some_and(|tasks| {
                tasks.iter().any(|t| {
                    &t.id == task
                        && t.kind == "shell"
                        && t.status == "running"
                        && t.command.as_deref() == Some(record.command.as_str())
                })
            });
            if starting || runtime.lock(&key, "lease")?.is_none() {
                return Ok(Output::default());
            }
        }
        // It could complete immediately after background registration. That
        // completion is queued by Grok, never an inline foreground result.
        if matches!(record.phase, Phase::Delivered { .. }) && &record.prompt == prompt {
            return Ok(Output::default());
        }
        if record.repairs >= 2 {
            // Failed background completions create fresh synthetic prompt ids.
            // They must not reset this budget and start an endless re-arm loop.
            return Ok(Output {
                reason: Some("Clank could not establish the automatic Grok handoff after two repairs. Pending work remains intact. Check native lifecycle hooks or run `clank auto off` before manual `clank wait`.".into()),
                ..Default::default()
            });
        }
    }
    let mut record = previous.filter(|r| &r.owner == owner).unwrap_or(Record {
        owner: owner.clone(),
        generation: generation()?,
        command: String::new(),
        prompt: prompt.clone(),
        tool_use: String::new(),
        tool_name: String::new(),
        phase: Phase::Cancelled,
        repair_prompt: None,
        repairs: 0,
        manual_task: None,
    });
    if let Some(tasks) = &input.background_tasks {
        record.manual_task = tasks
            .iter()
            .find(|task| {
                task.kind == "shell"
                    && task.status == "running"
                    && !task.id.is_empty()
                    && task.id.len() <= 256
                    && task.command.as_deref().is_some_and(manual_wait_command)
            })
            .map(|task| task.id.clone());
    }
    record.repairs = record.repairs.saturating_add(1);
    record.repair_prompt = Some(prompt.clone());
    record.phase = Phase::Cancelled;
    runtime.write(&key, &record)?;
    let reason = match &record.manual_task {
        Some(task) => format!(
            "Native task `{task}` is already running a manual clank wait. Choose explicitly: terminate that task and end your turn so Stop can confirm its absence, or run `clank auto off` to retain manual operation. Do not start another clank wait alongside it; Clank will not cancel your manual task."
        ),
        None => ARM.to_owned(),
    };
    Ok(Output::decision("block", reason))
}

fn handle(runtime: &Runtime, input: &Input, repo: &Path, env: &dyn Ambient) -> Result<Output> {
    if input.subagent_type.is_some() {
        return Ok(Output::default());
    }
    ensure!(
        dunce::canonicalize(&input.cwd)? == repo,
        "native Grok cwd does not match the Clank repository; refusing to bind or deliver across repositories"
    );
    let label = resolve_identity_for_hook_in(repo, Tool::Grok, &input.session_id, env)?;
    let owner = Owner {
        repo: repo.to_owned(),
        label,
        session: input.session_id.clone(),
    };
    match input.hook_event_name.as_str() {
        "session_start" => {
            // Only an explicit launched label may claim a session. Lookup-only
            // resumes identify an existing binding; no newest-mtime discovery.
            if env.var(crate::agent_env::ENV_CLANK_AGENT).is_some() {
                crate::agent_store::resolve_role(repo, &owner.label)?;
                crate::agent_store::bind_session_to_agent(
                    repo,
                    &owner.label,
                    Tool::Grok,
                    &owner.session,
                )?;
            }
            runtime.cancel_with(&owner, true)?;
            Ok(Output::default())
        }
        "session_end" | "stop_cancelled" | "stop_failure" => {
            runtime.cancel(&owner)?;
            Ok(Output::default())
        }
        _ if !binding_matches(repo, &owner.label, Tool::Grok, owner.session.as_str()) => {
            Ok(Output::default())
        }
        "pre_tool_use" => pre(runtime, &owner, input, env),
        "post_tool_use" | "post_tool_use_failure" | "permission_denied" => {
            post(runtime, &owner, input)
        }
        "stop" => stop(runtime, &owner, input, env),
        _ => Ok(Output::default()),
    }
}

pub(super) fn hook(args: StopHookArgs) -> Result<()> {
    // Hooks fail open, but errors must be visible rather than reported healthy.
    let result = (|| -> Result<Output> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(MAX_INPUT + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_INPUT,
            "native Grok hook input exceeds 1 MiB"
        );
        let input: Input =
            serde_json::from_slice(&bytes).context("unsupported native Grok hook payload")?;
        let repo = match super::resolve_repo(args.repo.as_deref().or(Some(&input.cwd))) {
            Ok(repo) if repo.join(".clank").is_dir() => repo,
            _ => return Ok(Output::default()),
        };
        handle(&Runtime::production()?, &input, &repo, &Process)
    })();
    match result {
        Ok(output) => {
            if output.decision.is_none() {
                if let Some(reason) = &output.reason {
                    eprintln!("clank: {reason}");
                }
            }
            if output.decision.is_some() || output.specific.is_some() {
                println!("{}", serde_json::to_string(&output)?);
            }
        }
        Err(error) => eprintln!(
            "clank: Grok lifecycle handoff unavailable: {error:#}. Pending work is preserved; check native hooks or use an explicit manual `clank wait`."
        ),
    }
    Ok(())
}

struct Worker<'a> {
    runtime: &'a Runtime,
    key: String,
    generation: String,
    owner: Owner,
    socket: tokio::net::UnixDatagram,
    _lease: Lease,
}

impl<'a> Worker<'a> {
    fn start(
        runtime: &'a Runtime,
        key: String,
        generation: String,
        env: &dyn Ambient,
    ) -> Result<Self> {
        ensure!(is_hex(&generation, 32), "invalid Grok handoff generation");
        let record = runtime
            .read(&key)?
            .context("Grok handoff has no native PreToolUse offer")?;
        ensure!(
            record.generation == generation,
            "Grok handoff was superseded"
        );
        ensure!(
            env.var(crate::agent_env::ENV_GROK_SESSION).as_deref()
                == Some(record.owner.session.as_str()),
            "automatic Grok wait belongs to another session"
        );
        ensure!(
            env.var(crate::agent_env::ENV_CLANK_AGENT)
                .is_none_or(|label| label == record.owner.label.as_str()),
            "automatic Grok wait belongs to another label"
        );
        ensure!(record.owner.automatic(env)?, "Clank auto mode is off");
        let lease = runtime
            .lock(&key, "lease")?
            .context("an automatic Grok wait already owns this slot")?;
        let path = runtime.path(&key, "sock")?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.file_type().is_socket(),
                    "Grok control path is not a socket"
                );
                std::fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let socket = tokio::net::UnixDatagram::bind(&path)?;
        if let Err(error) = super::monitor::restrict(&path) {
            let _ = std::fs::remove_file(&path);
            bail!("Grok control socket could not be restricted: {error}");
        }
        Ok(Self {
            runtime,
            key,
            generation,
            owner: record.owner,
            socket,
            _lease: lease,
        })
    }
    fn current(&self, env: &dyn Ambient) -> Result<Record> {
        let record = self
            .runtime
            .read(&self.key)?
            .context("Grok handoff record disappeared")?;
        ensure!(
            record.owner == self.owner && record.generation == self.generation,
            "Grok handoff was superseded"
        );
        ensure!(
            self.owner.automatic(env)?,
            "automatic Grok wait cancelled: Clank auto mode is off"
        );
        match &record.phase {
            Phase::Cancelled => {
                bail!("automatic Grok handoff cancelled; pending work remains intact")
            }
            Phase::Failed { message } => bail!("{message}"),
            Phase::Delivered { .. } => bail!("automatic Grok handoff already delivered"),
            _ => Ok(record),
        }
    }
    fn authorize_delivery(&self, env: &dyn Ambient) -> Result<()> {
        ensure!(
            self.owner.automatic(env)?,
            "automatic Grok wait cancelled before output: Clank auto mode is off"
        );
        let record = self
            .runtime
            .read(&self.key)?
            .context("Grok handoff record disappeared before output")?;
        ensure!(
            record.owner == self.owner
                && record.generation == self.generation
                && matches!(record.phase, Phase::Delivered { .. }),
            "automatic Grok handoff was cancelled or superseded before output"
        );
        Ok(())
    }
    async fn run<T>(&self, env: &dyn Ambient, work: impl Future<Output = Result<T>>) -> Result<T> {
        let mut message = [0; 1];
        // Registration may precede startup, or follow bind. Reading before
        // every recv handles both and messages are merely wake hints.
        tokio::time::timeout(REGISTRATION_TIMEOUT, async {
            loop {
                if matches!(self.current(env)?.phase, Phase::Registered { .. }) { return Ok::<(), anyhow::Error>(()); }
                self.socket.recv(&mut message).await?;
            }
        }).await.context("Grok did not confirm background registration within 30 seconds; check native PostToolUse hooks")??;
        tokio::pin!(work);
        loop {
            self.current(env)?;
            tokio::select! {
                biased;
                result = self.socket.recv(&mut message) => { result?; }
                result = &mut work => {
                    let result = result?;
                    let _lock = self.runtime.metadata_lock(&self.key)?;
                    let mut record = self.current(env)?;
                    let Phase::Registered { task } = record.phase else { bail!("Grok handoff lost its registration"); };
                    record.phase = Phase::Delivered { task };
                    self.runtime.write(&self.key, &record)?;
                    return Ok(result);
                }
            }
        }
    }
}

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        // Lease still held: no replacement may have bound this socket yet.
        if let Ok(path) = self.runtime.path(&self.key, "sock") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Select the provider lifecycle once at the CLI boundary; worker calls use the
/// existing items pipeline directly and never redispatch through this command.
pub(super) async fn run_wait(args: WaitCommandArgs) -> Result<()> {
    let tagged = args.handoff_key.is_some() || args.handoff_generation.is_some();
    let automatic = automatic_wait(&args.wait, &Process)?;
    if tagged {
        ensure!(
            automatic,
            "native handoff parameters require the current automatic Grok session"
        );
        return wait(
            args.handoff_key.context("missing native handoff key")?,
            args.handoff_generation
                .context("missing native handoff generation")?,
            args.wait,
        )
        .await;
    }
    ensure!(
        !automatic,
        "Grok's native PreToolUse adapter did not tag this `clank wait`. This does not establish that hooks are missing. End the turn so Stop can recover the handoff; use `clank auto off` before manual operation."
    );
    super::wait::run(args.wait).await
}

fn automatic_wait(args: &WaitArgs, env: &dyn Ambient) -> Result<bool> {
    if args.peek || args.r#for.is_some() || args.die_with_owner {
        return Ok(false);
    }
    let Some((Tool::Grok, session)) = crate::agent_env::detect_session_in(env)? else {
        return Ok(false);
    };
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let label = resolve_identity_for_hook_in(&repo, Tool::Grok, &session, env)?;
    if let Some(author) = args.author.as_deref() {
        ensure!(
            author == label.as_str(),
            "automatic Grok wait cannot override its session's agent"
        );
    }
    Owner {
        repo,
        label,
        session,
    }
    .automatic(env)
}

async fn wait(key: String, generation: String, mut args: WaitArgs) -> Result<()> {
    let runtime = Runtime::production()?;
    let worker = Worker::start(&runtime, key, generation, &Process)?;
    ensure!(
        args.repo.as_ref().is_none_or(
            |repo| super::resolve_repo(Some(repo)).ok().as_ref() == Some(&worker.owner.repo)
        ),
        "automatic Grok wait belongs to another repository"
    );
    args.repo = Some(worker.owner.repo.clone());
    args.author = Some(worker.owner.label.as_str().to_owned());
    args.no_poll = true;
    let json = args.json;
    let result = worker
        .run(
            &Process,
            super::wait::automatic_grok_items(args, worker.owner.session.clone()),
        )
        .await;
    match result {
        Ok(Some(items)) => {
            worker.authorize_delivery(&Process)?;
            emit_selected(&worker, &Process, &items, json, &mut std::io::stdout())?;
            Ok(())
        }
        Ok(None) => bail!("automatic Grok handoff produced no work"),
        Err(error) => {
            if let Ok(_lock) = runtime.metadata_lock(&worker.key) {
                if let Ok(Some(mut record)) = runtime.read(&worker.key) {
                    if record.owner == worker.owner
                        && record.generation == worker.generation
                        && !matches!(record.phase, Phase::Cancelled)
                    {
                        record.phase = Phase::Failed {
                            message: format!("{error:#}").chars().take(1024).collect(),
                        };
                        let _ = runtime.write(&worker.key, &record);
                    }
                }
            }
            Err(error)
        }
    }
}

fn emit_selected(
    worker: &Worker,
    env: &dyn Ambient,
    items: &[clank_core::wait::WaitItem],
    json: bool,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    worker.authorize_delivery(env)?;
    super::wait::emit_checked(items, json, None, writer)?;
    let selected: Vec<_> = items
        .iter()
        .filter_map(|item| item.presentation().cloned())
        .collect();
    super::presentation::record(&worker.owner.repo, &worker.owner.label, &selected, || {
        worker.authorize_delivery(env).is_ok()
    });
    Ok(())
}

/// One read-only diagnosis of the owned slot. It neither creates runtime
/// files nor sends IPC, scans processes, or interprets selection as reception.
pub(super) fn diagnose(
    repo: &Path,
    label: &AgentLabel,
    session: Option<&SessionId>,
) -> Result<String> {
    let Some(session) = session else {
        return Ok("unbound: no Grok session owns this label".into());
    };
    let uid = unsafe { libc::getuid() };
    let runtime = Runtime {
        root: super::monitor::runtime_dir(uid),
        executable: PathBuf::new(),
    };
    match super::monitor::trusted_dir(&runtime.root, uid) {
        super::monitor::Trust::Ours => {}
        super::monitor::Trust::Absent => return Ok("bound but no native handoff".into()),
        super::monitor::Trust::Theirs(why) => {
            bail!("unsafe Grok runtime namespace; diagnosis refused: {why}")
        }
    }
    let owner = Owner {
        repo: dunce::canonicalize(repo)?,
        label: label.clone(),
        session: session.clone(),
    };
    let Some(record) = runtime.read(&owner.key())? else {
        return Ok("bound but no native handoff".into());
    };
    if record.owner != owner {
        return Ok("replacement fenced: slot belongs to another session".into());
    }
    if record.manual_task.is_some() {
        return Ok("existing manual task: explicit migration choice required".into());
    }
    match record.phase {
        Phase::Offered => {
            Ok("provider task starting: offered; native PostToolUse confirmation pending".into())
        }
        Phase::Registered { .. } => {
            // Probe only an EXISTING lease inode, nonblocking and without write
            // access. Drop explicitly unlocks a vacant file description.
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(runtime.path(&owner.key(), "lease")?)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(
                        "provider registered task; worker has not acquired its lease".into(),
                    );
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(
                file.metadata()?.is_file(),
                "Grok lease is not a regular file"
            );
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                drop(Lease(file));
                Ok(
                    "provider registered task; no live owned waiter lease (starting or stopped)"
                        .into(),
                )
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    Ok("native handoff registered: provider task confirmed and owned waiter lease held; monitor subscription and model wake unverified".into())
                } else {
                    Err(error.into())
                }
            }
        }
        Phase::Delivered { .. } => Ok(
            "completion selected for stdout; Grok receipt and model reaction are unverified".into(),
        ),
        Phase::Cancelled => Ok("handoff cancelled; pending work preserved".into()),
        Phase::Failed { .. } => {
            Ok("handoff failed; pending work preserved (see native hook/task diagnostic)".into())
        }
    }
}

/// Called AFTER the config mutation is visible. Notification runs even when
/// the metadata lock is busy, so a live worker rechecks auto/binding. Startup
/// performs the same check after binding its socket. Manual waits have no slot.
pub(crate) fn config_changed(
    repo: &Path,
    label: &AgentLabel,
    previous: Option<&clank_core::agent_config::AgentConfig>,
    next: &clank_core::agent_config::AgentConfig,
) {
    let Some(old) = previous
        .and_then(|cfg| cfg.session.as_ref())
        .filter(|s| s.tool == Tool::Grok)
    else {
        return;
    };
    let replaced = next
        .session
        .as_ref()
        .is_none_or(|s| s.tool != Tool::Grok || s.id != old.id);
    if !replaced && next.auto_mode != Some(AutoMode::Off) {
        return;
    }
    let Ok(repo) = dunce::canonicalize(repo) else {
        return;
    };
    // Discovery only: this callback must not create runtime state merely because
    // someone changes a config in a repo that never used a native handoff.
    let runtime = Runtime {
        root: super::monitor::runtime_dir(unsafe { libc::getuid() }),
        executable: PathBuf::new(),
    };
    let owner = Owner {
        repo,
        label: label.clone(),
        session: old.id.clone(),
    };
    if runtime
        .path(&owner.key(), "json")
        .is_ok_and(|path| path.exists())
    {
        if let Err(error) = super::monitor::ensure_runtime_dir() {
            eprintln!("clank: Grok runtime namespace is unsafe: {error}");
            return;
        }
        if let Err(error) = runtime.cancel(&owner) {
            eprintln!("clank: cancelling automatic Grok handoff: {error:#}");
        }
        runtime.notify(&owner.key());
    }
}

#[cfg(test)]
#[path = "grok_handoff_tests.rs"]
mod tests;
