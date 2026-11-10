use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{BlockArgs, BlockCmd, BlockCreateArgs, BlockMigrateArgs, UnblockArgs, resolve_repo};
use crate::agent_store::agents_root;
use crate::cli::nudge_log::Outcome;
use clank_core::wait::AnswerRef;

pub async fn run(args: BlockArgs) -> anyhow::Result<()> {
    match args.command {
        BlockCmd::Ask(a) => run_create(a).await,
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

/// The user's `blocked` hook may page someone and wait on the answer.
const BLOCK_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(10 * 60);

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
    let kind = if args.permission {
        let suggestion = match (args.suggest, args.suggest_file) {
            (Some(text), _) => Some(text),
            (None, Some(path)) => Some(std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("reading --suggest-file `{}`: {e}", path.display())
            })?),
            (None, None) => None,
        };
        Kind::Permission {
            suggestion: suggestion
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    } else {
        Kind::Plain
    };
    let path = ask_with(&repo, &author, &args.name, &args.message, &kind)?;

    let config = crate::cli::config::load(&repo).map_err(|e| {
        anyhow::anyhow!("the block is raised, but its `hooks.blocked` command was not run: {e:#}")
    })?;
    if let Some(Some(cmd)) = config.hooks.get(&clank_core::HookEvent::Blocked) {
        use crate::proc_run::{Out, Spec, Stdin};
        let _ = crate::proc_run::run_blocking(
            Spec::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("CLANK_EVENT", "blocked")
                .env("CLANK_AGENT", author.as_str())
                .env("CLANK_BLOCK_NAME", &args.name)
                .env("CLANK_REPO", &*repo.to_string_lossy())
                .stdin(Stdin::Inherit)
                .stderr(Out::Inherit),
            BLOCK_HOOK_BUDGET,
        );
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
    let text = || {
        args.message
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("`-m` is required unless settling with `--delivered`"))
    };
    let by = if args.override_permission_classifier {
        By::Person
    } else {
        By::Unattested
    };
    let resolved = if args.delivered {
        resolve(&repo, &args.agent, &args.name, None, by, How::Delivered)?
    } else if args.nudge {
        let to = clank_core::ids::AgentLabel::parse(&args.agent)
            .map_err(|e| anyhow::anyhow!("invalid agent label: {e}"))?;
        let text = text()?;
        let mut pane = crate::cli::nudge::Pane {
            repo: &repo,
            to: &to,
        };
        crate::cli::event_interrupt::blocking(|| {
            resolve(
                &repo,
                &args.agent,
                &args.name,
                None,
                by,
                How::Nudge {
                    text,
                    resend: args.resend,
                    via: &mut pane,
                },
            )
        })?
    } else {
        resolve(
            &repo,
            &args.agent,
            &args.name,
            None,
            by,
            How::Plain(text()?),
        )?
    };
    let path = resolved.into_answer(&args.agent, &args.name)?;
    let verb = if args.nudge || args.delivered {
        "unblocked by nudge"
    } else {
        "unblocked"
    };
    println!(
        "{verb} {} as {}",
        args.agent,
        path.strip_prefix(&repo).unwrap_or(&path).display()
    );
    Ok(())
}

const STAMP: &str = "occurrence: ";

fn is_occurrence(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A stored question or answer: the ask occurrence it names, its header
/// metadata, and the text a reader sees.
///
/// The header is `occurrence: <id>`, optionally followed on the same line
/// by ` <json>`. Files from before occurrences name none; files from
/// before the metadata end the line at the id and parse as they always did.
struct Stamp<'a> {
    occurrence: Option<&'a str>,
    meta: Option<&'a str>,
    body: &'a str,
    /// A header naming an occurrence but ending before its line does: every
    /// writer terminates the header, so this file was cut short.
    torn: bool,
}

impl Stamp<'_> {
    /// Only a header without metadata is plain by default: metadata that
    /// does not parse is unreadable, never taken for plain.
    fn kind(&self) -> Stored<Kind> {
        if self.torn {
            return Stored::Unreadable("the question's header is cut short".to_string());
        }
        match self.meta {
            None => Stored::Valid(Kind::Plain),
            Some(m) => serde_json::from_str(m).map_or_else(
                |e| Stored::Unreadable(format!("the question's kind `{m}` is unreadable: {e}")),
                Stored::Valid,
            ),
        }
    }

    fn disposition(&self) -> Stored<Option<Disposition>> {
        if self.torn {
            return Stored::Unreadable("the answer's header is cut short".to_string());
        }
        match self.meta {
            None => Stored::Valid(None),
            Some(m) => serde_json::from_str(m).map_or_else(
                |e| {
                    Stored::Unreadable(format!("the answer's disposition `{m}` is unreadable: {e}"))
                },
                |d| Stored::Valid(Some(d)),
            ),
        }
    }
}

/// A stored value as read. One that is present but cannot be read is kept
/// apart from one that is absent, so no reader can take it for absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored<T> {
    Valid(T),
    Unreadable(String),
}

impl<T> Stored<T> {
    pub fn valid(&self) -> Option<&T> {
        match self {
            Stored::Valid(t) => Some(t),
            Stored::Unreadable(_) => None,
        }
    }

    pub fn unreadable(&self) -> Option<&str> {
        match self {
            Stored::Valid(_) => None,
            Stored::Unreadable(why) => Some(why),
        }
    }
}

fn unstamp(raw: &str) -> Stamp<'_> {
    if let Some(rest) = raw.strip_prefix(STAMP) {
        let (line, body, torn) = match rest.split_once('\n') {
            Some((line, body)) => (line, body, false),
            None => (rest, "", true),
        };
        let (id, meta) = match line.split_once(' ') {
            Some((id, meta)) => (id, Some(meta)),
            None => (line, None),
        };
        if is_occurrence(id) {
            return Stamp {
                occurrence: Some(id),
                meta,
                body,
                torn,
            };
        }
    }
    Stamp {
        occurrence: None,
        meta: None,
        body: raw,
        torn: false,
    }
}

/// Without metadata this is the pre-metadata format, so a plain block
/// stays readable by binaries that predate kinds.
fn stamp(occurrence: &str, meta: Option<&str>, body: &str) -> String {
    match meta {
        Some(meta) => format!("{STAMP}{occurrence} {meta}\n{body}"),
        None => format!("{STAMP}{occurrence}\n{body}"),
    }
}

fn stamp_question(occurrence: &str, kind: &Kind, body: &str) -> anyhow::Result<String> {
    let meta = match kind {
        Kind::Plain => None,
        Kind::Permission { .. } => Some(serde_json::to_string(kind)?),
    };
    Ok(stamp(occurrence, meta.as_deref(), body))
}

fn stamp_nudged(occurrence: &str, attempt: &str, body: &str) -> anyhow::Result<String> {
    let meta = serde_json::to_string(&Disposition::Nudged {
        attempt: attempt.to_string(),
    })?;
    Ok(stamp(occurrence, Some(&meta), body))
}

/// What a block waits for.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Plain,
    /// A refused permission. Only the person's own message in the agent's
    /// terminal lifts it, so it is answered only by a nudge. The suggestion
    /// is the agent's guess at that message, never sent by itself.
    Permission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggestion: Option<String>,
    },
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Plain => "plain",
            Kind::Permission { .. } => "permission",
        }
    }

    pub fn suggestion(&self) -> Option<&str> {
        match self {
            Kind::Plain => None,
            Kind::Permission { suggestion } => suggestion.as_deref(),
        }
    }
}

/// How an answer was given, when not by a plain write.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum Disposition {
    /// Typed into the agent's terminal by the attempt it names. The agent
    /// has already seen it, so it is received the moment it is written.
    Nudged { attempt: String },
}

/// A fresh ask or attempt identity. Random rather than counted, so no
/// store has to survive `block clean` for an identity never to come back.
fn mint_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("minting a block identity: {e}"))?;
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
    attempt: PathBuf,
}

fn files(agent_dir: &Path, name: &str) -> Files {
    Files {
        question: agent_dir.join("blocks").join(format!("{name}.md")),
        answer: agent_dir.join("unblocks").join(format!("{name}.md")),
        marker: agent_dir.join("unblocks").join(format!("{name}.received")),
        attempt: agent_dir.join("blocks").join(format!("{name}.attempt")),
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

/// A claimed nudge answering one ask, kept while it is unsettled. It
/// serializes answerers in place of the lock, which is never held across
/// a delivery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attempt {
    /// The ask occurrence the attempt claimed.
    pub occurrence: String,
    /// Minted per claim. A settle acts only while the record still holds
    /// it, because the record's filename is reused by every claim.
    pub attempt: String,
    pub text: String,
    /// Unix seconds.
    pub started: u64,
    /// Why the outcome is unknown, once a delivery returned unable to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl Attempt {
    /// Why this attempt is unresolved, as every surface says it.
    pub fn unknown_reason(&self) -> String {
        match &self.diagnostic {
            Some(why) => format!("nudge outcome unknown: {why}"),
            None => "nudge outcome unknown: no outcome was recorded; it is still in flight, \
                     or its process was interrupted"
                .to_string(),
        }
    }

    /// Whether the record now on disk is this claim. An unreadable record
    /// is no one's: it is neither settled, updated nor removed.
    fn owns(&self, current: Option<&Stored<Attempt>>) -> bool {
        matches!(current, Some(Stored::Valid(c))
            if c.occurrence == self.occurrence && c.attempt == self.attempt)
    }
}

/// `None` only when there is no record.
fn read_attempt(path: &Path) -> Option<Stored<Attempt>> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Stored::Unreadable(format!(
            "its attempt record `{}` cannot be read: {e}",
            path.display()
        ))),
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_or_else(
            |e| {
                Stored::Unreadable(format!(
                    "its attempt record `{}` is malformed: {e}",
                    path.display()
                ))
            },
            Stored::Valid,
        )),
    }
}

