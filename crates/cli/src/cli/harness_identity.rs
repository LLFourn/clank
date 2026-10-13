//! The version of the harness an agent is actually running.
//!
//! The only version a capability may be judged by. A record states the
//! version that began its session (Codex never restates it on resume), and
//! a path probed later names whatever file is there now, which an upgrade
//! may have replaced under a running process. So the version is taken once,
//! by `clank agent start`, from the very file it is about to exec, and kept
//! in the launch record that names the process it becomes.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clank_core::Tool;
use clank_core::ids::AgentLabel;

use super::pane_target::Process;

/// An agent's provider process, and the version it was launched at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Running {
    pub(crate) process: Process,
    pub(crate) version: String,
}

/// How long the executable has to say its version.
const PROBE_WITHIN: std::time::Duration = std::time::Duration::from_secs(3);

/// `label`'s running harness and the version it was launched at, or why
/// that cannot be established.
pub(crate) async fn identify(repo: PathBuf, label: AgentLabel) -> Result<Running, String> {
    let (_, _, process) = super::pane_target::discover_process(repo, label)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let version = process.harness.clone()?;
    Ok(Running { process, version })
}

/// What `clank agent start` is about to exec: the file `program` resolves
/// to, which the launch then execs, and its version, or why there is none.
#[derive(Debug)]
pub(crate) struct Launch {
    pub(crate) exe: Option<PathBuf>,
    pub(crate) version: Result<String, String>,
}

/// `program`, as `clank agent start` is about to exec it for `tool`:
/// resolved from the launch's directory `cwd` on its `path`, and asked its
/// version in that same context (`context` applies the launch's directory
/// and environment, as the exec will). The file must stay the same file
/// throughout and must answer as that harness.
pub(crate) fn at_launch(
    tool: Tool,
    program: &str,
    cwd: &Path,
    path: Option<&OsStr>,
    context: impl Fn(&mut std::process::Command),
) -> Launch {
    let Some(exe) = resolve(program, cwd, path) else {
        return Launch {
            exe: None,
            version: Err(format!("`{program}` was not found")),
        };
    };
    let version = (|| {
        let before = file_identity(&exe).ok_or("its executable could not be read")?;
        let printed = probe(&exe, &context)
            .ok_or_else(|| format!("{} did not report a version", exe.display()))?;
        if file_identity(&exe) != Some(before) {
            return Err("its executable changed while its version was read".to_string());
        }
        harness_version(tool, &printed)
            .ok_or_else(|| format!("{} did not answer as {}", exe.display(), tool.as_str()))
    })();
    Launch {
        exe: Some(exe),
        version,
    }
}

/// `program` as exec would find it from `cwd` on `path`.
fn resolve(program: &str, cwd: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(cwd.join(program)).filter(|p| p.is_file());
    }
    std::env::split_paths(path?)
        .map(|dir| cwd.join(dir).join(program))
        .find(|candidate| candidate.is_file())
}

/// Which file `exe` is, following links, and its state.
fn file_identity(exe: &Path) -> Option<(u64, u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(exe).ok()?;
    Some((m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

/// What `exe --version` prints, run in `context`, if it both exits and
/// closes its output within one budget. Its whole process group is killed
/// before it is reaped, whether it finished or not, so no descendant holding
/// its output outlives the probe.
fn probe(exe: &Path, context: &impl Fn(&mut std::process::Command)) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt;
    const OUTPUT_CAP: u64 = 64 * 1024;
    let mut command = std::process::Command::new(exe);
    context(&mut command);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;
    let mut group = super::wait::GroupKill::over(Some(child.id()));
    let stdout = child.stdout.take()?;
    let (sent, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(OUTPUT_CAP).read_to_end(&mut buf);
        let _ = sent.send(buf);
    });
    let deadline = std::time::Instant::now() + PROBE_WITHIN;
    let printed = output.recv_timeout(PROBE_WITHIN).ok();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => break None,
        }
    };
    group.terminate();
    let _ = child.kill();
    let _ = child.wait();
    match (printed, status) {
        (Some(printed), Some(status)) if status.success() => {
            Some(String::from_utf8_lossy(&printed).into_owned())
        }
        _ => None,
    }
}

