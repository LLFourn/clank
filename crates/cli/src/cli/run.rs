//! `clank run --desc "two words" -- <cmd>` — run a command under clank
//! so the Stop hook knows exactly when the work ends.
//!
//! Clank writes the attendance marker and then `exec`s the command,
//! replacing itself. One process does both jobs, which is what keeps
//! every property aligned: the pid in the marker is the pid the work
//! runs under, the harness's background task ends exactly when the
//! work ends, and stdio, exit status and signals are the command's own
//! because nothing is wrapping them.
//!
//! A spawn-and-wait supervisor would break the middle one. Killed
//! while its child kept running, the harness task would end with the
//! work still going — liveness lying in the direction that puts an
//! agent to sleep.

use super::{RunArgs, resolve_repo};
use crate::agent_store::agents_root;

/// `exec` is the whole design — same pid, same stdio, same exit
/// status — and it has no portable equivalent. Rather than silently
/// degrading to a supervisor with different lifetime semantics on a
/// platform we do not run on, say so.
#[cfg(not(unix))]
pub fn run(_args: RunArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`clank run` needs `exec` to replace itself with the command, which this platform does \
         not provide. Use `clank attending <task-id> --desc \"two words\" --pid <pid>` instead."
    )
}

#[cfg(unix)]
pub fn run(args: RunArgs) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let (program, rest) = args
        .command
        .split_first()
        .expect("clap requires at least one");
    let program = program.clone();
    let rest = rest.to_vec();

    // The marker exists BEFORE the exec, so it covers the whole life
    // of the work rather than starting some moment after it began.
    let marker = write_run_marker(&args)?;

    let err = std::process::Command::new(&program).args(&rest).exec();

    // Only reachable if the exec FAILED, so nothing is running and
    // nothing may claim to be attended.
    let _ = std::fs::remove_file(&marker);
    Err(anyhow::anyhow!("cannot run `{program}`: {err}"))
}

