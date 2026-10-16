//! Launch provenance independent of hooks, inboxes and mutable session bindings.
//! Reads inspect one recorded process on demand; no process scan or idle worker.

use crate::proc_identity::{
    ProcToken,
    inspect::{self, Inspection, ProcessState},
};
use anyhow::{Context, ensure};
use clank_core::{Tool, ids::AgentLabel};
use std::path::{Path, PathBuf};

use super::open_zellij::{self, SessionPane, SessionScan};

/// One explicit batch lookup; healthy servers respond well below this deadline.
const SCAN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// Shared nudge/event discovery. No hook, binding, inbox or role lookup, and no
/// idle worker. The recorded running provider, including overrides, supplies keys.
pub(crate) async fn discover(repo: PathBuf, label: AgentLabel) -> anyhow::Result<(Found, Tool)> {
    let scan = open_zellij::panes_in_all_sessions_async(SCAN_DEADLINE).await;
    super::event_interrupt::blocking(move || {
        let found = locate(&scan, &repo, &label)?;
        let tool = resolve(&repo, &label, &found.session, &found.pane)?;
        Ok((found, tool))
    })
}

/// The one pane a nudge may type into.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) session: String,
    pub(crate) pane: String,
}

/// Pick the target from one scan, or explain why there isn't one.
///
/// Fails CLOSED on every doubt. A launch may treat a session that did
/// not answer as one fewer pane; here the same silence could be hiding
/// the target or a second copy of it, and typing a message to a human
/// into an arbitrary agent is the outcome this command exists to avoid.
pub(crate) fn locate(scan: &SessionScan, repo: &Path, label: &AgentLabel) -> anyhow::Result<Found> {
    let command = open_zellij::agent_start_command(label.as_str(), &repo.to_string_lossy());
    let launched: Vec<&SessionPane> = scan
        .panes
        .iter()
        .filter(|sp| sp.pane.launched_by(&command))
        .collect();
    let live: Vec<&&SessionPane> = launched.iter().filter(|sp| !sp.pane.has_exited()).collect();

    if scan.listing_failed {
        anyhow::bail!(
            "could not ask zellij what sessions exist, so nothing is known about \
             `{label}`'s pane — not even whether there is one. Check that `zellij` runs, \
             then retry; nothing was typed."
        );
    }

    let missed = || scan.unanswered.join(", ");
    match live.as_slice() {
        [one] if scan.unanswered.is_empty() => Ok(Found {
            session: one.session.clone(),
            pane: one.pane.pane_id(),
        }),
        [one] => anyhow::bail!(
            "`{label}` has a pane in session `{}`, but session(s) {} did not answer — one of \
             them may hold another `{label}` for this repo, and a nudge to the wrong one is \
             not recoverable. Close or fix the unresponsive session, then retry.",
            one.session,
            missed()
        ),
        [] if !launched.is_empty() => anyhow::bail!(
            "`{label}`'s pane in session `{}` has EXITED — its agent is not running, and \
             zellij would accept the keystrokes anyway. Restart it (Ctrl-a r in the \
             console) and retry.",
            launched[0].session
        ),
        [] if !scan.unanswered.is_empty() => anyhow::bail!(
            "no live pane for `{label}` in {}, and session(s) {} did not answer within \
             {SCAN_DEADLINE:?} — this is not the same as `{label}` having no pane, so \
             nothing was typed.",
            repo.display(),
            missed()
        ),
        [] => anyhow::bail!(
            "no pane for `{label}` in {} in any live zellij session — `clank open zellij` \
             starts the team.",
            repo.display()
        ),
        many => anyhow::bail!(
            "`{label}` in {} has live panes in {} sessions ({}) — close the duplicate, then \
             retry. Typing into an arbitrary one is exactly the misdelivery this command \
             exists to avoid.",
            repo.display(),
            many.len(),
            many.iter()
                .map(|sp| sp.session.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Launch {
    version: u32,
    session: String,
    pane: String,
    tool: Tool,
    program: String,
    pid: i32,
    birth: ProcToken,
    /// Absent from a record written before launches were watched.
    #[serde(default)]
    label: Option<String>,
    /// The version of the file exec'd, asked of it just before, or why
    /// there is none. Absent from a record written before clank asked.
    #[serde(default)]
    harness: Option<Result<String, String>>,
}

fn path(repo: &Path, label: &AgentLabel, session: &str, pane: &str) -> PathBuf {
    // Length-delimited fields prevent aliases and keep session names out of paths.
    let mut hash = blake3::Hasher::new();
    for field in [label.as_str(), session, pane] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    super::monitor::runtime_path(
        repo,
        &format!("pane-launch-{}.json", hash.finalize().to_hex()),
    )
}

/// Called after preview/repair and immediately before exec. Nothing for non-pane
/// launches. Atomic replacement avoids exposing a half-written launch record.
pub(crate) fn record(
    repo: &Path,
    label: &AgentLabel,
    tool: Tool,
    program: &str,
    harness: Result<String, String>,
) -> anyhow::Result<()> {
    let (Some(session), Some(pane)) = (
        super::open_zellij::current_session(),
        super::open_zellij::caller_pane_id(),
    ) else {
        return Ok(());
    };
    let pid = i32::try_from(std::process::id())?;
    let birth =
        crate::proc_identity::token_for(pid).context("cannot establish launch process birth")?;
    super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    let destination = path(repo, label, &session, &pane);
    publish(
        &destination,
        &Launch {
            version: 1,
            session,
            pane,
            tool,
            program: program.into(),
            pid,
            birth,
            label: Some(label.as_str().to_string()),
            harness: Some(harness),
        },
    )
}

/// A recorded pane launch. The process was exec'd in its pane, so it
/// lives exactly as long as the pane holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneLaunch {
    pub(crate) record: PathBuf,
    pub(crate) label: String,
    pub(crate) pid: i32,
    pub(crate) birth: ProcToken,
}

/// The directory launch records live in, and this repo's filename
/// prefix there.
pub(crate) fn launch_dir(repo: &Path) -> (PathBuf, String) {
    let probe = super::monitor::runtime_path(repo, "pane-launch-");
    let prefix = probe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (
        probe.parent().map(Path::to_path_buf).unwrap_or_default(),
        prefix,
    )
}

/// This repo's labelled launch records for zellij `session`, in path
/// order. Records written before labels were, and unreadable ones, are
/// left out.
pub(crate) fn launches_in(repo: &Path, session: &str) -> Vec<PaneLaunch> {
    let (dir, prefix) = launch_dir(repo);
    launches_at(&dir, &prefix, session)
}

pub(crate) fn launches_at(dir: &Path, prefix: &str, session: &str) -> Vec<PaneLaunch> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PaneLaunch> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix) && name.ends_with(".json")
        })
        .filter_map(|entry| {
            let record = entry.path();
            let launch = read(&record).ok()?;
            (launch.session == session).then_some(())?;
            Some(PaneLaunch {
                label: launch.label?,
                pid: launch.pid,
                birth: launch.birth,
                record,
            })
        })
        .collect();
    found.sort_by(|a, b| a.record.cmp(&b.record));
    found
}