/// The attempt record of one ask, as a reader must treat it.
enum AttemptState {
    Absent,
    /// The unsettled nudge for this ask.
    Live(Attempt),
    /// Valid, and provably not this ask's unsettled nudge: for another
    /// occurrence, or its nudged answer is written. Ignored by every
    /// reader, and removed by the next write under the lock.
    Inert,
    /// Present but unreadable: it may be an attempt in flight, so it
    /// keeps the block unresolved and is never removed.
    Unreadable(String),
}

/// One block name's files as they stand.
struct Pair {
    occurrence: Option<String>,
    question: String,
    kind: Stored<Kind>,
    /// The answer belonging to this ask, never a leftover from another.
    answer: Option<String>,
    disposition: Stored<Option<Disposition>>,
    received: bool,
    attempt: AttemptState,
}

impl Pair {
    fn read(agent_dir: &Path, name: &str) -> Option<Pair> {
        let f = files(agent_dir, name);
        if !std::fs::symlink_metadata(&f.question).is_ok_and(|m| m.file_type().is_file()) {
            return None;
        }
        let (raw, question_error) = match std::fs::read_to_string(&f.question) {
            Ok(raw) => (raw, None),
            Err(e) => (
                String::new(),
                Some(format!("the question cannot be read: {e}")),
            ),
        };
        let q = unstamp(&raw);
        let occurrence = q.occurrence;
        let (answer, disposition) = match std::fs::read_to_string(&f.answer) {
            Ok(raw) => {
                let a = unstamp(&raw);
                if belongs(occurrence, a.occurrence) {
                    (Some(a.body.to_string()), a.disposition())
                } else {
                    (None, Stored::Valid(None))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, Stored::Valid(None)),
            Err(e) => (
                None,
                Stored::Unreadable(format!("the answer cannot be read: {e}")),
            ),
        };
        let nudged_by = match &disposition {
            Stored::Valid(Some(Disposition::Nudged { attempt })) => Some(attempt.as_str()),
            _ => None,
        };
        let received = nudged_by.is_some()
            || match (occurrence, &answer) {
                (Some(occurrence), Some(answer)) => std::fs::read(&f.marker)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
                    .is_some_and(|m| {
                        m == Marker {
                            occurrence: occurrence.to_string(),
                            question_digest: digest(q.body),
                            answer_digest: digest(answer),
                        }
                    }),
                _ => false,
            };
        let attempt = match read_attempt(&f.attempt) {
            None => AttemptState::Absent,
            Some(Stored::Unreadable(why)) => AttemptState::Unreadable(why),
            // An unread question names no occurrence to judge it against.
            Some(Stored::Valid(a)) if question_error.is_some() => AttemptState::Live(a),
            Some(Stored::Valid(a))
                if Some(a.occurrence.as_str()) != occurrence
                    || nudged_by == Some(a.attempt.as_str()) =>
            {
                AttemptState::Inert
            }
            Some(Stored::Valid(a)) => AttemptState::Live(a),
        };
        Some(Pair {
            occurrence: occurrence.map(str::to_owned),
            question: q.body.to_string(),
            kind: match question_error {
                Some(why) => Stored::Unreadable(why),
                None => q.kind(),
            },
            answer,
            disposition,
            received,
            attempt,
        })
    }

    /// Every part of the pair that cannot be read. While there is any,
    /// the block is neither answered, retried, owed, received nor removed.
    fn damage(&self) -> Option<String> {
        let reasons: Vec<&str> = [
            self.kind.unreadable(),
            self.disposition.unreadable(),
            match &self.attempt {
                AttemptState::Unreadable(why) => Some(why.as_str()),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        .collect();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }

    fn live_attempt(&self) -> Option<&Attempt> {
        match &self.attempt {
            AttemptState::Live(a) => Some(a),
            _ => None,
        }
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

/// Ask `question` as a new plain occurrence of `name`.
#[cfg(test)]
pub(crate) fn ask(
    repo: &Path,
    author: &clank_core::AgentLabel,
    name: &str,
    question: &str,
) -> anyhow::Result<PathBuf> {
    ask_with(repo, author, name, question, &Kind::Plain)
}

/// Ask `question` as a new occurrence of `name`, of `kind`. Whatever
/// answer or receipt an earlier ask of the name left is discarded, never
/// inherited; an earlier ask's attempt record is left inert.
pub(crate) fn ask_with(
    repo: &Path,
    author: &clank_core::AgentLabel,
    name: &str,
    question: &str,
    kind: &Kind,
) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    let _lock = lock(repo, author)?;
    let f = files(&agents_root(repo).join(author.as_str()), name);
    remove_if_present(&f.answer)?;
    remove_if_present(&f.marker)?;
    publish(
        &f.question,
        stamp_question(&mint_id()?, kind, question)?.as_bytes(),
    )?;
    Ok(f.question)
}

/// The ask a form was showing when its answer was written.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shown<'a> {
    pub(crate) occurrence: Option<&'a str>,
    pub(crate) question: &'a str,
}

/// Types a block's answer into the agent's terminal: the one seam every
/// nudged answer passes through.
pub(crate) trait Nudger {
    /// Only `Submitted` says the text arrived and `Refused` that nothing
    /// was typed; any other outcome may have landed.
    fn nudge(&mut self, text: &str) -> Outcome;
}

#[cfg(test)]
impl<F: FnMut(&str) -> Outcome> Nudger for F {
    fn nudge(&mut self, text: &str) -> Outcome {
        self(text)
    }
}

/// How a block is answered.
pub(crate) enum How<'a> {
    /// Record the text as the answer. Refused for a permission block, and
    /// while a nudge is outstanding.
    Plain(&'a str),
    /// Type the text into the agent's terminal, recording it only once it
    /// was submitted. `resend` replaces an outstanding attempt rather than
    /// being refused by it.
    Nudge {
        text: &'a str,
        resend: bool,
        via: &'a mut dyn Nudger,
    },
    /// The person saw the outstanding attempt's text arrive: record it as
    /// nudged, typing nothing.
    Delivered,
}

/// How a resolution ended, when nothing refused it before any typing.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// The answer is recorded there.
    Answered(PathBuf),
    /// Nothing was typed; the block stays open.
    NotSent(String),
    /// The text may have arrived. The attempt stays outstanding with this
    /// diagnostic, and nothing resends it.
    Unknown(String),
    /// The text was delivered, but its claim no longer stood, so nothing
    /// was recorded.
    Superseded(String),
}

impl Resolved {
    /// The answer's path, or the outcome as an error naming what to run.
    pub(crate) fn into_answer(self, agent: &str, name: &str) -> anyhow::Result<PathBuf> {
        match self {
            Resolved::Answered(path) => Ok(path),
            Resolved::NotSent(why) => {
                anyhow::bail!("nothing was typed, so `{agent}/{name}` stays open: {why}")
            }
            Resolved::Unknown(why) => anyhow::bail!(
                "nudge outcome unknown: {why}. `{agent}/{name}` stays unresolved and nothing \
                 will be resent. If the text arrived: `clank block answer {agent} {name} \
                 --delivered`; if it did not: `clank block answer {agent} {name} --nudge \
                 --resend -m …` (a permission block also needs \
                 `--override-permission-classifier`, passed by a person)"
            ),
            Resolved::Superseded(why) => anyhow::bail!("{why}"),
        }
    }
}

/// Who is answering, as far as clank is told. Answering a permission block
/// lifts a refusal the agent's permission classifier made, so only a
/// [`By::Person`] answerer may resolve one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum By {
    /// A person's own action: a TUI keypress, or the CLI with
    /// `--override-permission-classifier`.
    Person,
    Unattested,
}

/// A plain, unattested answer: [`resolve`] with [`How::Plain`].
pub(crate) fn answer_block(
    repo: &Path,
    agent: &str,
    name: &str,
    message: &str,
    shown: Option<Shown<'_>>,
) -> anyhow::Result<PathBuf> {
    resolve(
        repo,
        agent,
        name,
        shown,
        By::Unattested,
        How::Plain(message),
    )?
    .into_answer(agent, name)
}

/// The one way every answerer resolves a block. `shown` is captured by a
/// form, so an answer is recorded only for the ask the reader saw, never
/// for a re-ask that replaced it, even with the same text; without it the
/// current ask of `name` is answered. At most one answer is recorded per
/// ask, and at most one delivery is made per ask without `resend`.
pub(crate) fn resolve(
    repo: &Path,
    agent: &str,
    name: &str,
    shown: Option<Shown<'_>>,
    by: By,
    how: How<'_>,
) -> anyhow::Result<Resolved> {
    use anyhow::Context as _;
    let agent = clank_core::ids::AgentLabel::parse(agent)
        .map_err(|e| anyhow::anyhow!("invalid agent label: {e}"))?;
    validate_name(name)?;
    let dir = agents_root(repo).join(agent.as_str());
    let f = files(&dir, name);
    match how {
        How::Plain(text) => {
            let text = nonempty(text)?;
            let _lock = lock(repo, &agent)?;
            let open = open_pair(&dir, &f, &agent, name, shown)?;
            if let Kind::Permission { .. } = open.kind {
                anyhow::bail!(
                    "`{agent}/{name}` is a permission block: only the person's own message in \
                     the agent's terminal lifts the refusal, so a person answers it with `clank \
                     block answer {agent} {name} --nudge --override-permission-classifier -m \
                     \"…\"`"
                );
            }
            if let Some(a) = open.pair.live_attempt() {
                return Err(outstanding(&agent, name, a));
            }
            let occurrence = stamped(&f, &open)?;
            publish(&f.answer, stamp(&occurrence, None, text).as_bytes())
                .with_context(|| format!("recording answer for `{agent}/{name}`"))?;
            Ok(Resolved::Answered(f.answer))
        }
        How::Delivered => {
            let _lock = lock(repo, &agent)?;
            let open = open_pair(&dir, &f, &agent, name, shown)?;
            attested(&open, by, &agent, name)?;
            let Some(a) = open.pair.live_attempt() else {
                anyhow::bail!(
                    "no nudge answering `{agent}/{name}` is outstanding, so there is nothing \
                     to settle as delivered"
                );
            };
            publish(
                &f.answer,
                stamp_nudged(&a.occurrence, &a.attempt, &a.text)?.as_bytes(),
            )
            .with_context(|| format!("recording answer for `{agent}/{name}`"))?;
            // The nudged answer already makes the record inert.
            let _ = remove_if_present(&f.attempt);
            Ok(Resolved::Answered(f.answer))
        }
        How::Nudge { text, resend, via } => {
            let text = nonempty(text)?;
            let claimed = claim(repo, &agent, &dir, &f, name, shown, by, text, resend)?;
            let outcome = via.nudge(text);
            let _ = crate::cli::nudge_log::append(
                &dir,
                &crate::cli::nudge_log::Record {
                    at: crate::cli::recovery::unix_now(),
                    attempt: claimed.attempt.clone(),
                    trigger: crate::cli::nudge_log::Trigger::Block {
                        name: name.to_string(),
                    },
                    text: text.to_string(),
                    outcome: outcome.clone(),
                },
            );
            settle(repo, &agent, &dir, &f, name, &claimed, outcome)
        }
    }
}

fn nonempty(text: &str) -> anyhow::Result<&str> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("write an answer before submitting");
    }
    Ok(text)
}

