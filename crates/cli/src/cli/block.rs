use std::path::Path;

use super::{BlockArgs, BlockCmd, BlockCreateArgs, UnblockArgs, resolve_repo};
use crate::agent_store::agents_root;

pub async fn run(args: BlockArgs) -> anyhow::Result<()> {
    match args.command {
        BlockCmd::Create(a) => run_create(a).await,
        BlockCmd::Clean(a) => run_clean(a).await,
    }
}

async fn run_create(args: BlockCreateArgs) -> anyhow::Result<()> {
    // Scope is explicit-or-error: forgetting to scope would
    // silently suppress every wait item for this agent across
    // every plan + queue item (because `wait::check_blocks`
    // reads `plan: None` as `suppress_all = true`). Make the
    // user choose. `--plan` and `--all` are mutually exclusive
    // (clap enforces via `conflicts_with`); at least one must
    // be present here.
    if args.plan.is_none() && !args.all {
        anyhow::bail!(
            "block scope is required: pass `--plan <plan-stem>` to scope to a single plan, \
             or `--all` to suppress every wait item for this agent (rarely the right call)"
        );
    }

    let repo = resolve_repo(args.repo.as_deref())?;
    let author = match args.author.as_deref() {
        Some(raw) => clank_core::ids::AgentLabel::parse(raw)
            .map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?,
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };

    let dir = if let Some(ref plan) = args.plan {
        agents_root(&repo)
            .join(author.as_str())
            .join("blocks")
            .join(plan)
    } else {
        // --all path. Print the warning on stderr so the user
        // sees it even when the success line is captured.
        eprintln!(
            "REPO-WIDE BLOCK: this will suppress every wait item for `{}` until the block is answered",
            author.as_str()
        );
        agents_root(&repo).join(author.as_str()).join("blocks")
    };
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
        if let Some(ref plan) = args.plan {
            child.env("CLANK_PLAN", plan);
        }
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

    let dir = if let Some(ref plan) = args.plan {
        agents_root(&repo)
            .join(&args.agent)
            .join("unblocks")
            .join(plan)
    } else {
        agents_root(&repo).join(&args.agent).join("unblocks")
    };
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
        let block_path = match &entry.plan {
            Some(plan) => agents
                .join(&entry.agent)
                .join("blocks")
                .join(plan)
                .join(format!("{}.md", entry.name)),
            None => agents
                .join(&entry.agent)
                .join("blocks")
                .join(format!("{}.md", entry.name)),
        };
        let unblock_path = match &entry.plan {
            Some(plan) => agents
                .join(&entry.agent)
                .join("unblocks")
                .join(plan)
                .join(format!("{}.md", entry.name)),
            None => agents
                .join(&entry.agent)
                .join("unblocks")
                .join(format!("{}.md", entry.name)),
        };
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
    pub plan: Option<String>,
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
        if !entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let Some(agent) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let blocks_dir = entry.path().join("blocks");
        let unblocks_dir = entry.path().join("unblocks");

        scan_blocks_in_dir(&blocks_dir, &unblocks_dir, &agent, None, &mut out);

        if let Ok(subdirs) = std::fs::read_dir(&blocks_dir) {
            for sub in subdirs.flatten() {
                if !sub.file_type().map_or(false, |t| t.is_dir()) {
                    continue;
                }
                let Some(plan) = sub.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let plan_unblocks = unblocks_dir.join(&plan);
                scan_blocks_in_dir(&sub.path(), &plan_unblocks, &agent, Some(&plan), &mut out);
            }
        }
    }
    out.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.name.cmp(&b.name)));
    out
}

fn scan_blocks_in_dir(
    blocks_dir: &Path,
    unblocks_dir: &Path,
    agent: &str,
    plan: Option<&str>,
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
            plan: plan.map(str::to_string),
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
        assert!(blocks[0].plan.is_none());
        assert_eq!(blocks[0].question, "What should the return type be?");
        assert!(blocks[0].answer.is_none());
    }

    #[test]
    fn scan_finds_plan_block() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path()
                .join(".clank/agents/bob/blocks/auth/need-creds.md"),
            "Which OAuth provider?",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].agent, "bob");
        assert_eq!(blocks[0].name, "need-creds");
        assert_eq!(blocks[0].plan.as_deref(), Some("auth"));
        assert_eq!(blocks[0].question, "Which OAuth provider?");
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

    #[test]
    fn scan_plan_unblock_matches_plan_block() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".clank/agents/alice/blocks/auth/q.md"),
            "question",
        );
        write(
            &dir.path().join(".clank/agents/alice/unblocks/auth/q.md"),
            "answer",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].answer.as_deref(), Some("answer"));
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