/// A labelled launch record at `destination`, for tests elsewhere.
#[cfg(test)]
pub(crate) fn publish_for_test(
    destination: &Path,
    session: &str,
    label: &str,
    pid: i32,
    birth: ProcToken,
) {
    publish(
        destination,
        &Launch {
            version: 1,
            session: session.into(),
            pane: "terminal_1".into(),
            tool: Tool::Claude,
            program: "claude".into(),
            pid,
            birth,
            label: Some(label.into()),
            harness: None,
        },
    )
    .unwrap();
}

fn publish(destination: &Path, launch: &Launch) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(
        destination
            .parent()
            .context("launch record has no parent")?,
    )?;
    serde_json::to_writer(&mut file, launch)?;
    file.flush()?;
    file.as_file().sync_all()?;
    // Persist replaces a symlink rather than following it. The vetted private
    // runtime directory, not a configurable provider home, owns this record.
    file.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

/// The provider process a verified launch record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) pid: i32,
    pub(crate) birth: ProcToken,
    /// The harness version it was launched at, or why it has none.
    pub(crate) harness: Result<String, String>,
}

/// [`discover`], with the provider process its launch record names.
pub(crate) async fn discover_process(
    repo: PathBuf,
    label: AgentLabel,
) -> anyhow::Result<(Found, Tool, Process)> {
    let scan = open_zellij::panes_in_all_sessions_async(SCAN_DEADLINE).await;
    super::event_interrupt::blocking(move || {
        let found = locate(&scan, &repo, &label)?;
        let (tool, process) = resolve_process(&repo, &label, &found.session, &found.pane)?;
        Ok((found, tool, process))
    })
}

