//! `clank attending <task-id>` — record the background task this agent
//! is waiting on.
//!
//! The marker is a POINTER, not a claim. The Stop hook validates the
//! recorded id against the tool's live task list and discards it the
//! moment that task is gone, so a forgotten marker cannot silence the
//! hook (attending-suppresses-standing-wakes). That is why nothing
//! here has to be cleaned up to stay correct — `--clear` is a
//! convenience, not a requirement.

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
    // Capture WHAT is being acknowledged, not just the task. A
    // reviewer's CONTINUE on a new commit reuses the same
    // `gate_continue` reason at a new sha, so suppressing by reason
    // alone would swallow real review progress; only this exact item
    // is suppressed.
    let role = crate::agent_store::resolve_role(&repo, &label)?;
    let pending = crate::cli::stop_hook::peek_items(&repo, &label, role)
        .await
        .unwrap_or_default();
    let standing = pending.iter().find(|i| {
        i.kind.as_deref() == Some("master")
            && i.reason.as_deref() == Some(clank_core::vocab::WaitingReason::GateContinue.as_str())
    });

    let record = crate::cli::stop_hook::Attending {
        task: task_id.to_string(),
        plan: standing.and_then(|i| i.plan.clone()),
        sha: standing.and_then(|i| i.sha.clone()),
    };
    std::fs::write(&path, serde_json::to_string(&record)?)?;

    match (&record.plan, &record.sha) {
        (Some(plan), Some(sha)) => println!(
            "attending `{task_id}` — `continue {plan} @ {short}` will not wake you again while \
             it runs; anything new still will",
            short = &sha[..sha.len().min(12)]
        ),
        _ => println!(
            "attending `{task_id}` — nothing standing to acknowledge right now, so all work \
             still wakes you"
        ),
    }
    Ok(())
}
