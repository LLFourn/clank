use std::path::{Path, PathBuf};

use super::{BlockArgs, BlockCmd, BlockCreateArgs, BlockMigrateArgs, UnblockArgs, resolve_repo};
use crate::agent_store::agents_root;

pub async fn run(args: BlockArgs) -> anyhow::Result<()> {
    match args.command {
        BlockCmd::Create(a) => run_create(a).await,
        BlockCmd::Clean(a) => run_clean(a).await,
        BlockCmd::Migrate(a) => run_migrate(a).await,
    }
}

/// Flatten `blocks/<plan>/<name>.md` to `blocks/<name>.md`.
///
/// A legacy plan-scoped block is INVISIBLE to the flat reader, so
/// leaving one behind silently drops a pending question rather than
/// parking on it. That cuts both ways: a block whose plan is gone must
/// be reported as it is dropped, never removed quietly — the human is
/// entitled to see the question they are losing.
///
/// A question and its answer move as ONE unit, and the whole plan is
/// preflighted before anything is touched. Migrating the two halves
/// independently can land an answer flat while its question stays
/// scoped, which silently marks an UNRELATED flat question answered —
/// the fabricated-answer failure this layout change exists to prevent.
async fn run_migrate(args: BlockMigrateArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let root = agents_root(&repo);
    let Ok(agents) = std::fs::read_dir(&root) else {
        println!("no agents dir; nothing to migrate");
        return Ok(());
    };

    let active = |plan: &str| repo.join(crate::init_facts::plan_md_rel(plan)).exists();
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    // Destinations claimed by moves planned SO FAR. Checking only the
    // filesystem misses collisions between two planned moves: two
    // active plans each holding `q.md` both see no flat `q.md`, both
    // queue the same destination, and the apply loop silently
    // overwrites the first with the second.
    let mut reserved: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut drops: Vec<(PathBuf, String)> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();

    let mut agent_dirs: Vec<PathBuf> = agents
        .flatten()
        .filter(|a| a.file_type().is_ok_and(|t| t.is_dir()))
        .map(|a| a.path())
        .collect();
    agent_dirs.sort();

    for agent in agent_dirs {
        let blocks = agent.join("blocks");
        let unblocks = agent.join("unblocks");
        let mut plan_dirs: Vec<PathBuf> = std::fs::read_dir(&blocks)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
        plan_dirs.sort();

        for dir in plan_dirs {
            let Some(plan) = dir.file_name().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|s| s.strip_suffix(".md"))
                        .map(str::to_owned)
                })
                .collect();
            names.sort();

            for name in names {
                let src_q = dir.join(format!("{name}.md"));
                let src_a = unblocks.join(&plan).join(format!("{name}.md"));
                let dst_q = blocks.join(format!("{name}.md"));
                let dst_a = unblocks.join(format!("{name}.md"));

                if !active(&plan) {
                    drops.push((src_q.clone(), format!("blocks/{plan}/{name}.md")));
                    if src_a.exists() {
                        drops.push((src_a, format!("unblocks/{plan}/{name}.md")));
                    }
                    continue;
                }
                // The destination PAIR must be entirely free, on disk
                // AND among the moves already planned. A flat answer
                // sitting at `dst_a` would attach to this question the
                // moment it lands.
                let taken = dst_q.exists()
                    || dst_a.exists()
                    || reserved.contains(&dst_q)
                    || reserved.contains(&dst_a);
                if taken {
                    conflicts.push(format!(
                        "`{name}` (agent `{}`, plan `{plan}`) — another block or answer already \
                         claims that flat name",
                        agent.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                    ));
                    continue;
                }
                // Both halves are reserved even when only the question
                // exists, so a later plan's ANSWER cannot claim `dst_a`.
                reserved.insert(dst_q.clone());
                reserved.insert(dst_a.clone());
                moves.push((src_q, dst_q));
                if src_a.exists() {
                    moves.push((src_a, dst_a));
                }
            }
        }
    }

    for (src, dst) in &moves {
        println!("move {} -> {}", src.display(), dst.display());
    }
    for (path, label) in &drops {
        let body = std::fs::read_to_string(path).unwrap_or_default();
        let first = body.lines().next().unwrap_or("").trim();
        println!("DROP {label} — its plan is not active\n     {first}");
    }
    for c in &conflicts {
        println!("CONFLICT {c}");
    }

    if !conflicts.is_empty() {
        anyhow::bail!(
            "{} name collision(s); nothing was changed — resolve them by hand and re-run",
            conflicts.len()
        );
    }

    let verb = if args.dry { "would " } else { "" };
    println!("{verb}move {}, {verb}drop {}", moves.len(), drops.len());
    if args.dry {
        return Ok(());
    }

    for (src, dst) in &moves {
        std::fs::rename(src, dst)?;
    }
    for (path, _) in &drops {
        std::fs::remove_file(path)?;
    }
    // Leaving the emptied `<plan>/` dirs behind would make a migrated
    // repo look unmigrated on the next run.
    for kind in ["blocks", "unblocks"] {
        for agent in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            for sub in std::fs::read_dir(agent.path().join(kind))
                .into_iter()
                .flatten()
                .flatten()
            {
                if sub.file_type().is_ok_and(|t| t.is_dir()) {
                    let _ = std::fs::remove_dir(sub.path());
                }
            }
        }
    }
    Ok(())
}