pub(crate) fn resolve(
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    pane: &str,
) -> anyhow::Result<Tool> {
    resolve_process(repo, label, session, pane).map(|(tool, _)| tool)
}

fn resolve_process(
    repo: &Path,
    label: &AgentLabel,
    session: &str,
    pane: &str,
) -> anyhow::Result<(Tool, Process)> {
    let root = super::monitor::runtime_dir(unsafe { libc::getuid() });
    ensure!(
        matches!(
            super::monitor::trusted_dir(&root, unsafe { libc::getuid() }),
            super::monitor::Trust::Ours
        ),
        "launch provenance is unavailable; restart `{label}` through clank before nudging"
    );
    let launch = read(&path(repo, label, session, pane))?;
    let observation = inspect::inspect(launch.pid, &launch.birth);
    let tool = verify(&launch, session, pane, observation)?;
    Ok((
        tool,
        Process {
            pid: launch.pid,
            birth: launch.birth,
            harness: launch.harness.unwrap_or_else(|| {
                Err(
                    "it was launched before clank recorded its harness's version; \
                     restart it through clank"
                        .into(),
                )
            }),
        },
    ))
}

fn read(destination: &Path) -> anyhow::Result<Launch> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let metadata = std::fs::symlink_metadata(&destination)
        .context("launch provenance is missing; restart this agent through clank before nudging")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "launch provenance is not a regular file"
    );
    ensure!(
        metadata.uid() == unsafe { libc::getuid() } && metadata.mode() & 0o022 == 0,
        "launch provenance has unsafe ownership or permissions"
    );
    let mut bytes = Vec::new();
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?
        .take(16385)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16384,
        "launch provenance is malformed or oversized"
    );
    serde_json::from_slice(&bytes).context("invalid launch provenance")
}

