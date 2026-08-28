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
    println!("{}", confirmation(&record));
    Ok(())
}

/// What the caller sees on a successful record.
///
/// Derived from the RECORD, never from the arguments, so it cannot
/// promise something other than what was stored. It leads with the
/// description because that is the fact the caller can check — a task
/// id echoed back proves only that argv arrived intact.
///
/// Crucially it says whether the marker can SUPPRESS anything. The
/// hook silences a turn only on proof the work is still running (a
/// live task id, a live pid, and a token that still holds it), so a
/// marker missing any of that is recorded and then ignored. A caller
/// silently getting a no-op is how this whole class of bug stayed
/// invisible (codex on 8be3184).
fn confirmation(rec: &crate::cli::stop_hook::Attending) -> String {
    let desc = rec.desc.as_deref().unwrap_or_default();
    let task = &rec.task;
    match (rec.pid, rec.token.is_some()) {
        // Still conditional: the tool must ALSO be listing the task as
        // live when the turn ends. Promising more than that is what
        // the previous wording got wrong.
        (Some(pid), true) => format!(
            "attending \"{desc}\" (`{task}`, pid {pid}) — your next turn-end stays silent for \
             as long as the tool still reports this task running"
        ),
        (Some(pid), false) => format!(
            "attending \"{desc}\" (`{task}`, pid {pid}) — RECORDED, BUT IT WILL NOT SILENCE \
             ANYTHING: no process token could be taken for pid {pid}, so clank cannot prove that \
             process is still the one you meant"
        ),
        (None, _) => format!(
            "attending \"{desc}\" (`{task}`) — RECORDED, BUT IT WILL NOT SILENCE ANYTHING: \
             without `--pid` clank cannot prove the work is still running, and silencing a turn \
             with no provable wake is how an agent goes to sleep for good"
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

    fn rec(pid: Option<i32>, token: bool) -> crate::cli::stop_hook::Attending {
        crate::cli::stop_hook::Attending {
            task: "br9711ewy".into(),
            desc: Some("test run".into()),
            pid,
            token: token.then(|| {
                crate::proc_identity::token_for(std::process::id() as i32)
                    .expect("this platform answers for its own pid")
            }),
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

    /// A marker that cannot suppress must SAY it cannot. The hook
    /// silences only on proof the work is still running, so every
    /// other shape is recorded and then ignored — and a caller who is
    /// not told has no way to discover it except by being nudged
    /// anyway and not knowing why (codex on 8be3184).
    #[test]
    fn only_a_marker_with_a_verified_identity_claims_it_can_silence() {
        let silences = |r: &crate::cli::stop_hook::Attending| {
            let line = confirmation(r);
            let refuses = line.contains("WILL NOT SILENCE");
            let claims = line.contains("stays silent");
            assert_ne!(refuses, claims, "must say exactly one of the two: {line}");
            claims
        };

        assert!(
            !silences(&rec(None, false)),
            "no pid: nothing can prove the work is running"
        );
        assert!(
            !silences(&rec(Some(41293), false)),
            "a pid with no token is not an identity — pids are reused"
        );
        assert!(
            silences(&rec(Some(41293), true)),
            "pid plus a token is the only shape entitled to claim it"
        );

        // And the claim it does make stays conditional: the tool has
        // to be listing the task live at turn-end too.
        let full = confirmation(&rec(Some(41293), true));
        assert!(
            full.contains("as long as") && full.contains("running"),
            "the promise is conditional, not absolute: {full}"
        );
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