async fn run_create(args: BlockCreateArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let author = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };

    // Every block halts the whole repo until it is answered; say so
    // on stderr, which survives the caller capturing stdout.
    eprintln!(
        "BLOCK: this parks every wait item for `{}` until the block is answered",
        author.as_str()
    );
    let dir = agents_root(&repo).join(author.as_str()).join("blocks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", args.name));
    std::fs::write(&path, &args.message)?;

    let config = crate::cli::config::load(&repo);
    if let Some(Some(cmd)) = config.hooks.get(&clank_core::HookEvent::Blocked) {
        let mut child = std::process::Command::new("sh");
        child
            .arg("-c")
            .arg(cmd)
            .env("CLANK_EVENT", "blocked")
            .env("CLANK_AGENT", author.as_str())
            .env("CLANK_BLOCK_NAME", &args.name)
            .env("CLANK_REPO", &*repo.to_string_lossy())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        let _ = child.status();
    }

    println!(
        "blocked {} as {}",
        author.as_str(),
        path.strip_prefix(&repo).unwrap_or(&path).display()
    );
    Ok(())
}

pub async fn run_unblock(args: UnblockArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;

    let dir = agents_root(&repo).join(&args.agent).join("unblocks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", args.name));
    std::fs::write(&path, &args.message)?;

    println!(
        "unblocked {} as {}",
        args.agent,
        path.strip_prefix(&repo).unwrap_or(&path).display()
    );
    Ok(())
}

async fn run_clean(args: super::BlockCleanArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let author = crate::agent_env::resolve_identity_from_env(&repo)?;
    let entries = scan_blocks(&repo);
    let agents = agents_root(&repo);
    let mut removed = 0;
    for entry in &entries {
        if entry.agent != author.as_str() || entry.answer.is_none() {
            continue;
        }
        let block_path = agents
            .join(&entry.agent)
            .join("blocks")
            .join(format!("{}.md", entry.name));
        let unblock_path = agents
            .join(&entry.agent)
            .join("unblocks")
            .join(format!("{}.md", entry.name));
        if block_path.exists() {
            std::fs::remove_file(&block_path)?;
            removed += 1;
        }
        if unblock_path.exists() {
            std::fs::remove_file(&unblock_path)?;
            removed += 1;
        }
    }
    println!("cleaned {removed} file(s)");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct BlockEntry {
    pub agent: String,
    pub name: String,
    pub question: String,
    pub answer: Option<String>,
}

pub fn scan_blocks(repo: &Path) -> Vec<BlockEntry> {
    let root = agents_root(repo);
    let Ok(agents) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in agents.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(agent) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let blocks_dir = entry.path().join("blocks");
        let unblocks_dir = entry.path().join("unblocks");

        scan_blocks_in_dir(&blocks_dir, &unblocks_dir, &agent, &mut out);
    }
    out.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.name.cmp(&b.name)));
    out
}

