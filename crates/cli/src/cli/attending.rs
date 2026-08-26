//! `clank attending <task-id> --desc "two words"` — record the
//! background task this agent is waiting on, and what it IS.
//!
//! While the task is live NOTHING wakes this agent. The marker is a
//! POINTER, not a claim: the Stop hook validates the recorded id
//! against the tool's live task list and discards it the moment that
//! task is gone, so the silence lasts exactly as long as the work and
//! a forgotten marker cannot extend it. That is why nothing here has
//! to be cleaned up to stay correct — `--clear` is a convenience, not
//! a requirement.

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
    if args.pid.is_some_and(|p| p <= 0) {
        anyhow::bail!("--pid must be a real process id");
    }
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
        pid: args.pid,
        // Taken NOW, while the pid is known to be the process the
        // caller meant. Read later it would identify whatever holds
        // the number by then, which is the reuse this exists to catch.
        token: args.pid.and_then(crate::proc_identity::token_for),
    };
    std::fs::write(&path, serde_json::to_string(&record)?)?;
    println!("{}", confirmation(&desc, task_id, args.pid));
    Ok(())
}

/// What the caller sees on a successful record. It leads with the
/// DESCRIPTION because that is the recorded fact the caller can check
/// — a task id echoed back proves only that argv arrived intact,
/// while a description read back is the caller's chance to notice the
/// row will say something they did not mean.
fn confirmation(desc: &str, task_id: &str, pid: Option<i32>) -> String {
    match pid {
        Some(pid) => format!(
            "attending \"{desc}\" (`{task_id}`, pid {pid}) — nothing will wake you until it ends"
        ),
        // Worth saying: without a pid `clank status` can show that the
        // wait exists but never that it has ended, so a finished wait
        // keeps reading as live until the next hook reaps it.
        None => format!(
            "attending \"{desc}\" (`{task_id}`) — nothing will wake you until it ends. Pass \
             `--pid` to let `clank status` show when it has ended."
        ),
    }
}

/// Collapse a description to the single terminal row it will be drawn
/// as: first line only, control bytes dropped, trimmed. The renderer
/// normalises too — records can be hand-edited — but a clean write
/// keeps the stored value honest.
fn normalise_desc(raw: &str) -> String {
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
            let err = run(args(Some("br9711ewy"), Some(blank), dir.path()))
                .await
                .expect_err("refused");
            assert!(
                err.to_string().contains("--desc"),
                "says which flag is at fault: {err}"
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
        run(args(Some("br9711ewy"), Some("  test run  "), dir.path()))
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
        run(args(Some("br9711ewy"), Some(&long), dir.path()))
            .await
            .unwrap();
        assert_eq!(
            record(dir.path()).and_then(|r| r.desc).as_deref(),
            Some(long.trim())
        );
    }

    /// The confirmation is the caller's only sight of what was
    /// actually stored — the same String that went into the record,
    /// so a description mangled by normalisation shows up HERE rather
    /// than in the TUI row hours later.
    #[test]
    fn the_confirmation_names_the_wait_not_just_the_task_id() {
        for pid in [None, Some(41293)] {
            let line = confirmation("test run", "br9711ewy", pid);
            assert!(line.contains("test run"), "names the wait: {line}");
            assert!(line.contains("br9711ewy"), "and the task id: {line}");
        }
        let with_pid = confirmation("test run", "br9711ewy", Some(41293));
        assert!(with_pid.contains("pid 41293"), "and the pid: {with_pid}");
    }

    #[tokio::test]
    async fn clearing_needs_no_description() {
        let dir = tempfile::tempdir().unwrap();
        run(args(Some("br9711ewy"), Some("test run"), dir.path()))
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
