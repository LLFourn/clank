//! Bounded, launch-only Grok session preparation. ACP copies history to an
//! explicit cwd; interactive `--fork-session` would restore the parent's cwd.
//! No helper survives preparation, and no Clank binding is made here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::fork::{GrokForkIntent as ForkIntent, load_fork_spec, save_fork_spec};
use clank_core::agent_config::LaunchConfig;
use clank_core::ids::AgentLabel;

#[cfg(test)]
#[path = "grok_launch_tests.rs"]
mod tests;

const BUDGET: Duration = Duration::from_secs(30);
const OUTPUT_CAP: usize = 1024 * 1024;
const SUMMARY_CAP: u64 = 1024 * 1024;

impl ForkIntent {
    pub(crate) fn new(source_cwd: &Path) -> anyhow::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|e| anyhow::anyhow!("generating Grok child session ID: {e}"))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            source_cwd: source_cwd.to_owned(),
            child_id: format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            ),
            prepared: false,
        })
    }
}

pub(crate) struct Prepared {
    pub id: String,
    pub resume: bool,
    pub prompt: String,
}

/// The configured binary/env owns both preparation and interactive launch.
/// Args are global profile options, preceding ACP's subcommands.
pub(crate) struct Provider {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub root: PathBuf,
}

impl Provider {
    pub(crate) fn resolve(launch: Option<&LaunchConfig>, repo: &Path) -> anyhow::Result<Self> {
        let env = launch.map(|l| l.env.clone()).unwrap_or_default();
        validate_profile(launch)?;
        let root = env
            .get("GROK_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("GROK_HOME").map(PathBuf::from))
            .or_else(|| {
                env.get("HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                    .map(|h| h.join(".grok"))
            })
            .ok_or_else(|| anyhow::anyhow!("Grok preparation needs HOME or GROK_HOME"))?;
        Ok(Self {
            program: launch
                .and_then(|l| l.command.clone())
                .unwrap_or_else(|| "grok".into()),
            args: launch.map(|l| l.args.clone()).unwrap_or_default(),
            env,
            root: if root.is_absolute() {
                root
            } else {
                repo.join(root)
            },
        })
    }
}

pub(crate) fn validate_profile(launch: Option<&LaunchConfig>) -> anyhow::Result<()> {
    for arg in launch.into_iter().flat_map(|l| &l.args) {
        let flag = arg.split('=').next().unwrap_or(arg);
        anyhow::ensure!(
            ![
                "--resume",
                "--session-id",
                "--fork-session",
                "--cwd",
                "--worktree"
            ]
            .contains(&flag),
            "Grok launch profile cannot set `{flag}`; Clank owns session identity and working directory"
        );
    }
    Ok(())
}

pub(crate) fn session_dir(root: &Path, cwd: &Path, id: &str) -> PathBuf {
    let mut encoded = String::new();
    for b in cwd.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char)
            }
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    root.join("sessions").join(encoded).join(id)
}

fn uuid(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "invalid Grok session UUID `{id}`"
    );
    Ok(())
}

#[derive(Deserialize)]
struct Summary {
    info: Info,
    #[serde(default)]
    parent_session_id: Option<String>,
}
#[derive(Deserialize)]
struct Info {
    id: String,
    cwd: PathBuf,
}

fn read_summary(dir: &Path) -> anyhow::Result<Option<Summary>> {
    use std::io::Read;
    let file = match std::fs::File::open(dir.join("summary.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dir.try_exists()? => {
            return Ok(None);
        }
        Err(e) => {
            return Err(e)
                .context("reading Grok summary (existing incomplete session is not absent)");
        }
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Grok summary is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(SUMMARY_CAP + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= SUMMARY_CAP,
        "Grok summary exceeds {SUMMARY_CAP} bytes"
    );
    Ok(Some(
        serde_json::from_slice(&bytes).context("parsing Grok summary")?,
    ))
}

fn verify(summary: &Summary, id: &str, cwd: &Path, parent: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        summary.info.id == id
            && summary.info.cwd == cwd
            && summary.parent_session_id.as_deref() == parent,
        "Grok session identity/cwd/parent does not match the launch intent"
    );
    Ok(())
}

pub(crate) fn validate_resume(provider: &Provider, repo: &Path, id: &str) -> anyhow::Result<()> {
    uuid(id)?;
    let summary = read_summary(&session_dir(&provider.root, repo, id))?.ok_or_else(|| {
        anyhow::anyhow!(
            "Grok session `{id}` is not stored in `{}`; --cwd cannot relocate a saved session",
            repo.display()
        )
    })?;
    anyhow::ensure!(
        summary.info.id == id && summary.info.cwd == repo,
        "Grok resume session does not belong to this repository"
    );
    Ok(())
}