fn outstanding(agent: &clank_core::AgentLabel, name: &str, a: &Attempt) -> anyhow::Error {
    anyhow::anyhow!(
        "a nudge answering `{agent}/{name}` is outstanding ({}; started {} ago, typing {:?}), and \
         nothing resends it automatically. If the text arrived: `clank block answer {agent} \
         {name} --delivered`. If it did not, or a duplicate is acceptable: `clank block answer \
         {agent} {name} --nudge --resend -m …`. A permission block also needs \
         `--override-permission-classifier`, passed by a person",
        a.unknown_reason(),
        crate::age::duration(crate::cli::recovery::unix_now().saturating_sub(a.started)),
        a.text,
    )
}

/// A pair an answerer may act on: every part of it read, and its kind
/// known.
struct Open {
    pair: Pair,
    kind: Kind,
}

/// Under the lock: the pair as an answerer must find it, asked, the ask it
/// saw, wholly readable, and unanswered. An inert attempt record is
/// removed on the way.
fn open_pair(
    dir: &Path,
    f: &Files,
    agent: &clank_core::AgentLabel,
    name: &str,
    shown: Option<Shown<'_>>,
) -> anyhow::Result<Open> {
    let Some(pair) = Pair::read(dir, name) else {
        anyhow::bail!("`{agent}` has no pending block named `{name}`");
    };
    if shown
        .is_some_and(|s| s.question != pair.question || s.occurrence != pair.occurrence.as_deref())
    {
        anyhow::bail!("the question for `{agent}/{name}` changed; read it again before answering");
    }
    if let Some(why) = pair.damage() {
        anyhow::bail!(
            "`{agent}/{name}` has stored state clank cannot read, so nothing answers or retries \
             it until a person repairs it: {why}"
        );
    }
    let Stored::Valid(kind) = pair.kind.clone() else {
        unreachable!("an unreadable kind is damage");
    };
    if let AttemptState::Inert = pair.attempt {
        remove_if_present(&f.attempt)?;
    }
    if pair.answer.is_some() {
        anyhow::bail!("`{agent}/{name}` was already answered");
    }
    Ok(Open { pair, kind })
}

/// A permission block is resolved only by a person.
fn attested(open: &Open, by: By, agent: &clank_core::AgentLabel, name: &str) -> anyhow::Result<()> {
    if let Kind::Permission { .. } = open.kind
        && by != By::Person
    {
        anyhow::bail!(
            "`{agent}/{name}` is a permission block: answering it lifts a refusal the agent's \
             permission classifier made, so it must be run by a person, who passes \
             `--override-permission-classifier`"
        );
    }
    Ok(())
}

