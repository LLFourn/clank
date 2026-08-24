//! `clank attending <task-id>` — record the background task this agent
//! is waiting on.
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
    let record = crate::cli::stop_hook::Attending {
        task: task_id.to_string(),
        // One line, no control bytes: the record is read straight
        // into a terminal row. The renderer normalises too — records
        // can be hand-edited — but a clean write keeps the stored
        // value honest.
        desc: args
            .desc
            .as_deref()
            .map(|d| {
                d.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>()
            })
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
        pid: args.pid,
        // Taken NOW, while the pid is known to be the process the
        // caller meant. Read later it would identify whatever holds
        // the number by then, which is the reuse this exists to catch.
        token: args.pid.and_then(crate::proc_identity::token_for),
    };
    std::fs::write(&path, serde_json::to_string(&record)?)?;

    match args.pid {
        Some(pid) => {
            println!("attending `{task_id}` (pid {pid}) — nothing will wake you until it ends")
        }
        // Worth saying: without a pid `clank status` can show that the
        // wait exists but never that it has ended, so a finished wait
        // keeps reading as live until the next hook reaps it.
        None => println!(
            "attending `{task_id}` — nothing will wake you until it ends. Pass `--pid` to let \
             `clank status` show when it has ended."
        ),
    }
    Ok(())
}
