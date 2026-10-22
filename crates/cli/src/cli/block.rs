use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{BlockArgs, BlockCmd, BlockCreateArgs, BlockMigrateArgs, UnblockArgs, resolve_repo};
use crate::agent_store::agents_root;
use clank_core::wait::AnswerRef;

pub async fn run(args: BlockArgs) -> anyhow::Result<()> {
    match args.command {
        BlockCmd::Ask(a) | BlockCmd::Create(a) => run_create(a).await,
        BlockCmd::Answer(a) => run_unblock(a).await,
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
    let path = ask(&repo, &author, &args.name, &args.message)?;

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
    let path = answer_block(&repo, &args.agent, &args.name, &args.message, None)?;

    println!(
        "unblocked {} as {}",
        args.agent,
        path.strip_prefix(&repo).unwrap_or(&path).display()
    );
    Ok(())
}

const STAMP: &str = "occurrence: ";

fn is_occurrence(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A stored question or answer split into the ask occurrence it names
/// and the text a reader sees. Files from before occurrences name none.
fn unstamp(raw: &str) -> (Option<&str>, &str) {
    if let Some(rest) = raw.strip_prefix(STAMP)
        && let Some((id, body)) = rest.split_once('\n')
        && is_occurrence(id)
    {
        return (Some(id), body);
    }
    (None, raw)
}

fn stamp(occurrence: &str, body: &str) -> String {
    format!("{STAMP}{occurrence}\n{body}")
}

/// A fresh ask identity. Random rather than counted, so no store has
/// to survive `block clean` for an identity never to come back.
fn mint_occurrence() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("minting a block occurrence: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn digest(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Whether an answer naming `answer` belongs to the ask naming
/// `question`. An unstamped question was written by a binary that paired
/// by name alone, so it keeps that rule until it is first stamped.
fn belongs(question: Option<&str>, answer: Option<&str>) -> bool {
    match question {
        Some(q) => answer == Some(q),
        None => true,
    }
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        anyhow::bail!("invalid block name `{name}`");
    }
    Ok(())
}

struct Files {
    question: PathBuf,
    answer: PathBuf,
    marker: PathBuf,
}

fn files(agent_dir: &Path, name: &str) -> Files {
    Files {
        question: agent_dir.join("blocks").join(format!("{name}.md")),
        answer: agent_dir.join("unblocks").join(format!("{name}.md")),
        marker: agent_dir.join("unblocks").join(format!("{name}.received")),
    }
}

/// The durable receipt: present and matching the current pair, it is
/// what retires that pair.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Marker {
    occurrence: String,
    question_digest: String,
    answer_digest: String,
}

/// One block name's files as they stand.
struct Pair {
    occurrence: Option<String>,
    question: String,
    /// The answer belonging to this ask, never a leftover from another.
    answer: Option<String>,
    received: bool,
}

impl Pair {
    fn read(agent_dir: &Path, name: &str) -> Option<Pair> {
        let f = files(agent_dir, name);
        if !std::fs::symlink_metadata(&f.question).is_ok_and(|m| m.file_type().is_file()) {
            return None;
        }
        let raw = std::fs::read_to_string(&f.question).unwrap_or_default();
        let (occurrence, question) = unstamp(&raw);
        let answer = std::fs::read_to_string(&f.answer).ok().and_then(|raw| {
            let (named, body) = unstamp(&raw);
            belongs(occurrence, named).then(|| body.to_string())
        });
        let received = match (occurrence, &answer) {
            (Some(occurrence), Some(answer)) => std::fs::read(&f.marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
                .is_some_and(|m| {
                    m == Marker {
                        occurrence: occurrence.to_string(),
                        question_digest: digest(question),
                        answer_digest: digest(answer),
                    }
                }),
            _ => false,
        };
        Some(Pair {
            occurrence: occurrence.map(str::to_owned),
            question: question.to_string(),
            answer,
            received,
        })
    }

    fn carries(&self, agent: &clank_core::AgentLabel, name: &str) -> Option<AnswerRef> {
        Some(AnswerRef {
            agent: agent.clone(),
            name: name.to_string(),
            occurrence: self.occurrence.clone(),
            question_digest: digest(&self.question),
            answer_digest: digest(self.answer.as_deref()?),
        })
    }
}

/// The names of the questions in `agent_dir`, sorted.
fn question_names(agent_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(agent_dir.join("blocks"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".md"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// Serializes every read-compare-write of `agent`'s blocks: ask, answer,
/// clean and receive.
pub(crate) fn lock(repo: &Path, agent: &clank_core::AgentLabel) -> anyhow::Result<std::fs::File> {
    let dir = agents_root(repo).join(agent.as_str());
    std::fs::create_dir_all(&dir)?;
    crate::agent_store::exclusive_lock(&dir.join("blocks.lock"))
}

fn publish(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let dir = path.parent().context("a block file has a directory")?;
    std::fs::create_dir_all(dir)?;
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged
        .persist(path)
        .with_context(|| format!("publishing `{}`", path.display()))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Ask `question` as a new occurrence of `name`. Whatever answer or
/// receipt an earlier ask of the name left is discarded, never inherited.
pub(crate) fn ask(
    repo: &Path,
    author: &clank_core::AgentLabel,
    name: &str,
    question: &str,
) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    let _lock = lock(repo, author)?;
    let f = files(&agents_root(repo).join(author.as_str()), name);
    remove_if_present(&f.answer)?;
    remove_if_present(&f.marker)?;
    publish(&f.question, stamp(&mint_occurrence()?, question).as_bytes())?;
    Ok(f.question)
}

/// The ask a form was showing when its answer was written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shown<'a> {
    pub(crate) occurrence: Option<&'a str>,
    pub(crate) question: &'a str,
}

/// The one unblock write for CLI and web. `shown` is captured by a web
/// form, so an answer is recorded only for the ask the reader saw, never
/// for a re-ask that replaced it, even with the same text. Without it the
/// current ask of `name` is answered. At most one answer is recorded per
/// ask.
pub(crate) fn answer_block(
    repo: &Path,
    agent: &str,
    name: &str,
    message: &str,
    shown: Option<Shown<'_>>,
) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let agent = clank_core::ids::AgentLabel::parse(agent)
        .map_err(|e| anyhow::anyhow!("invalid agent label: {e}"))?;
    validate_name(name)?;
    let answer = message.trim();
    if answer.is_empty() {
        anyhow::bail!("write an answer before submitting");
    }
    let _lock = lock(repo, &agent)?;
    let f = files(&agents_root(repo).join(agent.as_str()), name);
    if !std::fs::symlink_metadata(&f.question).is_ok_and(|meta| meta.file_type().is_file()) {
        anyhow::bail!("`{agent}` has no pending block named `{name}`");
    }
    let raw = std::fs::read_to_string(&f.question)
        .with_context(|| format!("reading block `{agent}/{name}`"))?;
    let (occurrence, question) = unstamp(&raw);
    if shown.is_some_and(|s| s.question != question || s.occurrence != occurrence) {
        anyhow::bail!("the question for `{agent}/{name}` changed; read it again before answering");
    }
    if let Ok(existing) = std::fs::read_to_string(&f.answer)
        && belongs(occurrence, unstamp(&existing).0)
    {
        anyhow::bail!("`{agent}/{name}` was already answered");
    }
    let occurrence = match occurrence {
        Some(o) => o.to_string(),
        None => {
            let o = mint_occurrence()?;
            publish(&f.question, stamp(&o, question).as_bytes())?;
            o
        }
    };
    publish(&f.answer, stamp(&occurrence, answer).as_bytes())
        .with_context(|| format!("recording answer for `{agent}/{name}`"))?;
    Ok(f.answer)
}

/// What one receipt did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Receipt {
    /// The carried pair is received; it will not be presented again.
    Retired,
    /// The name was re-asked, re-answered or cleaned since it was
    /// presented, so nothing was received.
    Stale,
    /// The delivering session no longer holds the label.
    Unauthorized,
}

/// Receive the exact answer `carries` names, once a wake carrying it has
/// reached the model. Retires the pair only while it still matches, and
/// only if `current` — the delivery's authorization — holds once the
/// receipt has the block and binding locks; the binding cannot change
/// until the marker is published.
pub(crate) fn receive(
    repo: &Path,
    carries: &AnswerRef,
    current: impl Fn() -> bool,
) -> anyhow::Result<Receipt> {
    validate_name(&carries.name)?;
    let _lock = lock(repo, &carries.agent)?;
    let _binding = crate::agent_store::hold_binding(repo, &carries.agent)?;
    if !current() {
        return Ok(Receipt::Unauthorized);
    }
    let dir = agents_root(repo).join(carries.agent.as_str());
    let pair = Pair::read(&dir, &carries.name);
    let now = pair
        .as_ref()
        .and_then(|p| p.carries(&carries.agent, &carries.name));
    let Some(pair) = pair.filter(|_| now.as_ref() == Some(carries)) else {
        eprintln!(
            "clank: received nothing for block `{}/{}`: it carried occurrence {} (question {}, \
             answer {}), and the block now holds {}",
            carries.agent,
            carries.name,
            carries.occurrence.as_deref().unwrap_or("none"),
            carries.question_digest,
            carries.answer_digest,
            now.map_or("no answer".to_string(), |n| format!(
                "occurrence {} (question {}, answer {})",
                n.occurrence.as_deref().unwrap_or("none"),
                n.question_digest,
                n.answer_digest
            )),
        );
        return Ok(Receipt::Stale);
    };
    if pair.received {
        return Ok(Receipt::Retired);
    }
    let f = files(&dir, &carries.name);
    let answer = pair.answer.as_deref().unwrap_or_default();
    let occurrence = match pair.occurrence {
        Some(o) => o,
        None => {
            // Answer first: an unstamped question still pairs with it, so
            // a crash between the two leaves the pair answered and owed.
            let o = mint_occurrence()?;
            publish(&f.answer, stamp(&o, answer).as_bytes())?;
            publish(&f.question, stamp(&o, &pair.question).as_bytes())?;
            o
        }
    };
    let marker = Marker {
        occurrence,
        question_digest: carries.question_digest.clone(),
        answer_digest: carries.answer_digest.clone(),
    };
    publish(&f.marker, &serde_json::to_vec(&marker)?)?;
    Ok(Receipt::Retired)
}

async fn run_clean(args: super::BlockCleanArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let author = crate::agent_env::resolve_identity_from_env(&repo)?;
    let removed = clean(&repo, &author)?;
    println!("cleaned {removed} file(s)");
    Ok(())
}

/// Remove `author`'s received pairs and every answer or receipt no
/// current ask owns. Housekeeping only: it never decides what is owed.
pub(crate) fn clean(repo: &Path, author: &clank_core::AgentLabel) -> anyhow::Result<usize> {
    let _lock = lock(repo, author)?;
    let dir = agents_root(repo).join(author.as_str());
    let mut removed = 0;
    let mut live = std::collections::BTreeSet::new();
    for name in question_names(&dir) {
        let Some(pair) = Pair::read(&dir, &name) else {
            continue;
        };
        let f = files(&dir, &name);
        if pair.received {
            // The question first: a leftover answer or marker names a
            // dead occurrence, which no reader takes for a live one.
            for path in [&f.question, &f.answer, &f.marker] {
                removed += usize::from(remove_if_present(path)?);
            }
            continue;
        }
        if pair.answer.is_some() {
            live.insert(f.answer);
        }
    }
    for entry in std::fs::read_dir(dir.join("unblocks"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        let leftover = path
            .extension()
            .is_some_and(|e| e == "md" || e == "received")
            && entry.file_type().is_ok_and(|t| t.is_file())
            && !live.contains(&path.with_extension("md"));
        if leftover {
            removed += usize::from(remove_if_present(&path)?);
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone)]
pub struct BlockEntry {
    pub agent: String,
    pub name: String,
    /// The ask this question is; `None` before asks carried one.
    pub occurrence: Option<String>,
    pub question: String,
    pub answer: Option<String>,
}

/// Every block not yet received, pending or answered. Read-only: it
/// never stamps, receives or removes anything.
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
        let dir = entry.path();
        for name in question_names(&dir) {
            let Some(pair) = Pair::read(&dir, &name).filter(|p| !p.received) else {
                continue;
            };
            out.push(BlockEntry {
                agent: agent.clone(),
                name,
                occurrence: pair.occurrence,
                question: pair.question,
                answer: pair.answer,
            });
        }
    }
    out.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.name.cmp(&b.name)));
    out
}

