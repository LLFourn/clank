//! `clank attending <task-id> --desc "two words"` — record the
//! background task this agent is waiting on, and what it IS.
//!
//! The marker buys ONE quiet turn-end. The Stop hook consumes it on
//! the turn that reads it, so it cannot outlive that turn and cannot
//! go stale — which is why nothing here has to be cleaned up to stay
//! correct, and `--clear` is a convenience rather than a requirement.
//!
//! It only silences that turn if the attendance can PROVE a wake is
//! still coming: the task listed live by the tool, a pid that is
//! alive, and a `ProcToken` that still holds it. Silence hands the
//! wake channel to the task's completion notification, and that
//! notification only fires while the task is really running — so
//! suppressing without proof leaves the agent with no channel at all,
//! which is a permanent sleep rather than a missed nudge.
//!
//! A marker that cannot prove itself — no `--pid`, no token, or a
//! task the tool no longer lists — is still consumed, and the turn
//! parks as usual. Noise, never silence.

use super::{AttendingArgs, resolve_repo};
use crate::agent_store::agents_root;

pub async fn run(args: AttendingArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let label = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    let dir = agents_root(&repo).join(label.as_str());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("attending");

    if args.clear {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("no longer attending"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("was not attending anything")
            }
            Err(e) => return Err(e.into()),
        }
        return Ok(());
    }

    let Some(task_id) = args.task_id.as_deref() else {
        anyhow::bail!("give a task id to attend, or `--clear` to stop attending");
    };
    let task_id = task_id.trim();
    if task_id.is_empty() {
        anyhow::bail!("task id is empty");
    }
    // A marker may only exist if clank can later determine whether the
    // work has FINISHED. Without that it is not a wait, it is a claim
    // nobody can check — recorded, ignored by the hook, and shown by
    // `clank status` as a row that ages forever because nothing can
    // ever learn it ended.
    //
    // Refusing beats warning. A caller told "this will not suppress"
    // has still been handed a useless record, and the useful thing to
    // hand them instead is the command that works.
    let Some(pid) = args.pid else {
        anyhow::bail!(
            "--pid is required: without it clank cannot tell when this work ends, so the marker \
             would suppress nothing.\n\nEasier: let clank supply it — `clank run --desc \"two \
             words\" -- <command>` records the marker and becomes the command, so the pid is \
             right by construction."
        );
    };
    if pid <= 0 {
        anyhow::bail!("--pid must be a real process id");
    }
    // A pid is a number the OS reuses. The token is what says WHICH
    // process held it, so a marker without one identifies nothing.
    let Some(token) = crate::proc_identity::token_for(pid) else {
        anyhow::bail!(
            "cannot identify pid {pid}: it may have already exited, or this platform cannot \
             answer. A pid alone is not an identity — the OS reuses them — so the marker would \
             not be trustworthy."
        );
    };
    // clap requires `--desc` unless `--clear`; what it cannot require
    // is that anything legible SURVIVES normalisation. `--desc "  "`
    // parses fine and names nothing, and a wait nobody can identify
    // is the failure this flag exists to prevent — so refuse it here
    // rather than store a record the TUI can only label with an
    // opaque task id.
    let desc = normalise_desc(args.desc.as_deref().unwrap_or_default());
    if desc.is_empty() {
        anyhow::bail!(
            "--desc is empty; give TWO WORDS for what you are waiting on, e.g. \
             `--desc \"test run\"`"
        );
    }
    let record = crate::cli::stop_hook::Attending {
        task: task_id.to_string(),
        desc: Some(desc.clone()),
        pid: Some(pid),
        // Taken NOW, while the pid is known to be the process the
        // caller meant. Read later it would identify whatever holds
        // the number by then, which is the reuse this exists to catch.
        token: Some(token),
        // A hand-supplied marker names a harness TASK ID, so only a
        // live task with that id may satisfy it.
        correlation: crate::cli::stop_hook::Correlation::TaskId,
    };
    std::fs::write(&path, serde_json::to_string(&record)?)?;
    println!("{}", confirmation(&record));
    Ok(())
}