/// A legacy spec has no source cwd. Find its exact ID once during explicit
/// launch, never newest-mtime, and reject ambiguity. No recursive discovery.
fn legacy_source(root: &Path, id: &str) -> anyhow::Result<Option<PathBuf>> {
    let groups = match std::fs::read_dir(root.join("sessions")) {
        Ok(g) => g,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut found = None;
    for (n, group) in groups.enumerate() {
        anyhow::ensure!(
            n < 10000,
            "too many Grok cwd groups for legacy fork discovery"
        );
        let group = group?;
        if !group.file_type()?.is_dir() {
            continue;
        }
        if let Some(summary) = read_summary(&group.path().join(id))? {
            anyhow::ensure!(
                summary.info.id == id
                    && found.is_none()
                    && session_dir(root, &summary.info.cwd, id) == group.path().join(id),
                "ambiguous or mismatched legacy Grok source"
            );
            found = Some(summary.info.cwd);
        }
    }
    Ok(found)
}

/// Nonblocking preparation lock. Contention is reported, never polled, and no
/// async task blocks on flock. Descriptor closes on exec and on cancellation.
fn lock(repo: &Path, label: &AgentLabel) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    let path = super::monitor::runtime_path(repo, &format!("grok-prepare-{label}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Grok preparation lease is not a regular file"
    );
    // SAFETY: valid owned fd. LOCK_NB guarantees this never parks the runtime.
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Grok fork preparation is already owned or could not be locked: {}",
        std::io::Error::last_os_error()
    );
    Ok(file)
}

pub(crate) async fn prepare(
    repo: &Path,
    label: &AgentLabel,
    provider: &Provider,
) -> anyhow::Result<Prepared> {
    let _lock = lock(repo, label)?;
    let mut spec =
        load_fork_spec(repo, label)?.context("Grok fork intent disappeared before preparation")?;
    if spec.grok.is_none() {
        let source = match spec.from_session.as_deref() {
            Some(id) => {
                uuid(id)?;
                legacy_source(&provider.root, id)?
            }
            None => None,
        };
        if spec.from_session.is_some() && source.is_none() {
            eprintln!("warning: Grok fork source is absent; starting fresh with orientation");
            spec.from_session = None;
        }
        spec.grok = Some(ForkIntent::new(source.as_deref().unwrap_or(repo))?);
        save_fork_spec(repo, label, &spec)?;
    }
    let intent = spec.grok.as_ref().unwrap().clone();
    uuid(&intent.child_id)?;
    if let Some(source) = spec.from_session.as_deref() {
        uuid(source)?;
        anyhow::ensure!(
            source != intent.child_id,
            "Grok fork child must differ from its source"
        );
    }
    let child_dir = session_dir(&provider.root, repo, &intent.child_id);
    let child = read_summary(&child_dir)?;
    if intent.prepared {
        if let Some(summary) = child {
            verify(
                &summary,
                &intent.child_id,
                repo,
                spec.from_session.as_deref(),
            )?;
            return Ok(Prepared {
                id: intent.child_id,
                resume: true,
                prompt: spec.prompt,
            });
        }
        anyhow::ensure!(
            spec.from_session.is_none(),
            "prepared Grok child is missing; launch intent retained"
        );
        return Ok(Prepared {
            id: intent.child_id,
            resume: false,
            prompt: spec.prompt,
        });
    }
    // A crash between ACP copy and recording its successful response cannot
    // prove sidecar copy completion. Never overwrite or reuse that ambiguity.
    anyhow::ensure!(
        child.is_none(),
        "unconfirmed Grok child already exists at `{}`; intent retained; inspect the interrupted copy before retrying",
        child_dir.display()
    );
    if let Some(source) = spec.from_session.clone() {
        match read_summary(&session_dir(&provider.root, &intent.source_cwd, &source))? {
            None => {
                eprintln!("warning: Grok fork source is absent; starting fresh with orientation");
                spec.from_session = None;
                save_fork_spec(repo, label, &spec)?;
            }
            Some(summary) => {
                anyhow::ensure!(
                    summary.info.id == source && summary.info.cwd == intent.source_cwd,
                    "Grok fork source does not match its recorded cwd"
                );
                let result = fork_rpc(provider, repo, &source, &intent, BUDGET).await?;
                anyhow::ensure!(
                    result.new_session_id == intent.child_id
                        && result.new_cwd == repo
                        && result.parent_session_id == source,
                    "Grok ACP fork response does not match the launch intent"
                );
                let summary = read_summary(&child_dir)?
                    .context("Grok reported a fork but its child summary is missing")?;
                verify(&summary, &intent.child_id, repo, Some(&source))?;
            }
        }
    }
    spec.grok.as_mut().unwrap().prepared = true;
    save_fork_spec(repo, label, &spec)?;
    Ok(Prepared {
        id: intent.child_id,
        resume: spec.from_session.is_some(),
        prompt: spec.prompt,
    })
}