/// The version in what `tool`'s own `--version` prints, and only if it is
/// that harness speaking: `2.1.280 (Claude Code)`, `codex-cli 0.161.0`. A
/// runtime or wrapper answering for it is not the harness.
fn harness_version(tool: Tool, printed: &str) -> Option<String> {
    let line = printed.lines().next()?.trim();
    let version = match tool {
        Tool::Claude => line.strip_suffix("(Claude Code)")?.trim(),
        Tool::Codex => line.strip_prefix("codex-cli ")?.trim(),
        Tool::Grok | Tool::OpenCode => return None,
    };
    version
        .starts_with(|c: char| c.is_ascii_digit())
        .then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the harness itself, in its own words, gives a version: a
    /// runtime or wrapper answering for it does not.
    #[test]
    fn a_version_is_only_the_harness_speaking() {
        assert_eq!(
            harness_version(Tool::Claude, "2.1.280 (Claude Code)\n").as_deref(),
            Some("2.1.280")
        );
        assert_eq!(
            harness_version(Tool::Codex, "codex-cli 0.161.0\n").as_deref(),
            Some("0.161.0")
        );
        assert_eq!(harness_version(Tool::Claude, "v22.15.0"), None, "node");
        assert_eq!(
            harness_version(Tool::Claude, "1.1.38"),
            None,
            "a bare runtime version"
        );
        assert_eq!(harness_version(Tool::Codex, "0.161.0"), None);
        assert_eq!(harness_version(Tool::Claude, "codex-cli 0.161.0"), None);
        assert_eq!(harness_version(Tool::Grok, "1.0.46"), None);
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn bare(dir: &Path) -> impl Fn(&mut std::process::Command) + '_ {
        move |cmd: &mut std::process::Command| {
            cmd.current_dir(dir);
        }
    }

    /// The launch-time probe: found on the launch's PATH, refused if it
    /// fails or answers as something else, and refused if the file changes
    /// while it is asked.
    #[test]
    fn the_launch_probe_trusts_only_an_unchanged_harness() {
        let dir = tempfile::tempdir().unwrap();
        let (cwd, path) = (dir.path(), Some(dir.path().as_os_str()));
        let at = |tool, program| at_launch(tool, program, cwd, path, bare(cwd));
        script(cwd, "codex", "echo codex-cli 0.161.0");
        let codex = at(Tool::Codex, "codex");
        assert_eq!(codex.version.as_deref(), Ok("0.161.0"));
        assert_eq!(
            codex.exe,
            Some(cwd.join("codex")),
            "the file the launch execs"
        );
        script(cwd, "claude", "echo v22.15.0");
        assert!(at(Tool::Claude, "claude").version.is_err(), "a runtime");
        script(cwd, "fails", "echo codex-cli 0.161.0; exit 3");
        assert!(at(Tool::Codex, "fails").version.is_err());
        assert!(at(Tool::Codex, "missing").version.is_err());
        let swaps = cwd.join("swaps");
        let body = format!(
            "echo codex-cli 0.161.0; cp {0} {0}.new && mv {0}.new {0}",
            swaps.display()
        );
        script(cwd, "swaps", &body);
        assert_eq!(
            at(Tool::Codex, "swaps").version,
            Err("its executable changed while its version was read".into())
        );
    }

    /// The version is asked in the launch's own directory and environment,
    /// so a shim that picks its harness by either answers for the one the
    /// launch will run.
    #[test]
    fn the_probe_runs_where_and_as_the_launch_will() {
        let bin = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        script(
            bin.path(),
            "claude",
            "if [ -f pinned ]; then echo '2.1.279 (Claude Code)'; \
             elif [ -n \"$PROFILE_OLD\" ]; then echo '2.1.278 (Claude Code)'; \
             else echo '2.1.280 (Claude Code)'; fi",
        );
        std::fs::write(repo.path().join("pinned"), "").unwrap();
        let path = Some(bin.path().as_os_str());
        let in_repo = at_launch(Tool::Claude, "claude", repo.path(), path, bare(repo.path()));
        assert_eq!(in_repo.version.as_deref(), Ok("2.1.279"), "its directory");
        let elsewhere = tempfile::tempdir().unwrap();
        let env = |cmd: &mut std::process::Command| {
            cmd.current_dir(elsewhere.path()).env("PROFILE_OLD", "1");
        };
        let profiled = at_launch(Tool::Claude, "claude", elsewhere.path(), path, env);
        assert_eq!(
            profiled.version.as_deref(),
            Ok("2.1.278"),
            "its environment"
        );
    }

    /// One budget covers the exit and the output: a child that outlives
    /// it, or an exited one whose descendant still holds the output, is
    /// cut off at the budget, its group killed.
    #[test]
    fn the_probe_is_bounded_however_its_output_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        for (name, body) in [
            ("hangs", "sleep 30"),
            ("holds", "echo codex-cli 0.161.0; sleep 6 & exit 0"),
        ] {
            script(cwd, name, body);
            let started = std::time::Instant::now();
            let launch = at_launch(Tool::Codex, name, cwd, Some(cwd.as_os_str()), bare(cwd));
            assert!(launch.version.is_err(), "{name}");
            assert!(
                started.elapsed() < PROBE_WITHIN + std::time::Duration::from_secs(1),
                "{name}: {:?}",
                started.elapsed()
            );
        }
    }
}