/// Write the attendance marker for this run and return its path.
///
/// The recorded pid is the pid the COMMAND runs under, because `exec`
/// replaces this process image without changing the process. That is
/// an OS guarantee and is documented here rather than tested: proving
/// it would need a child running clank's own code, which means
/// spawning the clank binary, which this repo bans. An earlier attempt
/// to dodge that with `pre_exec` was unsafe (post-`fork` allocation in
/// a threaded process), and its replacement was worse — it claimed to
/// distinguish exec from spawn while asserting something `spawn` does
/// identically. Confirmed by hand instead: a `clank run` of this
/// repo's suite recorded pid 80866, which `ps` showed as the cargo
/// process.
///
/// Split out so the lifecycle can be tested without spawning anything:
/// the exec that follows is a one-line tail with no branches of its
/// own, and everything worth asserting — that a marker exists before
/// the command starts, and that it records THIS process — happens
/// here (codex on 2f5a495).
fn write_run_marker(args: &RunArgs) -> anyhow::Result<std::path::PathBuf> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };

    let desc = crate::cli::attending::normalise_desc(&args.desc);
    if desc.is_empty() {
        anyhow::bail!(
            "--desc is empty; give TWO WORDS for what you are running, e.g. \
             `--desc \"test run\"`"
        );
    }

    // The identity comes FIRST. A marker clank cannot prove is worse
    // than no marker, so nothing is created — no directory, no file,
    // no exec — until the token exists (codex on c566eea).
    //
    // The pid recorded is OUR pid, and it stays correct across the
    // exec: `exec` replaces the process image, not the process. So
    // this is the pid the command itself will run under.
    let pid = std::process::id() as i32;
    let Some(token) = crate::proc_identity::token_for(pid) else {
        anyhow::bail!(
            "cannot take a process identity for pid {pid} on this platform, so the marker could \
             not be trusted and nothing has been recorded"
        );
    };

    let record = crate::cli::stop_hook::Attending {
        // The description is the correlator. There is no harness task
        // id to record — the tool assigns one only after launching
        // this command, so it cannot exist yet.
        task: desc.clone(),
        desc: Some(desc),
        pid: Some(pid),
        token: Some(token),
        // This marker names a DESCRIPTION, so only a live `clank run`
        // carrying it may satisfy it — never a task id that happens to
        // read the same.
        correlation: crate::cli::stop_hook::Correlation::RunDesc,
    };
    let dir = agents_root(&repo).join(label.as_str());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("attending");
    std::fs::write(&path, serde_json::to_string(&record)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct T {
        #[command(flatten)]
        a: RunArgs,
    }

    #[test]
    fn a_description_and_a_command_are_both_required() {
        assert!(T::try_parse_from(["t", "--desc", "test run", "--", "echo", "hi"]).is_ok());
        // The description is the correlator, so a run without one could
        // never be recognised among the harness's live tasks.
        assert!(
            T::try_parse_from(["t", "--", "echo", "hi"]).is_err(),
            "--desc required"
        );
        assert!(
            T::try_parse_from(["t", "--desc", "test run"]).is_err(),
            "a command is required — there is nothing to attend otherwise"
        );
    }

    /// Everything after `--` belongs to the command, flags included.
    /// Without this a run of `cargo test --release` would have its
    /// flags eaten by clank's own parser.
    #[test]
    fn the_command_keeps_its_own_flags() {
        let t = T::try_parse_from([
            "t",
            "--desc",
            "test run",
            "--",
            "cargo",
            "test",
            "--release",
            "--desc",
            "not-ours",
        ])
        .expect("parses");
        assert_eq!(
            t.a.command,
            vec!["cargo", "test", "--release", "--desc", "not-ours"]
        );
        assert_eq!(
            t.a.desc, "test run",
            "clank's own --desc is the one before --"
        );
    }

    fn repo_with_agent() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank/agents/claude")).unwrap();
        dir
    }

    fn run_args(desc: &str, cmd: &[&str], repo: &std::path::Path) -> RunArgs {
        RunArgs {
            desc: desc.to_string(),
            author: Some("claude".into()),
            repo: Some(repo.to_path_buf()),
            command: cmd.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The marker is on disk BEFORE the command starts, and it records
    /// THIS process — which is the pid the command will run under,
    /// because `exec` replaces the image and not the process. That
    /// identity is the whole reason to launch work this way rather
    /// than asking an agent to find a pid for it.
    #[test]
    fn the_marker_exists_before_the_command_and_records_this_process() {
        let dir = repo_with_agent();
        let path = write_run_marker(&run_args("test run", &["echo", "hi"], dir.path()))
            .expect("marker written");
        assert!(path.exists(), "written before anything is exec'd");

        let rec: crate::cli::stop_hook::Attending =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            rec.pid,
            Some(std::process::id() as i32),
            "the recorded pid is ours, and stays ours across the exec"
        );
        assert!(
            rec.token.is_some(),
            "with the identity that makes it usable"
        );
        assert_eq!(rec.correlation, crate::cli::stop_hook::Correlation::RunDesc);
        assert!(
            rec.provably_live(&[], 1),
            "so the hook can prove it without the caller supplying anything"
        );
    }

    /// A command that cannot start leaves nothing claiming to be
    /// attended. `run` reaches its tail ONLY when the exec failed, so
    /// this exercises the real failure path rather than a simulation
    /// of it.
    #[cfg(unix)]
    #[test]
    fn a_command_that_cannot_start_returns_nonzero_and_leaves_no_marker() {
        let dir = repo_with_agent();
        let marker = dir.path().join(".clank/agents/claude/attending");
        let err = run(run_args(
            "bad command",
            &["/nonexistent/binary-clank-test"],
            dir.path(),
        ))
        .expect_err("a missing program cannot be exec'd");
        assert!(
            err.to_string().contains("/nonexistent/binary-clank-test"),
            "names what could not be run: {err}"
        );
        assert!(
            !marker.exists(),
            "and nothing is left claiming to attend work that never started"
        );
    }

    /// An unusable description is refused before any directory or file
    /// is created — the marker must never exist in a state clank
    /// cannot later prove.
    #[test]
    fn a_blank_description_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_run_marker(&run_args("   ", &["echo"], dir.path())).is_err());
        assert!(
            !dir.path().join(".clank/agents/claude/attending").exists(),
            "no marker"
        );
    }

    /// The marker records the description as its TASK, not a harness
    /// id, because no id exists yet: the tool assigns one only after
    /// launching this command. The description is what the hook can
    /// recognise on the recorded command line (codex on d172ced).
    #[test]
    fn the_task_is_the_description_because_no_harness_id_exists_yet() {
        let desc = crate::cli::attending::normalise_desc("  test run  ");
        let pid = std::process::id() as i32;
        let rec = crate::cli::stop_hook::Attending {
            task: desc.clone(),
            desc: Some(desc),
            pid: Some(pid),
            token: crate::proc_identity::token_for(pid),
            correlation: crate::cli::stop_hook::Correlation::RunDesc,
        };
        assert_eq!(rec.task, "test run");
        assert_eq!(rec.desc.as_deref(), Some("test run"));
        // Provable on the spot from ONE live run carrying it — which
        // is the whole point of clank supplying the pid rather than
        // asking for it. A task id reading the same does NOT satisfy
        // it: this marker names a description.
        assert!(rec.provably_live(&[], 1));
        assert!(!rec.provably_live(&["test run".to_string()], 0));
    }
}