/// Called inside the binding write lock, before any state changes. A fork
/// cannot bind an ancestor or bypass unfinished preparation.
pub(crate) fn check_binding(repo: &Path, label: &AgentLabel, id: &str) -> anyhow::Result<bool> {
    let Some(spec) = load_fork_spec(repo, label)? else {
        return Ok(false);
    };
    if spec.tool != clank_core::vocab::Tool::Grok {
        return Ok(false);
    }
    let intent = spec
        .grok
        .context("Grok fork needs preparation; run `clank agent start` in this worktree")?;
    anyhow::ensure!(
        intent.prepared && intent.child_id == id,
        "this session is not the prepared Grok fork child; launch intent retained"
    );
    Ok(true)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkResponse {
    new_session_id: String,
    new_cwd: PathBuf,
    parent_session_id: String,
}

async fn fork_rpc(
    provider: &Provider,
    repo: &Path,
    source: &str,
    intent: &ForkIntent,
    budget: Duration,
) -> anyhow::Result<ForkResponse> {
    let result = acp_rpc(provider, repo, Some(("_x.ai/session/fork", json!({"sourceSessionId":source,"sourceCwd":intent.source_cwd,"newCwd":repo,"newSessionId":intent.child_id,"sessionKind":"fork"}))), budget).await?;
    serde_json::from_value(result).context("invalid Grok ACP fork response")
}

/// A bounded explicit setup/launch/doctor handshake, never an idle probe.
pub(super) async fn initialize(provider: &Provider, repo: &Path) -> anyhow::Result<Value> {
    acp_rpc(provider, repo, None, Duration::from_secs(10)).await
}

async fn acp_rpc(
    provider: &Provider,
    repo: &Path,
    followup: Option<(&str, Value)>,
    budget: Duration,
) -> anyhow::Result<Value> {
    let mut command = tokio::process::Command::new(&provider.program);
    command
        .args(&provider.args)
        .args(["agent", "--no-leader", "stdio"])
        .current_dir(repo)
        .envs(&provider.env)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // Profile overrides cannot smuggle the interactive label into preparation.
    for key in crate::agent_env::SESSION_IDENTITY_VARS {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting Grok ACP preparation with `{}`", provider.program))?;
    let mut group = super::wait::GroupKill::over(child.id());
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut error = child.stderr.take().unwrap();
    let mut tail = super::wait::RingTail::new(4096);
    let mut total = 0;
    let result = tokio::time::timeout(budget, async {
        let conversation = async {
            let init = request(&mut input, &mut output, &mut total, 1, "initialize", json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"clank-fork","version":env!("CARGO_PKG_VERSION")}})).await?;
            anyhow::ensure!(init.get("protocolVersion").and_then(Value::as_u64) == Some(1), "unsupported Grok ACP protocol version");
            match followup {
                Some((method, params)) => request(&mut input, &mut output, &mut total, 2, method, params).await,
                None => Ok(init),
            }
        };
        let errors = async {
            let mut buf = [0; 4096];
            loop { let n = error.read(&mut buf).await?; if n == 0 { break; } tail.push(&buf[..n]); }
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(conversation, errors);
        // Drain stderr while talking. No spawned reader can outlive this call.
        tokio::select! {
            result = &mut conversation => result,
            drained = &mut errors => { drained?; conversation.await },
        }
    }).await;
    group.terminate();
    let _ = child.start_kill();
    let reaped = tokio::time::timeout(Duration::from_millis(200), child.wait()).await;
    let result = result
        .map_err(|_| anyhow::anyhow!("Grok ACP preparation timed out after {budget:?}"))
        .and_then(|r| r);
    let result = result.with_context(|| {
        format!(
            "Grok ACP preparation failed; stderr (last 4096 bytes): {}",
            tail.into_string()
        )
    })?;
    anyhow::ensure!(
        matches!(reaped, Ok(Ok(_))),
        "Grok ACP helper could not be reaped within cleanup deadline"
    );
    Ok(result)
}

async fn request<W: tokio::io::AsyncWrite + Unpin, R: tokio::io::AsyncBufRead + Unpin>(
    input: &mut W,
    output: &mut R,
    total: &mut usize,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let mut bytes =
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
    bytes.push(b'\n');
    input.write_all(&bytes).await?;
    input.flush().await?;
    loop {
        let mut line = Vec::new();
        loop {
            let buf = output.fill_buf().await?;
            anyhow::ensure!(
                !buf.is_empty(),
                "Grok ACP ended before answering `{method}`"
            );
            let n = buf
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(buf.len());
            anyhow::ensure!(
                *total + n <= OUTPUT_CAP,
                "Grok ACP output exceeds {OUTPUT_CAP} bytes"
            );
            *total += n;
            line.extend_from_slice(&buf[..n]);
            output.consume(n);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        let reply: Value =
            serde_json::from_slice(&line).context("Grok ACP emitted malformed JSON")?;
        anyhow::ensure!(
            reply.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
            "invalid Grok ACP envelope"
        );
        if reply.get("id").is_none() {
            continue;
        } // notifications are bounded too
        anyhow::ensure!(
            reply.get("id").and_then(Value::as_u64) == Some(id),
            "Grok ACP answered the wrong request"
        );
        if let Some(error) = reply.get("error") {
            anyhow::bail!("Grok ACP `{method}` rejected: {error}");
        }
        return reply
            .get("result")
            .cloned()
            .context("Grok ACP reply has no result");
    }
}