/// The pair's occurrence, stamping a question that predates occurrences.
fn stamped(f: &Files, open: &Open) -> anyhow::Result<String> {
    match &open.pair.occurrence {
        Some(o) => Ok(o.clone()),
        None => {
            let o = mint_id()?;
            publish(
                &f.question,
                stamp_question(&o, &open.kind, &open.pair.question)?.as_bytes(),
            )?;
            Ok(o)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn claim(
    repo: &Path,
    agent: &clank_core::AgentLabel,
    dir: &Path,
    f: &Files,
    name: &str,
    shown: Option<Shown<'_>>,
    by: By,
    text: &str,
    resend: bool,
) -> anyhow::Result<Attempt> {
    let _lock = lock(repo, agent)?;
    let open = open_pair(dir, f, agent, name, shown)?;
    attested(&open, by, agent, name)?;
    if let Some(a) = open.pair.live_attempt()
        && !resend
    {
        return Err(outstanding(agent, name, a));
    }
    let claimed = Attempt {
        occurrence: stamped(f, &open)?,
        attempt: mint_id()?,
        text: text.to_string(),
        started: crate::cli::recovery::unix_now(),
        diagnostic: None,
    };
    publish(&f.attempt, &serde_json::to_vec(&claimed)?)?;
    Ok(claimed)
}

/// Settle `claimed` by what its delivery could tell. Every write is fenced
/// to the claim: a record replaced by a resend or a re-ask's claim is not
/// this attempt's to settle, update or remove.
fn settle(
    repo: &Path,
    agent: &clank_core::AgentLabel,
    dir: &Path,
    f: &Files,
    name: &str,
    claimed: &Attempt,
    outcome: Outcome,
) -> anyhow::Result<Resolved> {
    use anyhow::Context as _;
    let _lock = lock(repo, agent)?;
    let owned = claimed.owns(read_attempt(&f.attempt).as_ref());
    match outcome {
        Outcome::Submitted => {
            if !owned {
                return Ok(Resolved::Superseded(format!(
                    "the text was delivered to `{agent}`, but this attempt's claim on \
                     `{agent}/{name}` was replaced (a --resend or --delivered) before it settled, \
                     so nothing was recorded for it"
                )));
            }
            let pair = Pair::read(dir, name)
                .filter(|p| p.occurrence.as_deref() == Some(claimed.occurrence.as_str()));
            if let Some(why) = pair.as_ref().and_then(Pair::damage) {
                anyhow::bail!(
                    "the text was delivered to `{agent}`, but `{agent}/{name}` has stored state \
                     clank cannot read, so no answer was recorded: {why}. Once it is repaired \
                     and you have seen the text arrive, run `clank block answer {agent} {name} \
                     --delivered --override-permission-classifier`"
                );
            }
            match pair {
                Some(p) if p.answer.is_none() => {
                    publish(
                        &f.answer,
                        stamp_nudged(&claimed.occurrence, &claimed.attempt, &claimed.text)?
                            .as_bytes(),
                    )
                    .with_context(|| {
                        format!(
                            "the text was delivered to `{agent}`, but recording the answer for \
                             `{agent}/{name}` failed; once you have seen it arrive, run `clank \
                             block answer {agent} {name} --delivered`"
                        )
                    })?;
                    // The nudged answer already makes the record inert.
                    let _ = remove_if_present(&f.attempt);
                    Ok(Resolved::Answered(f.answer.clone()))
                }
                _ => {
                    remove_if_present(&f.attempt)?;
                    Ok(Resolved::Superseded(format!(
                        "the text was delivered to `{agent}` for a question `{name}` that has \
                         since been asked again, answered or withdrawn; nothing was recorded, \
                         and any new ask stays open"
                    )))
                }
            }
        }
        Outcome::Refused { reason } => {
            if owned {
                remove_if_present(&f.attempt)?;
            }
            Ok(Resolved::NotSent(reason))
        }
        other => {
            let reason = match other {
                Outcome::Indeterminate { reason } => reason,
                other => format!("{other:?}"),
            };
            if owned {
                let noted = Attempt {
                    diagnostic: Some(reason.clone()),
                    ..claimed.clone()
                };
                publish(&f.attempt, &serde_json::to_vec(&noted)?)?;
            }
            Ok(Resolved::Unknown(reason))
        }
    }
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
    if pair.damage().is_some() {
        return Ok(Receipt::Stale);
    }
    if pair.received {
        return Ok(Receipt::Retired);
    }
    let Stored::Valid(kind) = &pair.kind else {
        unreachable!("an unreadable kind is damage");
    };
    let f = files(&dir, &carries.name);
    let answer = pair.answer.as_deref().unwrap_or_default();
    let occurrence = match pair.occurrence {
        Some(o) => o,
        None => {
            // Answer first: an unstamped question still pairs with it, so
            // a crash between the two leaves the pair answered and owed.
            let o = mint_id()?;
            publish(&f.answer, stamp(&o, None, answer).as_bytes())?;
            publish(
                &f.question,
                stamp_question(&o, kind, &pair.question)?.as_bytes(),
            )?;
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

/// Remove `author`'s received pairs and every answer, receipt or attempt
/// record no current ask owns. Housekeeping only: it never decides what
/// is owed, and never removes an outstanding attempt.
pub(crate) fn clean(repo: &Path, author: &clank_core::AgentLabel) -> anyhow::Result<usize> {
    let _lock = lock(repo, author)?;
    let dir = agents_root(repo).join(author.as_str());
    let mut removed = 0;
    let mut live = std::collections::BTreeSet::new();
    let mut live_attempts = std::collections::BTreeSet::new();
    for name in question_names(&dir) {
        let Some(pair) = Pair::read(&dir, &name) else {
            continue;
        };
        let f = files(&dir, &name);
        if pair.damage().is_some() {
            live.insert(f.answer);
            live_attempts.insert(f.attempt);
            continue;
        }
        if pair.received {
            // The question first: a leftover answer or marker names a
            // dead occurrence, which no reader takes for a live one.
            for path in [&f.question, &f.answer, &f.marker, &f.attempt] {
                removed += usize::from(remove_if_present(path)?);
            }
            continue;
        }
        if pair.answer.is_some() {
            live.insert(f.answer);
        }
        if pair.live_attempt().is_some() {
            live_attempts.insert(f.attempt);
        }
    }
    let files_in = |sub: &str, exts: &[&str]| {
        std::fs::read_dir(dir.join(sub))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|e| exts.iter().any(|x| e == *x))
            })
            .collect::<Vec<_>>()
    };
    for path in files_in("unblocks", &["md", "received"]) {
        if !live.contains(&path.with_extension("md")) {
            removed += usize::from(remove_if_present(&path)?);
        }
    }
    for path in files_in("blocks", &["attempt"]) {
        let provably_dead = !matches!(read_attempt(&path), Some(Stored::Unreadable(_)));
        if !live_attempts.contains(&path) && provably_dead {
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
    pub kind: Stored<Kind>,
    pub question: String,
    pub answer: Option<String>,
    /// An unsettled nudge answering it, or an attempt record that cannot
    /// be read: either way the block is unresolved, its outcome unknown.
    pub nudge: Option<Stored<Attempt>>,
    /// What of its stored state cannot be read. Such a block is never
    /// answered, retried, owed or received until a person repairs it.
    pub unreadable: Option<String>,
}

impl BlockEntry {
    /// `plain`, `permission`, or `unreadable` when its kind cannot be read.
    pub fn kind_name(&self) -> &'static str {
        self.kind.valid().map_or("unreadable", Kind::name)
    }

    pub fn suggestion(&self) -> Option<&str> {
        self.kind.valid().and_then(Kind::suggestion)
    }

    pub fn is_permission(&self) -> bool {
        matches!(self.kind, Stored::Valid(Kind::Permission { .. }))
    }
}

impl Stored<Attempt> {
    /// Why the nudge it records is unresolved, as every surface says it.
    pub fn unknown_reason(&self) -> String {
        match self {
            Stored::Valid(a) => a.unknown_reason(),
            Stored::Unreadable(why) => format!("nudge outcome unknown: {why}"),
        }
    }
}

/// Every block not yet received, pending or answered, and every one
/// whose state cannot be read. Read-only: it
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
            let Some(pair) =
                Pair::read(&dir, &name).filter(|p| !p.received || p.damage().is_some())
            else {
                continue;
            };
            let unreadable = pair.damage();
            out.push(BlockEntry {
                agent: agent.clone(),
                name,
                occurrence: pair.occurrence,
                kind: pair.kind,
                question: pair.question,
                answer: pair.answer,
                nudge: match pair.attempt {
                    AttemptState::Live(a) => Some(Stored::Valid(a)),
                    AttemptState::Unreadable(why) => Some(Stored::Unreadable(why)),
                    AttemptState::Absent | AttemptState::Inert => None,
                },
                unreadable,
            });
        }
    }
    out.sort_by(|a, b| a.agent.cmp(&b.agent).then(a.name.cmp(&b.name)));
    out
}

/// How a block is named wherever one is said to withhold work.
pub(crate) fn key(agent: &str, name: &str) -> String {
    format!("{agent}/{name}")
}

/// When each block that has stopped withholding work from `author` did
/// so, by [`key`], in unix seconds: another agent's block when it was
/// answered, `author`'s own when the answer was received. Read from the
/// answer and receipt files' own times, so it is the same whoever asks and
/// whenever. A block cleaned away is not here: its end cannot be dated.
pub(crate) fn ended(
    repo: &Path,
    author: &clank_core::AgentLabel,
) -> std::collections::BTreeMap<String, u64> {
    let written = |path: &Path| {
        let at = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        Some(at.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
    };
    let mut out = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(agents_root(repo))
        .into_iter()
        .flatten()
        .flatten()
    {
        let Some(agent) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let dir = entry.path();
        for name in question_names(&dir) {
            let Some(pair) = Pair::read(&dir, &name).filter(|p| p.damage().is_none()) else {
                continue;
            };
            let f = files(&dir, &name);
            let at = match (agent == author.as_str(), pair.received, &pair.answer) {
                (true, true, _) => written(&f.marker),
                (false, _, Some(_)) => written(&f.answer),
                _ => None,
            };
            if let Some(at) = at {
                out.insert(key(&agent, &name), at);
            }
        }
    }
    out
}

/// An answer `agent` is owed: answered and not yet received.
pub(crate) struct OwedAnswer {
    pub(crate) answer: String,
    pub(crate) carries: AnswerRef,
}

/// `agent`'s owed answers, by name. Read-only, like [`scan_blocks`].
/// A nudged answer is never owed: the agent saw it typed. Nor is one
/// whose disposition cannot be read: it may have been.
pub(crate) fn owed_answers(repo: &Path, agent: &clank_core::AgentLabel) -> Vec<OwedAnswer> {
    let dir = agents_root(repo).join(agent.as_str());
    question_names(&dir)
        .into_iter()
        .filter_map(|name| {
            let pair = Pair::read(&dir, &name).filter(|p| !p.received && p.damage().is_none())?;
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

    #[tokio::test]
    async fn pause_round_trips_through_exactly_one_flat_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::cli::block::run(crate::cli::BlockArgs {
            command: crate::cli::BlockCmd::Ask(crate::cli::BlockCreateArgs {
                name: "pause".into(),
                message: "held for review".into(),
                author: Some("claude".into()),
                repo: Some(root.to_path_buf()),
                permission: false,
                suggest: None,
                suggest_file: None,
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
            message: Some("carry on".into()),
            nudge: false,
            resend: false,
            delivered: false,
            override_permission_classifier: false,
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
            message: Some("answered just this one".into()),
            nudge: false,
            resend: false,
            delivered: false,
            override_permission_classifier: false,
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
            "--dry-run must not delete the question it reports"
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

    fn written_at(path: &Path, secs: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    /// A block stops withholding work from everyone else when it is
    /// answered, and from its own asker only when the answer is received.
    /// Both moments are the files' own, so they read the same to a
    /// monitor that starts long after; a pending block has ended for
    /// nobody, and one cleaned away dates nothing.
    #[test]
    fn a_blocks_end_is_dated_by_its_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let bob = clank_core::AgentLabel::parse("bob").unwrap();
        ask(repo, &alice(), "pending", "Still open?").unwrap();
        assert!(
            ended(repo, &bob).is_empty(),
            "a pending block has not ended"
        );

        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        written_at(&repo.join(".clank/agents/alice/unblocks/q.md"), 7000);
        assert_eq!(
            ended(repo, &bob),
            [(key("alice", "q"), 7000)].into(),
            "for another agent, at the answer"
        );
        assert!(
            ended(repo, &alice()).is_empty(),
            "for its asker, not before the answer is received"
        );

        assert_eq!(receive(repo, &a, || true).unwrap(), Receipt::Retired);
        written_at(&marker(repo, "q"), 7600);
        assert_eq!(ended(repo, &alice()), [(key("alice", "q"), 7600)].into());
        assert_eq!(ended(repo, &bob)[&key("alice", "q")], 7000);

        clean(repo, &alice()).unwrap();
        assert!(ended(repo, &alice()).is_empty(), "cleaned away: undatable");
        assert!(ended(repo, &bob).is_empty());
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
                identities: Vec::new(),
            };
            let authorized = AtomicBool::new(true);
            let (observed, seen) = std::sync::mpsc::channel();
            let held = lock(repo, &alice()).unwrap();
            std::thread::scope(|s| {
                s.spawn(|| {
                    crate::cli::presentation::record_delivered(
                        repo,
                        &alice(),
                        clank_core::Tool::Claude,
                        "s1",
                        &carried,
                        || {
                            let _ = observed.send(());
                            authorized.load(Ordering::SeqCst)
                        },
                    )
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
        let (q, a) = (unstamp(&qo), unstamp(&ao));
        assert!(q.occurrence.is_some() && q.occurrence == a.occurrence);
        assert_eq!((q.body, a.body), ("Proceed?", "yes"));
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
            &stamp("0123456789abcdef0123456789abcdef", None, "yes"),
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
        assert!(unstamp(&raw).occurrence.is_some(), "{raw}");
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(carries[0].occurrence.as_deref(), unstamp(&raw).occurrence);
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
            &stamp("0123456789abcdef0123456789abcdef", None, "stale"),
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

    fn permission(repo: &Path, name: &str, suggestion: Option<&str>) {
        ask_with(
            repo,
            &alice(),
            name,
            "git push fork was refused; I need it to publish the repro",
            &Kind::Permission {
                suggestion: suggestion.map(str::to_owned),
            },
        )
        .unwrap();
    }

    fn nudge(
        repo: &Path,
        name: &str,
        text: &str,
        via: &mut dyn Nudger,
    ) -> anyhow::Result<Resolved> {
        resolve(
            repo,
            "alice",
            name,
            None,
            By::Person,
            How::Nudge {
                text,
                resend: false,
                via,
            },
        )
    }

    fn resend(
        repo: &Path,
        name: &str,
        text: &str,
        via: &mut dyn Nudger,
    ) -> anyhow::Result<Resolved> {
        resolve(
            repo,
            "alice",
            name,
            None,
            By::Person,
            How::Nudge {
                text,
                resend: true,
                via,
            },
        )
    }

    fn delivered(repo: &Path, name: &str) -> anyhow::Result<Resolved> {
        resolve(repo, "alice", name, None, By::Person, How::Delivered)
    }

    fn attempt_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/agents/alice/blocks/{name}.attempt"))
    }

    fn answer_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/agents/alice/unblocks/{name}.md"))
    }

    fn record(repo: &Path, name: &str) -> Option<Attempt> {
        read_attempt(&attempt_path(repo, name)).and_then(|r| r.valid().cloned())
    }

    fn the_block(repo: &Path) -> BlockEntry {
        let blocks = scan_blocks(repo);
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        blocks.into_iter().next().unwrap()
    }

    fn submitted() -> Outcome {
        Outcome::Submitted
    }

    fn refused() -> Outcome {
        Outcome::Refused {
            reason: "no pane named `alice`".into(),
        }
    }

    fn unknown() -> Outcome {
        Outcome::Indeterminate {
            reason: "the prompt was not confirmed within 5s".into(),
        }
    }

    fn never(text: &str) -> Outcome {
        panic!("nothing may be typed here, yet {text:?} reached the terminal")
    }

    /// The answer as written: its occurrence, its attempt when nudged,
    /// and its text.
    fn written(repo: &Path, name: &str) -> (Option<String>, Option<String>, String) {
        let raw = std::fs::read_to_string(answer_path(repo, name)).unwrap();
        let s = unstamp(&raw);
        let Stored::Valid(disposition) = s.disposition() else {
            panic!("an unreadable disposition: {raw}");
        };
        let attempt = disposition.map(|Disposition::Nudged { attempt }| attempt);
        (s.occurrence.map(str::to_owned), attempt, s.body.to_string())
    }

    #[test]
    fn old_question_and_answer_bytes_parse_as_they_always_did() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let id = "0123456789abcdef0123456789abcdef";
        write(
            &repo.join(".clank/agents/alice/blocks/q.md"),
            &format!("occurrence: {id}\nWhich option?\nsecond line"),
        );
        write(&answer_path(repo, "q"), &format!("occurrence: {id}\nB"));
        let b = the_block(repo);
        assert_eq!(b.occurrence.as_deref(), Some(id));
        assert_eq!(b.kind, Stored::Valid(Kind::Plain));
        assert_eq!(b.question, "Which option?\nsecond line");
        assert_eq!(b.answer.as_deref(), Some("B"));
        assert_eq!(b.nudge, None);
        assert_eq!(owed(repo).len(), 1, "a plain answer is still owed");

        ask(repo, &alice(), "fresh", "Proceed?").unwrap();
        let raw =
            std::fs::read_to_string(repo.join(".clank/agents/alice/blocks/fresh.md")).unwrap();
        let (header, body) = raw.split_once('\n').unwrap();
        assert!(
            header.starts_with(STAMP) && is_occurrence(&header[STAMP.len()..]),
            "a plain ask is written in the old format: {raw}"
        );
        assert_eq!(body, "Proceed?");
    }

    /// Test 1, the store half: the kind and suggestion live in the
    /// question's header, outside the question text.
    #[tokio::test]
    async fn a_permission_ask_stores_its_kind_and_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let suggestion = dir.path().join("suggestion.txt");
        std::fs::write(&suggestion, "yes: push repro-ci-experiment to the fork\n").unwrap();
        for (name, inline, file) in [
            (
                "inline",
                Some("yes: push repro-ci-experiment to the fork"),
                None,
            ),
            ("file", None, Some(suggestion.clone())),
        ] {
            run(crate::cli::BlockArgs {
                command: crate::cli::BlockCmd::Ask(crate::cli::BlockCreateArgs {
                    name: name.into(),
                    message: "git push fork was refused".into(),
                    author: Some("alice".into()),
                    repo: Some(repo.to_path_buf()),
                    permission: true,
                    suggest: inline.map(str::to_owned),
                    suggest_file: file,
                }),
            })
            .await
            .unwrap();
        }
        for b in scan_blocks(repo) {
            assert_eq!(
                b.kind,
                Stored::Valid(Kind::Permission {
                    suggestion: Some("yes: push repro-ci-experiment to the fork".into())
                }),
                "{}",
                b.name
            );
            assert_eq!(b.question, "git push fork was refused", "{}", b.name);
        }
    }

    #[test]
    fn the_cli_spells_the_new_flags_and_their_constraints() {
        use clap::Parser as _;
        let parse = |args: &[&str]| {
            crate::cli::command::Cli::try_parse_from(
                std::iter::once("clank").chain(args.iter().copied()),
            )
        };
        assert!(
            parse(&[
                "block",
                "ask",
                "q",
                "-m",
                "x",
                "--permission",
                "--suggest",
                "yes"
            ])
            .is_ok()
        );
        assert!(parse(&["block", "ask", "q", "-m", "x", "--suggest", "yes"]).is_err());
        assert!(parse(&["block", "answer", "alice", "q", "--nudge", "-m", "yes"]).is_ok());
        assert!(
            parse(&[
                "block", "answer", "alice", "q", "--nudge", "--resend", "-m", "yes"
            ])
            .is_ok()
        );
        assert!(parse(&["block", "answer", "alice", "q", "--resend", "-m", "yes"]).is_err());
        assert!(parse(&["block", "answer", "alice", "q", "--delivered"]).is_ok());
        assert!(parse(&["block", "answer", "alice", "q", "--delivered", "-m", "x"]).is_err());
        assert!(
            parse(&["block", "answer", "alice", "q"]).is_err(),
            "-m is still required"
        );
    }

    /// Test 2: refused by the shared operation, however it is reached.
    #[tokio::test]
    async fn a_plain_answer_to_a_permission_block_is_refused_and_it_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", Some("yes"));
        let cli = run_unblock(crate::cli::UnblockArgs {
            agent: "alice".into(),
            name: "push".into(),
            message: Some("yes".into()),
            nudge: false,
            resend: false,
            delivered: false,
            override_permission_classifier: false,
            repo: Some(repo.to_path_buf()),
        })
        .await
        .unwrap_err()
        .to_string();
        let shown = the_block(repo);
        let form = answer_block(
            repo,
            "alice",
            "push",
            "yes",
            Some(Shown {
                occurrence: shown.occurrence.as_deref(),
                question: &shown.question,
            }),
        )
        .unwrap_err()
        .to_string();
        for err in [cli, form] {
            assert!(
                err.contains("permission block") && err.contains("--nudge"),
                "{err}"
            );
        }
        let b = the_block(repo);
        assert_eq!((b.answer, b.nudge), (None, None));
        assert!(!answer_path(repo, "push").exists());
        assert!(!attempt_path(repo, "push").exists());
    }

    /// Test 3: one write makes the answer recorded and received, so no
    /// read after it can present it as owed.
    #[test]
    fn a_submitted_nudge_records_one_nudged_answer_that_is_never_owed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", Some("yes: push it"));
        let occurrence = the_block(repo).occurrence;
        let mut typed = Vec::new();
        let mut claim_seen = None;
        let resolved = nudge(repo, "push", "  yes: push it  ", &mut |text: &str| {
            typed.push(text.to_string());
            claim_seen = record(repo, "push");
            submitted()
        })
        .unwrap();
        assert!(matches!(resolved, Resolved::Answered(_)), "{resolved:?}");
        assert_eq!(typed, ["yes: push it"]);
        let claim = claim_seen.expect("the claim is published before anything is typed");
        let (o, attempt, body) = written(repo, "push");
        assert_eq!(o, occurrence);
        assert_eq!(attempt.as_deref(), Some(claim.attempt.as_str()));
        assert_eq!(body, "yes: push it");
        assert!(!marker(repo, "q").exists() && !marker(repo, "push").exists());
        assert!(owed(repo).is_empty());
        assert!(scan_blocks(repo).is_empty(), "neither pending nor owed");
        assert!(record(repo, "push").is_none());
        let history = crate::cli::nudge_log::read(&repo.join(".clank/agents/alice"));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].attempt, claim.attempt);
        assert_eq!(
            history[0].trigger,
            crate::cli::nudge_log::Trigger::Block {
                name: "push".into()
            }
        );

        // The instant after the answer write, before the record goes.
        publish(
            &attempt_path(repo, "push"),
            &serde_json::to_vec(&claim).unwrap(),
        )
        .unwrap();
        assert!(owed(repo).is_empty());
        assert!(scan_blocks(repo).is_empty());
    }

    /// Test 4.
    #[test]
    fn of_two_racing_answerers_exactly_one_reaches_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let mut second = None;
        let mut plain = None;
        nudge(repo, "push", "first", &mut |_: &str| {
            second = Some(nudge(repo, "push", "second", &mut never));
            plain = Some(resolve(
                repo,
                "alice",
                "push",
                None,
                By::Person,
                How::Plain("plain"),
            ));
            submitted()
        })
        .unwrap();
        let err = second.unwrap().unwrap_err().to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(plain.unwrap().is_err());
        assert_eq!(written(repo, "push").2, "first");

        for kind in [Kind::Plain, Kind::Permission { suggestion: None }] {
            ask_with(repo, &alice(), "race", "Proceed?", &kind).unwrap();
            let reached = std::sync::atomic::AtomicUsize::new(0);
            let start = std::sync::Barrier::new(2);
            let results: Vec<_> = std::thread::scope(|s| {
                let racers: Vec<_> = (0..2)
                    .map(|i| {
                        let (reached, start) = (&reached, &start);
                        s.spawn(move || {
                            start.wait();
                            nudge(repo, "race", &format!("racer {i}"), &mut |_: &str| {
                                reached.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                submitted()
                            })
                        })
                    })
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            assert_eq!(
                reached.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{kind:?}"
            );
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{kind:?}");
        }
    }

    /// Test 5.
    #[test]
    fn a_stale_or_re_asked_occurrence_is_never_answered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let old = the_block(repo);
        permission(repo, "push", None);
        let err = resolve(
            repo,
            "alice",
            "push",
            Some(Shown {
                occurrence: old.occurrence.as_deref(),
                question: &old.question,
            }),
            By::Person,
            How::Nudge {
                text: "yes",
                resend: false,
                via: &mut never,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed"), "{err}");
        assert!(record(repo, "push").is_none(), "nothing was claimed");

        let resolved = nudge(repo, "push", "yes", &mut |_: &str| {
            permission(repo, "push", None);
            submitted()
        })
        .unwrap();
        assert!(matches!(resolved, Resolved::Superseded(_)), "{resolved:?}");
        let b = the_block(repo);
        assert_eq!((b.answer, b.nudge), (None, None), "the new ask stays open");
        assert!(!answer_path(repo, "push").exists());
    }

    /// Test 6.
    #[test]
    fn a_nudge_that_typed_nothing_leaves_the_block_open() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let resolved = nudge(repo, "push", "yes", &mut |_: &str| refused()).unwrap();
        assert!(
            matches!(&resolved, Resolved::NotSent(why) if why.contains("no pane")),
            "{resolved:?}"
        );
        assert!(record(repo, "push").is_none());
        assert!(!answer_path(repo, "push").exists());
        let b = the_block(repo);
        assert_eq!((b.answer, b.nudge), (None, None));
        let mut again = 0;
        nudge(repo, "push", "yes", &mut |_: &str| {
            again += 1;
            submitted()
        })
        .unwrap();
        assert_eq!(again, 1, "nothing outstanding refuses the next try");
    }

    /// Test 7.
    #[test]
    fn an_unconfirmed_nudge_stays_visibly_unresolved_and_is_not_resent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let resolved = nudge(repo, "push", "yes", &mut |_: &str| unknown()).unwrap();
        assert!(
            matches!(&resolved, Resolved::Unknown(why) if why.contains("not confirmed")),
            "{resolved:?}"
        );
        let b = the_block(repo);
        assert_eq!(b.answer, None);
        let n = b
            .nudge
            .and_then(|n| n.valid().cloned())
            .expect("unresolved");
        assert_eq!(n.text, "yes");
        assert_eq!(
            n.diagnostic.as_deref(),
            Some("the prompt was not confirmed within 5s")
        );
        assert!(n.unknown_reason().starts_with("nudge outcome unknown: "));
        let err = nudge(repo, "push", "yes", &mut never)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--delivered") && err.contains("--resend"),
            "{err}"
        );
        assert!(!answer_path(repo, "push").exists());
    }

    /// Test 8.
    #[test]
    fn a_failed_answer_write_after_delivery_is_settled_by_delivered_without_typing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let unblocks = repo.join(".clank/agents/alice/unblocks");
        let err = nudge(repo, "push", "yes", &mut |_: &str| {
            let _ = std::fs::remove_dir_all(&unblocks);
            std::fs::write(&unblocks, "not a directory").unwrap();
            submitted()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("--delivered"), "{err:#}");
        let claim = record(repo, "push").expect("the record outlives the failed write");
        assert_eq!(claim.diagnostic, None);
        assert!(the_block(repo).nudge.is_some(), "shown unresolved");

        std::fs::remove_file(&unblocks).unwrap();
        assert!(matches!(
            delivered(repo, "push").unwrap(),
            Resolved::Answered(_)
        ));
        let (o, attempt, body) = written(repo, "push");
        assert_eq!(o.as_deref(), Some(claim.occurrence.as_str()));
        assert_eq!(attempt.as_deref(), Some(claim.attempt.as_str()));
        assert_eq!(body, "yes");
        assert!(record(repo, "push").is_none());
        assert!(owed(repo).is_empty() && scan_blocks(repo).is_empty());
    }

    /// Test 9.
    #[test]
    fn only_resend_types_again_for_an_outstanding_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        nudge(repo, "push", "yes", &mut |_: &str| unknown()).unwrap();
        let first = record(repo, "push").unwrap();
        assert!(nudge(repo, "push", "yes", &mut never).is_err());
        let mut typed = Vec::new();
        let resolved = resend(repo, "push", "yes, again", &mut |t: &str| {
            typed.push(t.to_string());
            submitted()
        })
        .unwrap();
        assert!(matches!(resolved, Resolved::Answered(_)), "{resolved:?}");
        assert_eq!(typed, ["yes, again"]);
        let (_, attempt, body) = written(repo, "push");
        assert_ne!(
            attempt.as_deref(),
            Some(first.attempt.as_str()),
            "a fresh claim"
        );
        assert_eq!(body, "yes, again");
        assert!(
            delivered(repo, "push").is_err(),
            "nothing is left to settle"
        );
    }

    /// Test 10.
    #[test]
    fn a_late_delivery_after_a_resend_records_and_removes_nothing() {
        for resend_outcome in [submitted(), unknown()] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            permission(repo, "push", None);
            let mut after_resend = None;
            let late = nudge(repo, "push", "first", &mut |_: &str| {
                resend(repo, "push", "second", &mut |_: &str| {
                    resend_outcome.clone()
                })
                .unwrap();
                after_resend = Some((
                    record(repo, "push"),
                    std::fs::read(answer_path(repo, "push")).ok(),
                ));
                submitted()
            })
            .unwrap();
            assert!(matches!(late, Resolved::Superseded(_)), "{late:?}");
            let (rec, ans) = after_resend.unwrap();
            assert_eq!(
                record(repo, "push"),
                rec,
                "{resend_outcome:?}: the resend's record"
            );
            assert_eq!(
                std::fs::read(answer_path(repo, "push")).ok(),
                ans,
                "{resend_outcome:?}: the resend's answer"
            );
            if resend_outcome == Outcome::Submitted {
                assert_eq!(written(repo, "push").2, "second");
            } else {
                assert_eq!(rec.unwrap().text, "second");
            }
            let history = crate::cli::nudge_log::read(&repo.join(".clank/agents/alice"));
            assert_eq!(history.len(), 2, "the late delivery is still reported");
        }
    }

    /// Test 11.
    #[test]
    fn an_old_attempts_settle_after_a_re_ask_leaves_the_new_ask_alone() {
        for (old_outcome, claim_new) in [
            (submitted(), true),
            (refused(), true),
            (unknown(), true),
            (submitted(), false),
            (refused(), false),
            (unknown(), false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            permission(repo, "push", None);
            let mut new_claim = None;
            nudge(repo, "push", "old", &mut |_: &str| {
                permission(repo, "push", None);
                if claim_new {
                    nudge(repo, "push", "new", &mut |_: &str| unknown()).unwrap();
                    new_claim = record(repo, "push");
                }
                old_outcome.clone()
            })
            .unwrap();
            let case = format!("{old_outcome:?}, new claim {claim_new}");
            let b = the_block(repo);
            assert_eq!(b.answer, None, "{case}");
            assert!(!answer_path(repo, "push").exists(), "{case}");
            assert_eq!(b.nudge, new_claim.clone().map(Stored::Valid), "{case}");
            if claim_new {
                assert_eq!(record(repo, "push"), new_claim, "{case}");
            }
            let mut fresh = 0;
            let next = resend(repo, "push", "now", &mut |_: &str| {
                fresh += 1;
                submitted()
            });
            assert!(
                matches!(next, Ok(Resolved::Answered(_))),
                "{case}: {next:?}"
            );
            assert_eq!(fresh, 1, "{case}");
        }
    }

    /// Test 12.
    #[test]
    fn an_interrupted_cleanup_resolves_to_the_nudged_answer() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let mut claim = None;
        nudge(repo, "push", "yes", &mut |_: &str| {
            claim = record(repo, "push");
            submitted()
        })
        .unwrap();
        publish(
            &attempt_path(repo, "push"),
            &serde_json::to_vec(&claim.unwrap()).unwrap(),
        )
        .unwrap();
        let answer = std::fs::read(answer_path(repo, "push")).unwrap();
        assert!(scan_blocks(repo).is_empty(), "answered, never reopened");
        assert!(owed(repo).is_empty());

        let err = nudge(repo, "push", "yes", &mut never)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already answered"), "{err}");
        assert!(!attempt_path(repo, "push").exists(), "recovery removed it");
        assert_eq!(std::fs::read(answer_path(repo, "push")).unwrap(), answer);
        for how in [How::Delivered, How::Plain("again")] {
            assert!(resolve(repo, "alice", "push", None, By::Person, how).is_err());
        }
        assert_eq!(std::fs::read(answer_path(repo, "push")).unwrap(), answer);
        assert_eq!(
            crate::cli::nudge_log::read(&repo.join(".clank/agents/alice")).len(),
            1,
            "nothing was sent again"
        );
    }

    /// Test 13.
    #[test]
    fn an_ordinary_block_answers_by_nudge_or_plainly_unless_a_nudge_is_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        ask(repo, &alice(), "q", "Which?").unwrap();
        let mut typed = Vec::new();
        nudge(repo, "q", "B", &mut |t: &str| {
            typed.push(t.to_string());
            submitted()
        })
        .unwrap();
        assert_eq!(typed, ["B"]);
        assert!(written(repo, "q").1.is_some(), "recorded as nudged");
        assert!(owed(repo).is_empty(), "the agent saw it typed");

        ask(repo, &alice(), "q", "Which?").unwrap();
        nudge(repo, "q", "B", &mut |_: &str| unknown()).unwrap();
        let err = answer_block(repo, "alice", "q", "C", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(matches!(
            delivered(repo, "q").unwrap(),
            Resolved::Answered(_)
        ));

        ask(repo, &alice(), "q", "Which?").unwrap();
        answer_block(repo, "alice", "q", "C", None).unwrap();
        assert_eq!(
            written(repo, "q"),
            (the_block(repo).occurrence, None, "C".into())
        );
        assert_eq!(owed(repo).len(), 1, "a plain answer is owed as before");
    }

    #[test]
    fn clean_keeps_an_outstanding_attempt_and_removes_inert_ones() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "live", None);
        nudge(repo, "live", "yes", &mut |_: &str| unknown()).unwrap();
        permission(repo, "stale", None);
        nudge(repo, "stale", "yes", &mut |_: &str| unknown()).unwrap();
        permission(repo, "stale", None);
        let orphan = std::fs::read(attempt_path(repo, "live")).unwrap();
        std::fs::write(attempt_path(repo, "gone"), orphan).unwrap();
        write(&attempt_path(repo, "torn"), "{\"occurrence\":");
        assert_eq!(clean(repo, &alice()).unwrap(), 2);
        assert!(record(repo, "live").is_some());
        assert!(!attempt_path(repo, "stale").exists());
        assert!(!attempt_path(repo, "gone").exists());
        assert!(
            attempt_path(repo, "torn").exists(),
            "an unreadable record may be an attempt in flight"
        );
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn question_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/agents/alice/blocks/{name}.md"))
    }

    /// Every How, as a person: each is refused before anything is typed.
    fn every_answer_refused(repo: &Path, name: &str) {
        let tries: [(&str, anyhow::Result<Resolved>); 4] = [
            (
                "plain",
                resolve(repo, "alice", name, None, By::Person, How::Plain("yes")),
            ),
            ("nudge", nudge(repo, name, "yes", &mut never)),
            ("resend", resend(repo, name, "yes", &mut never)),
            ("delivered", delivered(repo, name)),
        ];
        for (how, tried) in tries {
            let err = tried.expect_err(how).to_string();
            assert!(err.contains("cannot read"), "{how}: {err}");
        }
    }

    /// A header with metadata that does not parse is unreadable, never
    /// taken for the plain legacy format.
    #[tokio::test]
    async fn an_unreadable_kind_refuses_every_answer_and_stays_visible() {
        for meta in [
            r#"{"kind":"permission","#,
            r#"{"kind":"telepathy"}"#,
            "{}",
            "nonsense",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            write(
                &question_path(repo, "push"),
                &format!("occurrence: {ID} {meta}\ngit push fork was refused"),
            );
            let b = the_block(repo);
            assert!(b.kind.unreadable().is_some(), "{meta}: {:?}", b.kind);
            assert_eq!(b.kind_name(), "unreadable", "{meta}");
            assert!(b.unreadable.is_some() && b.answer.is_none(), "{meta}");
            let cli = run_unblock(crate::cli::UnblockArgs {
                agent: "alice".into(),
                name: "push".into(),
                message: Some("yes".into()),
                nudge: false,
                resend: false,
                delivered: false,
                override_permission_classifier: true,
                repo: Some(repo.to_path_buf()),
            })
            .await;
            assert!(cli.is_err(), "{meta}");
            every_answer_refused(repo, "push");
            assert!(!answer_path(repo, "push").exists(), "{meta}");
            assert!(!attempt_path(repo, "push").exists(), "{meta}");
        }
    }

    #[test]
    fn an_unreadable_question_keeps_its_attempt_record() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        nudge(repo, "push", "yes", &mut |_: &str| unknown()).unwrap();
        let mut raw = std::fs::read(question_path(repo, "push")).unwrap();
        raw.extend([0xff, 0xfe]);
        std::fs::write(question_path(repo, "push"), raw).unwrap();
        let b = the_block(repo);
        assert!(b.kind.unreadable().is_some());
        every_answer_refused(repo, "push");
        clean(repo, &alice()).unwrap();
        assert!(record(repo, "push").is_some(), "never removed");
    }

    /// An answer whose disposition cannot be read may have been typed:
    /// it is shown answered and unreadable, and is never owed.
    #[test]
    fn an_unreadable_disposition_is_shown_answered_and_never_owed() {
        for meta in [
            r#"{"disposition":"nudged""#,
            r#"{"disposition":"carrier-pigeon"}"#,
            "garbled",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            write(
                &question_path(repo, "q"),
                &format!("occurrence: {ID}\nWhich?"),
            );
            write(
                &answer_path(repo, "q"),
                &format!("occurrence: {ID} {meta}\nB"),
            );
            let b = the_block(repo);
            assert_eq!(b.answer.as_deref(), Some("B"), "{meta}");
            assert!(b.unreadable.is_some(), "{meta}");
            assert!(owed(repo).is_empty(), "{meta}");
            every_answer_refused(repo, "q");
            assert_eq!(clean(repo, &alice()).unwrap(), 0, "{meta}");
            assert!(answer_path(repo, "q").exists(), "{meta}");
        }
    }

    /// Every writer ends the header line, so a header that names an
    /// occurrence but stops before its newline was cut short: it is
    /// unreadable, never the unstamped legacy format.
    #[tokio::test]
    async fn a_cut_short_header_is_unreadable_not_legacy() {
        for header in [
            format!(r#"occurrence: {ID} {{"kind":"permission""#),
            format!("occurrence: {ID}"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            write(&question_path(repo, "push"), &header);
            let b = the_block(repo);
            assert!(b.kind.unreadable().is_some(), "{header}: {:?}", b.kind);
            let cli = run_unblock(crate::cli::UnblockArgs {
                agent: "alice".into(),
                name: "push".into(),
                message: Some("yes".into()),
                nudge: false,
                resend: false,
                delivered: false,
                override_permission_classifier: false,
                repo: Some(repo.to_path_buf()),
            })
            .await;
            assert!(cli.is_err(), "{header}");
            every_answer_refused(repo, "push");
            clean(repo, &alice()).unwrap();
            assert_eq!(
                std::fs::read_to_string(question_path(repo, "push")).unwrap(),
                header,
                "kept"
            );
            assert!(!answer_path(repo, "push").exists(), "{header}");
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(
            &question_path(repo, "q"),
            &format!("occurrence: {ID}\nWhich?"),
        );
        let torn = format!(r#"occurrence: {ID} {{"disposition":"nudged""#);
        write(&answer_path(repo, "q"), &torn);
        let b = the_block(repo);
        assert!(b.unreadable.is_some());
        assert!(owed(repo).is_empty());
        every_answer_refused(repo, "q");
        assert_eq!(clean(repo, &alice()).unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(answer_path(repo, "q")).unwrap(),
            torn
        );
    }

    #[test]
    fn an_unreadable_answer_file_is_never_answered_over() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        ask(repo, &alice(), "q", "Which?").unwrap();
        std::fs::create_dir_all(answer_path(repo, "q")).unwrap();
        let b = the_block(repo);
        assert!(b.unreadable.is_some());
        assert!(owed(repo).is_empty());
        every_answer_refused(repo, "q");
    }

    /// An attempt record that is malformed, or that cannot be read at all,
    /// may be a nudge in flight: it keeps the block unresolved, refuses
    /// every answer and retry, and survives every write.
    #[test]
    fn an_unreadable_attempt_keeps_the_block_unresolved_and_survives() {
        for damage in ["malformed", "unreadable"] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            permission(repo, "push", None);
            let path = attempt_path(repo, "push");
            match damage {
                "malformed" => write(&path, "{\"occurrence\":"),
                _ => std::fs::create_dir_all(&path).unwrap(),
            }
            let b = the_block(repo);
            let nudge = b.nudge.clone().expect(damage);
            assert!(nudge.unreadable().is_some(), "{damage}");
            assert!(
                nudge
                    .unknown_reason()
                    .starts_with("nudge outcome unknown: "),
                "{damage}"
            );
            assert!(b.unreadable.is_some() && b.answer.is_none(), "{damage}");
            every_answer_refused(repo, "push");
            clean(repo, &alice()).unwrap();
            assert!(path.exists(), "{damage}: never removed");
        }
    }

    /// A settle never takes an unreadable record for its own.
    #[test]
    fn a_settle_leaves_an_unreadable_record_alone() {
        for outcome in [submitted(), refused(), unknown()] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            permission(repo, "push", None);
            let resolved = nudge(repo, "push", "yes", &mut |_: &str| {
                write(&attempt_path(repo, "push"), "torn");
                outcome.clone()
            });
            assert_eq!(
                std::fs::read_to_string(attempt_path(repo, "push")).unwrap(),
                "torn",
                "{outcome:?}: {resolved:?}"
            );
            assert!(!answer_path(repo, "push").exists(), "{outcome:?}");
        }
    }

    /// A question damaged while its nudge was in flight records no answer
    /// over the damage.
    #[test]
    fn a_submitted_settle_records_nothing_over_unreadable_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let raw = std::fs::read_to_string(question_path(repo, "push")).unwrap();
        let err = nudge(repo, "push", "yes", &mut |_: &str| {
            let (header, body) = raw.split_once('\n').unwrap();
            let id = &header[STAMP.len()..STAMP.len() + 32];
            write(
                &question_path(repo, "push"),
                &format!("occurrence: {id} {{torn\n{body}"),
            );
            submitted()
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("cannot read"), "{err:#}");
        assert!(!answer_path(repo, "push").exists());
        assert!(
            record(repo, "push").is_some(),
            "kept for the person to settle"
        );
    }

    #[test]
    fn a_receipt_never_retires_unreadable_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let carries = asked_and_answered(repo, "q", "Which?", "B");
        write(
            &answer_path(repo, "q"),
            &format!(
                "occurrence: {} {{torn\nB",
                carries.occurrence.clone().unwrap()
            ),
        );
        assert_eq!(receive(repo, &carries, || true).unwrap(), Receipt::Stale);
        assert!(!marker(repo, "q").exists());
    }

    /// A delivered answer beside an unreadable record is not tidied away:
    /// the record may be another attempt's.
    #[test]
    fn a_nudged_answer_beside_an_unreadable_record_stays_visible() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        nudge(repo, "push", "yes", &mut |_: &str| submitted()).unwrap();
        write(&attempt_path(repo, "push"), "torn");
        let b = the_block(repo);
        assert_eq!(b.answer.as_deref(), Some("yes"));
        assert!(b.unreadable.is_some());
        assert_eq!(clean(repo, &alice()).unwrap(), 0);
        assert!(question_path(repo, "push").exists() && attempt_path(repo, "push").exists());
    }

    /// Answering a permission block needs a person's override; an
    /// ordinary block does not.
    #[test]
    fn a_permission_block_is_resolved_only_by_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let unattested = |how: How<'_>| resolve(repo, "alice", "push", None, By::Unattested, how);
        for how in [
            How::Nudge {
                text: "yes",
                resend: false,
                via: &mut never,
            },
            How::Nudge {
                text: "yes",
                resend: true,
                via: &mut never,
            },
        ] {
            let err = unattested(how).unwrap_err().to_string();
            assert!(
                err.contains("--override-permission-classifier") && err.contains("person"),
                "{err}"
            );
        }
        assert!(record(repo, "push").is_none(), "nothing was claimed");
        nudge(repo, "push", "yes", &mut |_: &str| unknown()).unwrap();
        let err = unattested(How::Delivered).unwrap_err().to_string();
        assert!(err.contains("--override-permission-classifier"), "{err}");
        assert!(matches!(
            delivered(repo, "push").unwrap(),
            Resolved::Answered(_)
        ));

        ask(repo, &alice(), "q", "Which?").unwrap();
        let mut typed = 0;
        let ordinary = resolve(
            repo,
            "alice",
            "q",
            None,
            By::Unattested,
            How::Nudge {
                text: "B",
                resend: false,
                via: &mut |_: &str| {
                    typed += 1;
                    unknown()
                },
            },
        )
        .unwrap();
        assert!(matches!(ordinary, Resolved::Unknown(_)));
        assert_eq!(typed, 1);
        let settled = resolve(repo, "alice", "q", None, By::Unattested, How::Delivered).unwrap();
        assert!(matches!(settled, Resolved::Answered(_)));
    }

    /// The CLI's flag is the override: `--delivered` needs no pane, so it
    /// drives the real command.
    #[tokio::test]
    async fn the_cli_needs_the_override_flag_for_a_permission_block() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        nudge(repo, "push", "yes", &mut |_: &str| unknown()).unwrap();
        let settle = |flag: bool| {
            run_unblock(crate::cli::UnblockArgs {
                agent: "alice".into(),
                name: "push".into(),
                message: None,
                nudge: false,
                resend: false,
                delivered: true,
                override_permission_classifier: flag,
                repo: Some(repo.to_path_buf()),
            })
        };
        let err = settle(false).await.unwrap_err().to_string();
        assert!(err.contains("--override-permission-classifier"), "{err}");
        assert!(record(repo, "push").is_some(), "still outstanding");
        settle(true).await.unwrap();
        assert!(written(repo, "push").1.is_some());

        ask(repo, &alice(), "q", "Which?").unwrap();
        nudge(repo, "q", "B", &mut |_: &str| unknown()).unwrap();
        run_unblock(crate::cli::UnblockArgs {
            agent: "alice".into(),
            name: "q".into(),
            message: None,
            nudge: false,
            resend: false,
            delivered: true,
            override_permission_classifier: false,
            repo: Some(repo.to_path_buf()),
        })
        .await
        .unwrap();
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