/// What the caller sees on a successful record.
///
/// Derived from the RECORD, never from the arguments, so it cannot
/// promise something other than what was stored.
///
/// There is one shape to describe: admission refuses anything clank
/// cannot verify, so a marker that exists is one that can suppress.
/// The promise stays CONDITIONAL even so — the hook also requires the
/// tool to still be reporting the work as live at turn-end, and
/// claiming more than that is what the previous wording got wrong.
fn confirmation(rec: &crate::cli::stop_hook::Attending) -> String {
    let desc = rec.desc.as_deref().unwrap_or_default();
    let task = &rec.task;
    let pid = rec.pid.map(|p| p.to_string()).unwrap_or_default();
    format!(
        "attending \"{desc}\" (`{task}`, pid {pid}) — your next turn-end stays silent for as \
         long as the tool still reports this work running"
    )
}

/// Collapse a description to the single terminal row it will be drawn
/// as: first line only, control bytes dropped, trimmed. The renderer
/// normalises too — records can be hand-edited — but a clean write
/// keeps the stored value honest.
pub(crate) fn normalise_desc(raw: &str) -> String {
    raw.lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(task: Option<&str>, desc: Option<&str>, repo: &std::path::Path) -> AttendingArgs {
        AttendingArgs {
            task_id: task.map(str::to_string),
            pid: None,
            desc: desc.map(str::to_string),
            clear: false,
            author: Some("claude".into()),
            repo: Some(repo.to_path_buf()),
        }
    }

    /// Args that pass admission: a real, identifiable pid. Most tests
    /// are about something else and should not have to restate why a
    /// marker is admissible.
    fn ok_args(task: Option<&str>, desc: Option<&str>, repo: &std::path::Path) -> AttendingArgs {
        AttendingArgs {
            pid: Some(std::process::id() as i32),
            ..args(task, desc, repo)
        }
    }

    fn record(repo: &std::path::Path) -> Option<crate::cli::stop_hook::Attending> {
        let raw = std::fs::read_to_string(repo.join(".clank/agents/claude/attending")).ok()?;
        Some(serde_json::from_str(&raw).expect("a written record parses"))
    }

    #[test]
    fn a_description_is_required_unless_you_are_clearing() {
        use clap::Parser;
        #[derive(Parser, Debug)]
        struct T {
            #[command(flatten)]
            a: AttendingArgs,
        }
        let err = T::try_parse_from(["t", "br9711ewy"]).expect_err("--desc is required");
        assert!(
            err.to_string().contains("--desc"),
            "the error names the missing flag: {err}"
        );
        assert!(T::try_parse_from(["t", "br9711ewy", "--desc", "test run"]).is_ok());
        // Clearing names no wait, so it needs no description.
        assert!(T::try_parse_from(["t", "--clear"]).is_ok());
    }

    #[tokio::test]
    async fn a_description_that_normalises_to_nothing_is_refused_and_writes_no_record() {
        // Each of these parses fine and names nothing.
        for blank in ["   ", "\u{7}\u{1b}", "\n first line lost"] {
            let dir = tempfile::tempdir().unwrap();
            // An admissible pid, so this exercises the DESCRIPTION
            // rule and not the identity one — without it the pid check
            // fires first and the assertion below passes on the wrong
            // error, since that message also mentions `--desc`.
            let err = run(ok_args(Some("br9711ewy"), Some(blank), dir.path()))
                .await
                .expect_err("refused");
            assert!(
                err.to_string().contains("--desc is empty"),
                "says which flag is at fault, and why: {err}"
            );
            assert!(
                record(dir.path()).is_none(),
                "and no record survives the refusal: {blank:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_recorded_wait_carries_a_subject() {
        let dir = tempfile::tempdir().unwrap();
        run(ok_args(Some("br9711ewy"), Some("  test run  "), dir.path()))
            .await
            .unwrap();
        let rec = record(dir.path()).expect("written");
        assert_eq!(rec.desc.as_deref(), Some("test run"), "normalised, not raw");
        assert_eq!(rec.task, "br9711ewy");
    }

    /// Storage is width-independent: the pane the record will be drawn
    /// in is not known at write time, and may be resized afterwards.
    /// Truncation is the renderer's job, so an over-long description
    /// is stored WHOLE — never rejected, never cut on the way in.
    #[tokio::test]
    async fn an_over_long_description_is_stored_whole() {
        let dir = tempfile::tempdir().unwrap();
        let long = "cargo nextest run across every crate in the workspace ".repeat(8);
        run(ok_args(Some("br9711ewy"), Some(&long), dir.path()))
            .await
            .unwrap();
        assert_eq!(
            record(dir.path()).and_then(|r| r.desc).as_deref(),
            Some(long.trim())
        );
    }

    fn rec(pid: Option<i32>, token: bool) -> crate::cli::stop_hook::Attending {
        crate::cli::stop_hook::Attending {
            task: "br9711ewy".into(),
            desc: Some("test run".into()),
            pid,
            token: token.then(|| {
                crate::proc_identity::token_for(std::process::id() as i32)
                    .expect("this platform answers for its own pid")
            }),
            correlation: crate::cli::stop_hook::Correlation::TaskId,
        }
    }

    /// The confirmation is the caller's only sight of what was
    /// actually stored — the same values that went into the record,
    /// so a description mangled by normalisation shows up HERE rather
    /// than in the TUI row hours later.
    #[test]
    fn the_confirmation_names_the_wait_not_just_the_task_id() {
        for r in [
            rec(None, false),
            rec(Some(41293), false),
            rec(Some(41293), true),
        ] {
            let line = confirmation(&r);
            assert!(line.contains("test run"), "names the wait: {line}");
            assert!(line.contains("br9711ewy"), "and the task id: {line}");
        }
        let with_pid = confirmation(&rec(Some(41293), true));
        assert!(with_pid.contains("pid 41293"), "and the pid: {with_pid}");
    }

    /// Admission REFUSES what it cannot verify, rather than recording
    /// it and warning. A caller told "this will not suppress" still
    /// holds a useless record; refusing hands them the working command
    /// instead. This is the shape the penlock marker had.
    #[tokio::test]
    async fn a_wait_clank_cannot_verify_is_refused_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(args(Some("br9711ewy"), Some("test run"), dir.path()))
            .await
            .expect_err("no --pid: refused");
        let msg = err.to_string();
        assert!(msg.contains("--pid"), "names what is missing: {msg}");
        assert!(
            msg.contains("clank run"),
            "and points at the command that supplies it: {msg}"
        );
        assert!(
            record(dir.path()).is_none(),
            "nothing unverifiable reaches disk"
        );
    }

    #[tokio::test]
    async fn a_pid_that_cannot_be_identified_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing can hold this, so no token can be taken for it.
        let err = run(AttendingArgs {
            pid: Some(999_999_998),
            ..args(Some("br9711ewy"), Some("test run"), dir.path())
        })
        .await
        .expect_err("unidentifiable pid: refused");
        assert!(
            err.to_string().contains("identity") || err.to_string().contains("identify"),
            "says why a bare pid is not enough: {err}"
        );
        assert!(record(dir.path()).is_none());
    }

    #[tokio::test]
    async fn a_verifiable_wait_is_recorded_with_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i32;
        run(AttendingArgs {
            pid: Some(me),
            ..args(Some("br9711ewy"), Some("test run"), dir.path())
        })
        .await
        .unwrap();
        let rec = record(dir.path()).expect("written");
        assert_eq!(rec.pid, Some(me));
        assert!(rec.token.is_some(), "and the identity that makes it usable");
        assert!(
            rec.provably_live(&["br9711ewy".to_string()], 0),
            "so it satisfies the hook's proof"
        );
    }

    #[tokio::test]
    async fn clearing_needs_no_description() {
        let dir = tempfile::tempdir().unwrap();
        run(ok_args(Some("br9711ewy"), Some("test run"), dir.path()))
            .await
            .unwrap();
        run(AttendingArgs {
            clear: true,
            ..args(None, None, dir.path())
        })
        .await
        .unwrap();
        assert!(record(dir.path()).is_none(), "the record is gone");
    }
}