/// An answer `agent` is owed: answered and not yet received.
pub(crate) struct OwedAnswer {
    pub(crate) answer: String,
    pub(crate) carries: AnswerRef,
}

/// `agent`'s owed answers, by name. Read-only, like [`scan_blocks`].
pub(crate) fn owed_answers(repo: &Path, agent: &clank_core::AgentLabel) -> Vec<OwedAnswer> {
    let dir = agents_root(repo).join(agent.as_str());
    question_names(&dir)
        .into_iter()
        .filter_map(|name| {
            let pair = Pair::read(&dir, &name).filter(|p| !p.received)?;
            Some(OwedAnswer {
                carries: pair.carries(agent, &name)?,
                answer: pair.answer?,
            })
        })
        .collect()
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

    #[test]
    fn answer_requires_current_question_and_records_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(
            &repo.join(".clank/agents/alice/blocks/why.md"),
            "Which option?",
        );
        assert!(answer_block(repo, "alice", "why", " ", None).is_err());
        assert!(answer_block(repo, "alice", "../why", "yes", None).is_err());
        assert!(answer_block(repo, "alice", "missing", "yes", None).is_err());
        assert!(
            answer_block(repo, "alice", "why", "yes", Some(legacy("Old question")))
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        assert!(scan_blocks(repo)[0].answer.is_none());
        answer_block(
            repo,
            "alice",
            "why",
            "  first answer  ",
            Some(legacy("Which option?")),
        )
        .unwrap();
        assert!(
            answer_block(repo, "alice", "why", "second answer", None)
                .unwrap_err()
                .to_string()
                .contains("already answered")
        );
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("first answer"));
    }

    /// `block ask`/`block answer` and the old `block create`/`unblock`
    /// leave the same flat pair.
    #[tokio::test]
    async fn the_old_and_new_spellings_leave_the_same_pair() {
        let ask = |root: &std::path::Path| crate::cli::BlockCreateArgs {
            name: "q".into(),
            message: "why?".into(),
            author: Some("claude".into()),
            repo: Some(root.to_path_buf()),
        };
        let answer = |root: &std::path::Path| crate::cli::UnblockArgs {
            agent: "claude".into(),
            name: "q".into(),
            message: "yes".into(),
            repo: Some(root.to_path_buf()),
        };
        let new = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        run(BlockArgs {
            command: BlockCmd::Ask(ask(new.path())),
        })
        .await
        .unwrap();
        run(BlockArgs {
            command: BlockCmd::Answer(answer(new.path())),
        })
        .await
        .unwrap();
        run(BlockArgs {
            command: BlockCmd::Create(ask(old.path())),
        })
        .await
        .unwrap();
        run_unblock(answer(old.path())).await.unwrap();
        let read = |root: &std::path::Path| {
            scan_blocks(root)
                .into_iter()
                .map(|b| (b.agent, b.name, b.question, b.answer))
                .collect::<Vec<_>>()
        };
        assert_eq!(read(new.path()), read(old.path()));
        assert_eq!(
            read(new.path()),
            [(
                "claude".into(),
                "q".into(),
                "why?".into(),
                Some("yes".into())
            )]
        );
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

    fn legacy(question: &str) -> Shown<'_> {
        Shown {
            occurrence: None,
            question,
        }
    }

    fn alice() -> clank_core::AgentLabel {
        clank_core::AgentLabel::parse("alice").unwrap()
    }

    fn owed(repo: &Path) -> Vec<AnswerRef> {
        owed_answers(repo, &alice())
            .into_iter()
            .map(|o| o.carries)
            .collect()
    }

    fn marker(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/agents/alice/unblocks/{name}.received"))
    }

    fn asked_and_answered(repo: &Path, name: &str, q: &str, a: &str) -> AnswerRef {
        ask(repo, &alice(), name, q).unwrap();
        answer_block(repo, "alice", name, a, None).unwrap();
        let carries = owed(repo);
        assert_eq!(carries.len(), 1, "{carries:?}");
        carries[0].clone()
    }

    #[test]
    fn a_received_answer_is_retired_by_its_marker_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("yes"));
        assert_eq!(receive(repo, &a, || true).unwrap(), Receipt::Retired);
        assert!(marker(repo, "q").exists());
        assert!(
            repo.join(".clank/agents/alice/blocks/q.md").exists()
                && repo.join(".clank/agents/alice/unblocks/q.md").exists(),
            "receipt deletes nothing"
        );
        assert!(owed(repo).is_empty(), "not presented again");
        assert!(scan_blocks(repo).is_empty(), "neither pending nor owed");
    }

    /// A delivery whose binding is revoked while it waits for the block
    /// lock receives nothing: its authorization is read inside the receipt
    /// transaction, never carried in from before the wait.
    #[test]
    fn a_rebind_while_a_receipt_waits_for_the_lock_receives_nothing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for revoke in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let a = asked_and_answered(repo, "q", "Proceed?", "yes");
            let carried = crate::cli::presentation::Carried {
                events: Vec::new(),
                answers: vec![a.clone()],
            };
            let authorized = AtomicBool::new(true);
            let (observed, seen) = std::sync::mpsc::channel();
            let held = lock(repo, &alice()).unwrap();
            std::thread::scope(|s| {
                s.spawn(|| {
                    crate::cli::presentation::record_delivered(repo, &alice(), &carried, || {
                        let _ = observed.send(());
                        authorized.load(Ordering::SeqCst)
                    })
                });
                // A pre-lock check reports in at once; one read under the
                // lock cannot until the lock is released below.
                let _ = seen.recv_timeout(std::time::Duration::from_secs(2));
                if revoke {
                    authorized.store(false, Ordering::SeqCst);
                }
                drop(held);
            });
            assert_eq!(marker(repo, "q").exists(), !revoke, "revoked: {revoke}");
            assert_eq!(owed(repo).is_empty(), !revoke, "revoked: {revoke}");
        }
    }

    /// The authorization a receipt read stays true until its marker is
    /// published: a rebind that starts after the check lands after it.
    #[test]
    fn a_rebind_after_the_check_waits_for_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        let landed_after_marker = std::thread::scope(|s| {
            let rebind = std::cell::OnceCell::new();
            receive(repo, &a, || {
                rebind.get_or_init(|| {
                    s.spawn(|| {
                        crate::agent_store::update_agent_config(repo, &alice(), |_| {}).unwrap();
                        marker(repo, "q").exists()
                    })
                });
                std::thread::sleep(std::time::Duration::from_millis(200));
                true
            })
            .unwrap();
            rebind.into_inner().unwrap().join().unwrap()
        });
        assert!(landed_after_marker);
    }

    #[test]
    fn duplicate_receipts_write_one_marker() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        assert_eq!(receive(repo, &a, || true).unwrap(), Receipt::Retired);
        let first = std::fs::read(marker(repo, "q")).unwrap();
        assert_eq!(receive(repo, &a, || true).unwrap(), Receipt::Retired);
        assert_eq!(std::fs::read(marker(repo, "q")).unwrap(), first);
        let markers = std::fs::read_dir(repo.join(".clank/agents/alice/unblocks"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "received"))
            .count();
        assert_eq!(markers, 1);
    }

    #[test]
    fn files_without_a_matching_marker_stay_owed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        write(
            &marker(repo, "q"),
            &serde_json::to_string(&serde_json::json!({
                "occurrence": a.occurrence,
                "question_digest": a.question_digest,
                "answer_digest": digest("no"),
            }))
            .unwrap(),
        );
        assert_eq!(owed(repo), [a], "a marker for other bytes is stale");
    }

    /// The same question asked again and given the same answer is a new
    /// occurrence: a receipt carried for the first is a no-op, whether it
    /// arrives at once, after a clean, or from a process that started
    /// after the reuse.
    #[test]
    fn a_late_receipt_after_identical_reuse_receives_nothing() {
        for interleave in ["direct", "clean", "restart"] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let first = asked_and_answered(repo, "q", "Proceed?", "yes");
            if interleave == "clean" {
                clean(repo, &alice()).unwrap();
            }
            let second = asked_and_answered(repo, "q", "Proceed?", "yes");
            if interleave == "clean" {
                clean(repo, &alice()).unwrap();
            }
            let late: AnswerRef = match interleave {
                "restart" => serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap(),
                _ => first.clone(),
            };
            assert_eq!(
                (&first.question_digest, &first.answer_digest),
                (&second.question_digest, &second.answer_digest),
                "{interleave}: identical bytes"
            );
            assert_ne!(first.occurrence, second.occurrence, "{interleave}");
            assert_eq!(
                receive(repo, &late, || true).unwrap(),
                Receipt::Stale,
                "{interleave}"
            );
            assert!(!marker(repo, "q").exists(), "{interleave}");
            assert_eq!(owed(repo), [second], "{interleave}: the new ask stays owed");
        }
    }

    #[test]
    fn re_asking_inherits_neither_the_old_answer_nor_its_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        receive(repo, &a, || true).unwrap();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        assert!(!repo.join(".clank/agents/alice/unblocks/q.md").exists());
        assert!(!marker(repo, "q").exists());
        let blocks = scan_blocks(repo);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].answer, None, "owed to the human again");
        assert!(owed(repo).is_empty(), "no old answer is rendered for it");
        assert_eq!(receive(repo, &a, || true).unwrap(), Receipt::Stale);
        answer_block(repo, "alice", "q", "no", None).unwrap();
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("no"));
    }

    /// Whatever an interrupted clean leaves behind names a dead
    /// occurrence, so a new ask ignores it even where nothing removed it.
    #[test]
    fn a_leftover_from_another_occurrence_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        receive(repo, &a, || true).unwrap();
        let answer = std::fs::read(repo.join(".clank/agents/alice/unblocks/q.md")).unwrap();
        let receipt = std::fs::read(marker(repo, "q")).unwrap();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        write(
            &repo.join(".clank/agents/alice/unblocks/q.md"),
            std::str::from_utf8(&answer).unwrap(),
        );
        write(&marker(repo, "q"), std::str::from_utf8(&receipt).unwrap());
        assert_eq!(scan_blocks(repo)[0].answer, None);
        assert!(owed(repo).is_empty());
        answer_block(repo, "alice", "q", "no", None).unwrap();
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("no"));
    }

    /// A pair asked and answered before occurrences existed is owed once
    /// more, and reading it changes nothing; receiving it stamps both
    /// halves with one occurrence and keeps the human's answer.
    #[test]
    fn a_legacy_pair_is_owed_until_received_and_keeps_its_answer() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let q = repo.join(".clank/agents/alice/blocks/q.md");
        let a = repo.join(".clank/agents/alice/unblocks/q.md");
        write(&q, "Proceed?");
        write(&a, "yes");
        let carries = owed(repo);
        scan_blocks(repo);
        assert_eq!(
            std::fs::read_to_string(&q).unwrap(),
            "Proceed?",
            "reads never stamp"
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "yes");
        assert_eq!(carries.len(), 1);
        assert_eq!(carries[0].occurrence, None);

        assert_eq!(
            receive(repo, &carries[0], || true).unwrap(),
            Receipt::Retired
        );
        let (qo, ao) = (
            std::fs::read_to_string(&q).unwrap(),
            std::fs::read_to_string(&a).unwrap(),
        );
        let ((stamped_q, body_q), (stamped_a, body_a)) = (unstamp(&qo), unstamp(&ao));
        assert!(stamped_q.is_some() && stamped_q == stamped_a);
        assert_eq!((body_q, body_a), ("Proceed?", "yes"));
        assert!(owed(repo).is_empty(), "received");
    }

    /// A crash between stamping the answer and the question leaves the
    /// pair answered, and owed.
    #[test]
    fn a_half_stamped_legacy_pair_stays_answered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(".clank/agents/alice/blocks/q.md"), "Proceed?");
        write(
            &repo.join(".clank/agents/alice/unblocks/q.md"),
            &stamp("0123456789abcdef0123456789abcdef", "yes"),
        );
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("yes"));
        assert_eq!(
            receive(repo, &carries[0], || true).unwrap(),
            Receipt::Retired
        );
        assert!(owed(repo).is_empty());
    }

    #[test]
    fn answering_a_legacy_question_stamps_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let q = repo.join(".clank/agents/alice/blocks/q.md");
        write(&q, "Proceed?");
        answer_block(repo, "alice", "q", "yes", Some(legacy("Proceed?"))).unwrap();
        let raw = std::fs::read_to_string(&q).unwrap();
        assert!(unstamp(&raw).0.is_some(), "{raw}");
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(carries[0].occurrence.as_deref(), unstamp(&raw).0);
    }

    #[test]
    fn clean_removes_received_pairs_and_leftovers_and_keeps_what_is_owed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let done = asked_and_answered(repo, "done", "Proceed?", "yes");
        receive(repo, &done, || true).unwrap();
        let owed_now = asked_and_answered(repo, "owed", "Which?", "B");
        ask(repo, &alice(), "pending", "Why?").unwrap();
        write(
            &repo.join(".clank/agents/alice/unblocks/orphan.md"),
            &stamp("0123456789abcdef0123456789abcdef", "stale"),
        );
        assert_eq!(clean(repo, &alice()).unwrap(), 4);
        let left: Vec<_> = scan_blocks(repo)
            .into_iter()
            .map(|b| (b.name, b.answer))
            .collect();
        assert_eq!(
            left,
            [("owed".into(), Some("B".into())), ("pending".into(), None)]
        );
        assert_eq!(owed(repo), [owed_now]);
        assert!(!repo.join(".clank/agents/alice/blocks/done.md").exists());
        assert!(!repo.join(".clank/agents/alice/unblocks/orphan.md").exists());
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