fn verify(
    launch: &Launch,
    session: &str,
    pane: &str,
    observation: Inspection,
) -> anyhow::Result<Tool> {
    ensure!(
        launch.version == 1 && launch.session == session && launch.pane == pane,
        "launch provenance belongs to a different pane or unsupported version"
    );
    let Inspection::Verified(process) = observation else {
        anyhow::bail!(
            "recorded launch process has ended, changed, or cannot be verified; nothing was typed"
        );
    };
    ensure!(
        process.pid == launch.pid && process.birth == launch.birth,
        "process observation belongs to another launch"
    );
    ensure!(
        !matches!(process.state, Ok(ProcessState::Zombie | ProcessState::Dead)),
        "launch process has ended"
    );
    let argv = process
        .argv
        .map_err(|_| anyhow::anyhow!("cannot inspect the running provider command"))?;
    let executable = argv.first().and_then(|arg| Path::new(arg).file_name());
    let expected = Path::new(&launch.program).file_name();
    let provider = std::ffi::OsStr::new(launch.tool.as_str());
    // The launcher itself can still be alive in the narrow pre-exec window.
    // Never interpret its presence as proof that the provider has started.
    ensure!(
        executable.is_some()
            && executable != Some(std::ffi::OsStr::new("clank"))
            && (executable == expected || executable == Some(provider)),
        "the recorded process is not the selected provider or configured executable; nothing was typed"
    );
    Ok(launch.tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch() -> Launch {
        Launch {
            version: 1,
            session: "live".into(),
            pane: "terminal_9".into(),
            tool: Tool::Grok,
            program: "/opt/bin/grok".into(),
            pid: 123,
            birth: ProcToken::MacStart { sec: 1, usec: 2 },
            label: None,
            harness: None,
        }
    }

    #[test]
    fn launches_are_this_repos_labelled_records_for_this_session() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, session: &str, label: Option<&str>| {
            let mut l = launch();
            l.session = session.into();
            l.label = label.map(str::to_string);
            publish(&dir.path().join(name), &l).unwrap();
        };
        write("tag.pane-launch-a.json", "live", Some("claude"));
        write("tag.pane-launch-b.json", "live", Some("claude"));
        write("tag.pane-launch-c.json", "other", Some("codex"));
        write("tag.pane-launch-d.json", "live", None);
        write("elsewhere.pane-launch-e.json", "live", Some("codex"));
        let found = launches_at(dir.path(), "tag.pane-launch-", "live");
        let names: Vec<String> = found
            .iter()
            .map(|l| l.record.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["tag.pane-launch-a.json", "tag.pane-launch-b.json"]);
        assert!(found.iter().all(|l| l.label == "claude"));
    }

    #[test]
    fn a_record_written_before_labels_still_parses() {
        let mut json = serde_json::to_value(launch()).unwrap();
        json.as_object_mut().unwrap().remove("label");
        let parsed: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.label, None);
    }
    fn observed(program: &str) -> Inspection {
        Inspection::Verified(inspect::ProcessDetails {
            pid: 123,
            birth: launch().birth,
            observed_at: time::OffsetDateTime::UNIX_EPOCH,
            argv: Ok(vec![program.into()]),
            state: Ok(ProcessState::Sleeping),
            parent: Ok(1),
            group: Ok(123),
        })
    }
    #[test]
    fn actual_launch_provider_wins_without_reading_roster_or_bindings() {
        assert_eq!(
            verify(&launch(), "live", "terminal_9", observed("/opt/bin/grok")).unwrap(),
            Tool::Grok
        );
    }

    #[test]
    fn different_panes_cannot_overwrite_each_others_launch_evidence() {
        let repo = Path::new("/r");
        let label = AgentLabel::parse("grok").unwrap();
        assert_ne!(
            path(repo, &label, "one", "terminal_1"),
            path(repo, &label, "one", "terminal_2")
        );
        assert_ne!(
            path(repo, &label, "one", "terminal_1"),
            path(repo, &label, "other", "terminal_1")
        );
    }
    #[test]
    fn stale_identity_or_wrong_pane_never_authorizes_input() {
        for observation in [Inspection::Ended, Inspection::Mismatched] {
            assert!(verify(&launch(), "live", "terminal_9", observation).is_err());
        }
        assert!(verify(&launch(), "other", "terminal_9", observed("grok")).is_err());
        assert!(verify(&launch(), "live", "terminal_8", observed("grok")).is_err());
    }
    #[test]
    fn pre_exec_launcher_and_changed_executable_are_refused() {
        for program in ["clank", "/bin/bash", "claude"] {
            assert!(verify(&launch(), "live", "terminal_9", observed(program)).is_err());
        }
    }

    #[test]
    fn publication_replaces_a_symlink_without_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "preserve").unwrap();
        let destination = dir.path().join("launch");
        std::os::unix::fs::symlink(&outside, &destination).unwrap();
        assert!(read(&destination).is_err());
        publish(&destination, &launch()).unwrap();
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "preserve");
        assert_eq!(read(&destination).unwrap().tool, Tool::Grok);
    }

    #[test]
    fn native_process_identity_can_corroborate_a_published_record() {
        let dir = tempfile::tempdir().unwrap();
        let pid = i32::try_from(std::process::id()).unwrap();
        let birth = crate::proc_identity::token_for(pid).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut record = launch();
        record.pid = pid;
        record.birth = birth;
        record.program = executable.to_str().unwrap().into();
        let destination = dir.path().join("launch");
        publish(&destination, &record).unwrap();
        let stored = read(&destination).unwrap();
        assert_eq!(
            verify(
                &stored,
                "live",
                "terminal_9",
                inspect::inspect(pid, &stored.birth)
            )
            .unwrap(),
            Tool::Grok
        );
        // The fixture proves kernel identity and publication, not that this
        // test runner is a real Grok harness or that a model was interrupted.
    }
}