fn scan_blocks_in_dir(
    blocks_dir: &Path,
    unblocks_dir: &Path,
    agent: &str,
    out: &mut Vec<BlockEntry>,
) {
    let Ok(files) = std::fs::read_dir(blocks_dir) else {
        return;
    };
    for file in files.flatten() {
        let fname = file.file_name();
        let Some(s) = fname.to_str() else { continue };
        let Some(name) = s.strip_suffix(".md") else {
            continue;
        };
        if file.file_type().map_or(true, |t| !t.is_file()) {
            continue;
        }
        let question = std::fs::read_to_string(file.path()).unwrap_or_default();
        let unblock_path = unblocks_dir.join(s);
        let answer = std::fs::read_to_string(&unblock_path).ok();
        out.push(BlockEntry {
            agent: agent.to_string(),
            name: name.to_string(),
            question,
            answer,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn scan_finds_repo_block() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".clank/agents/alice/blocks/fix-api.md"),
            "What should the return type be?",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].agent, "alice");
        assert_eq!(blocks[0].name, "fix-api");
        assert_eq!(blocks[0].question, "What should the return type be?");
        assert!(blocks[0].answer.is_none());
    }

    #[test]
    fn scan_ignores_legacy_plan_scoped_subdir() {
        // The reader is flat-only. A pre-migration `blocks/<plan>/`
        // file is therefore INVISIBLE — which is exactly why the
        // migration is mandatory rather than optional: leaving one
        // behind silently drops a pending question instead of
        // parking on it.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path()
                .join(".clank/agents/bob/blocks/auth/need-creds.md"),
            "Which OAuth provider?",
        );
        assert!(
            scan_blocks(dir.path()).is_empty(),
            "legacy plan-scoped blocks must not be read; migrate them"
        );
    }

    #[test]
    fn scan_includes_answer_when_unblock_exists() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".clank/agents/alice/blocks/fix-api.md"),
            "What type?",
        );
        write(
            &dir.path().join(".clank/agents/alice/unblocks/fix-api.md"),
            "Use Result<T>",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].answer.as_deref(), Some("Use Result<T>"));
    }

    #[tokio::test]
    async fn pause_round_trips_through_exactly_one_flat_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::cli::block::run(crate::cli::BlockArgs {
            command: crate::cli::BlockCmd::Create(crate::cli::BlockCreateArgs {
                name: "pause".into(),
                message: "held for review".into(),
                author: Some("claude".into()),
                repo: Some(root.to_path_buf()),
            }),
        })
        .await
        .unwrap();

        let pending = scan_blocks(root);
        assert_eq!(pending.len(), 1, "exactly one pause block");
        assert_eq!(pending[0].name, "pause");
        assert!(pending[0].answer.is_none());
        assert!(
            root.join(".clank/agents/claude/blocks/pause.md").exists(),
            "written FLAT, where the reader looks"
        );

        run_unblock(crate::cli::UnblockArgs {
            agent: "claude".into(),
            name: "pause".into(),
            message: "carry on".into(),
            repo: Some(root.to_path_buf()),
        })
        .await
        .unwrap();

        let after = scan_blocks(root);
        assert_eq!(after.len(), 1, "the pair stays, now answered");
        assert_eq!(after[0].answer.as_deref(), Some("carry on"));
        assert!(
            root.join(".clank/agents/claude/unblocks/pause.md").exists(),
            "the answer must land flat too, or it never pairs"
        );
    }

    #[tokio::test]
    async fn answering_one_block_leaves_every_other_pending() {
        // The repo stays parked until each question is answered
        // individually; one answer must never discharge the rest.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (agent, name, q) in [
            ("claude", "first", "question one"),
            ("codex", "second", "question two"),
        ] {
            write(
                &root.join(format!(".clank/agents/{agent}/blocks/{name}.md")),
                q,
            );
        }

        run_unblock(crate::cli::UnblockArgs {
            agent: "claude".into(),
            name: "first".into(),
            message: "answered just this one".into(),
            repo: Some(root.to_path_buf()),
        })
        .await
        .unwrap();

        let blocks = scan_blocks(root);
        let answered: Vec<_> = blocks.iter().filter(|b| b.answer.is_some()).collect();
        assert_eq!(answered.len(), 1, "only the targeted block is answered");
        assert_eq!(answered[0].name, "first");
        let open: Vec<_> = blocks.iter().filter(|b| b.answer.is_none()).collect();
        assert_eq!(open.len(), 1, "the other agent's question stays open");
        assert_eq!(open[0].agent, "codex");
        assert!(
            !root.join(".clank/agents/codex/unblocks/second.md").exists(),
            "no canned answer written under the untouched question"
        );
    }

    #[tokio::test]
    async fn migrate_flattens_active_drops_inactive_and_refuses_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `live` is an active plan; `gone` is not (no plan file).
        write(&root.join(".clank/plans/live.md"), "# live");
        write(
            &root.join(".clank/agents/alice/blocks/live/q.md"),
            "keep me",
        );
        write(
            &root.join(".clank/agents/alice/unblocks/live/q.md"),
            "answered",
        );
        write(
            &root.join(".clank/agents/alice/blocks/gone/stale.md"),
            "drop me",
        );

        let args = crate::cli::BlockMigrateArgs {
            dry: false,
            repo: Some(root.to_path_buf()),
        };
        run_migrate(args).await.unwrap();

        // Active plan: block AND its answer both flatten. Moving one
        // without the other leaves the pair unmatched, which parks the
        // repo permanently.
        assert!(root.join(".clank/agents/alice/blocks/q.md").exists());
        assert!(root.join(".clank/agents/alice/unblocks/q.md").exists());
        // Inactive plan: dropped, not silently flattened into a
        // repo-wide park on a stale question.
        assert!(!root.join(".clank/agents/alice/blocks/stale.md").exists());
        assert!(
            !root
                .join(".clank/agents/alice/blocks/gone/stale.md")
                .exists()
        );

        let blocks = scan_blocks(root);
        assert_eq!(blocks.len(), 1, "only the migrated pair survives");
        assert_eq!(blocks[0].name, "q");
        assert_eq!(blocks[0].answer.as_deref(), Some("answered"));
    }

    #[tokio::test]
    async fn migrate_never_lands_an_answer_whose_question_stayed_scoped() {
        // The pair-splitting regression: `q` exists flat AND as a
        // scoped question+answer pair. Migrating the halves
        // independently refuses the question but moves the answer,
        // which marks the UNRELATED flat `q` answered.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".clank/plans/live.md"), "# live");
        write(
            &root.join(".clank/agents/alice/blocks/q.md"),
            "unrelated flat question",
        );
        write(
            &root.join(".clank/agents/alice/blocks/live/q.md"),
            "scoped question",
        );
        write(
            &root.join(".clank/agents/alice/unblocks/live/q.md"),
            "scoped answer",
        );

        let args = crate::cli::BlockMigrateArgs {
            dry: false,
            repo: Some(root.to_path_buf()),
        };
        assert!(
            run_migrate(args).await.is_err(),
            "a collision must fail the run"
        );

        assert!(
            !root.join(".clank/agents/alice/unblocks/q.md").exists(),
            "the answer must NOT land flat while its question stays scoped"
        );
        let blocks = scan_blocks(root);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].question, "unrelated flat question");
        assert!(
            blocks[0].answer.is_none(),
            "the unrelated flat question must not have acquired an answer"
        );
        // The scoped pair stays intact for the human to resolve.
        assert!(root.join(".clank/agents/alice/blocks/live/q.md").exists());
        assert!(root.join(".clank/agents/alice/unblocks/live/q.md").exists());
    }

    #[tokio::test]
    async fn migrate_refuses_two_active_plans_claiming_one_flat_name() {
        // Neither `q` collides with anything ON DISK, so a
        // filesystem-only preflight queues both to the same flat
        // destination and the apply loop overwrites the first.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".clank/plans/one.md"), "# one");
        write(&root.join(".clank/plans/two.md"), "# two");
        write(
            &root.join(".clank/agents/alice/blocks/one/q.md"),
            "question from plan one",
        );
        write(
            &root.join(".clank/agents/alice/unblocks/one/q.md"),
            "answer for plan one",
        );
        write(
            &root.join(".clank/agents/alice/blocks/two/q.md"),
            "question from plan two",
        );

        let args = crate::cli::BlockMigrateArgs {
            dry: false,
            repo: Some(root.to_path_buf()),
        };
        assert!(
            run_migrate(args).await.is_err(),
            "two sources claiming one flat name must fail preflight"
        );

        // Both scoped pairs survive intact, and nothing landed flat.
        assert_eq!(
            std::fs::read_to_string(root.join(".clank/agents/alice/blocks/one/q.md")).unwrap(),
            "question from plan one"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".clank/agents/alice/unblocks/one/q.md")).unwrap(),
            "answer for plan one"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".clank/agents/alice/blocks/two/q.md")).unwrap(),
            "question from plan two"
        );
        assert!(!root.join(".clank/agents/alice/blocks/q.md").exists());
        assert!(!root.join(".clank/agents/alice/unblocks/q.md").exists());
    }

    #[tokio::test]
    async fn migrate_conflict_leaves_every_other_agent_untouched() {
        // Preflight is repo-wide: one collision aborts the whole run,
        // so a partially-migrated repo can never be observed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".clank/plans/live.md"), "# live");
        write(&root.join(".clank/agents/alice/blocks/q.md"), "flat");
        write(&root.join(".clank/agents/alice/blocks/live/q.md"), "scoped");
        write(&root.join(".clank/agents/bob/blocks/live/ok.md"), "movable");

        let args = crate::cli::BlockMigrateArgs {
            dry: false,
            repo: Some(root.to_path_buf()),
        };
        assert!(run_migrate(args).await.is_err());
        assert!(
            root.join(".clank/agents/bob/blocks/live/ok.md").exists(),
            "bob's block must not move when alice's collides"
        );
        assert!(!root.join(".clank/agents/bob/blocks/ok.md").exists());
    }

    #[tokio::test]
    async fn migrate_refuses_to_clobber_an_existing_flat_block() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".clank/plans/live.md"), "# live");
        write(
            &root.join(".clank/agents/alice/blocks/q.md"),
            "flat question",
        );
        write(
            &root.join(".clank/agents/alice/blocks/live/q.md"),
            "scoped question",
        );

        let args = crate::cli::BlockMigrateArgs {
            dry: false,
            repo: Some(root.to_path_buf()),
        };
        assert!(
            run_migrate(args).await.is_err(),
            "a name collision must fail loudly, not destroy one of two real questions"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".clank/agents/alice/blocks/q.md")).unwrap(),
            "flat question",
            "the pre-existing flat block is untouched"
        );
        assert!(root.join(".clank/agents/alice/blocks/live/q.md").exists());
    }

    #[tokio::test]
    async fn migrate_dry_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".clank/agents/alice/blocks/gone/stale.md"), "q");

        let args = crate::cli::BlockMigrateArgs {
            dry: true,
            repo: Some(root.to_path_buf()),
        };
        run_migrate(args).await.unwrap();
        assert!(
            root.join(".clank/agents/alice/blocks/gone/stale.md")
                .exists(),
            "--dry must not delete the question it reports"
        );
    }

    #[test]
    fn scan_returns_sorted_by_agent_then_name() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".clank/agents/bob/blocks/z.md"), "q");
        write(&dir.path().join(".clank/agents/alice/blocks/b.md"), "q");
        write(&dir.path().join(".clank/agents/alice/blocks/a.md"), "q");
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].agent, "alice");
        assert_eq!(blocks[0].name, "a");
        assert_eq!(blocks[1].agent, "alice");
        assert_eq!(blocks[1].name, "b");
        assert_eq!(blocks[2].agent, "bob");
    }

    #[test]
    fn scan_empty_repo_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = scan_blocks(dir.path());
        assert!(blocks.is_empty());
    }
}
