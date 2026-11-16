use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{BlockArgs, BlockCmd, BlockCreateArgs, UnblockArgs, resolve_repo};
use crate::agent_store::agents_root;
use crate::cli::nudge_log::Outcome;
use clank_core::wait::AnswerRef;

pub async fn run(args: BlockArgs) -> anyhow::Result<()> {
    match args.command {
        BlockCmd::Ask(a) => create(a, &crate::agent_env::Process),
        BlockCmd::Answer(a) => run_unblock(a).await,
        BlockCmd::List(a) => list(a.repo.as_deref()),
    }
}

fn root(repo: &Path) -> PathBuf {
    repo.join(".clank")
}

/// No identity is needed to inspect repository questions. A delivered
/// answer is not listed: delivery deletes its block.
fn list(repo: Option<&Path>) -> anyhow::Result<()> {
    let repo = resolve_repo(repo)?;
    for b in scan_blocks(&repo) {
        println!("{}: {}", b.name, b.question);
        if let Some(answer) = &b.answer {
            println!("  answer: {answer}");
        }
        if let Some(why) = &b.unreadable {
            println!("  unresolved: {why}");
        }
    }
    Ok(())
}

pub(crate) fn master(repo: &Path) -> anyhow::Result<clank_core::AgentLabel> {
    let cfg = crate::agent_store::repo_config::read(repo)?.unwrap_or_default();
    super::watch_store::Target::Master
        .recipient(&cfg)
        .ok_or_else(|| anyhow::anyhow!("the repository has no unique master"))
}

/// The user's `blocked` hook may page someone and wait on the answer.
const BLOCK_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Who may raise a block (I7): a person running no agent, or the
/// repository's unique master. Any other agent is refused with what it
/// does instead, and so is a caller whose identity fails to resolve.
/// `Ok(None)` is the person.
fn admit(
    repo: &Path,
    caller: anyhow::Result<crate::agent_env::Identity>,
) -> anyhow::Result<Option<clank_core::AgentLabel>> {
    use crate::agent_env::Identity;
    use clank_core::vocab::Role;
    let label = match caller {
        Ok(Identity::Absent) => return Ok(None),
        Ok(Identity::Agent(label)) => label,
        Err(e) => anyhow::bail!(
            "refusing to raise a block: who is asking did not resolve, and only the master \
             may ask: {e:#}"
        ),
    };
    let cfg = crate::agent_store::repo_config::read(repo)?.unwrap_or_default();
    if super::watch_store::Target::Master.recipient(&cfg).as_ref() == Some(&label) {
        return Ok(Some(label));
    }
    match cfg.agents.get(&label).map(|a| a.role.resolved()) {
        Some(Role::Reviewer) => anyhow::bail!(
            "refusing to raise a block: only the master raises blocks, and `{label}` is a \
             reviewer. Write REQUEST_CHANGES saying what the person must decide; the master \
             raises the block. A `clank send --master` may add to the verdict, never replace it."
        ),
        Some(Role::Master) => anyhow::bail!(
            "refusing to raise a block: the repository has no unique master, and only the \
             master raises blocks"
        ),
        Some(Role::Standby) | None => anyhow::bail!(
            "refusing to raise a block: only the master raises blocks. Tell the master what \
             the person must decide: `clank send --master -m \"<question>\"`."
        ),
    }
}

/// Raise `name` as `caller`, once [`admit`] allows it.
fn raise(
    repo: &Path,
    caller: anyhow::Result<crate::agent_env::Identity>,
    name: &str,
    question: &str,
    kind: &Kind,
) -> anyhow::Result<(Option<clank_core::AgentLabel>, PathBuf)> {
    let author = admit(repo, caller)?;
    let path = ask_with(repo, author.as_ref(), name, question, kind)?;
    Ok((author, path))
}

fn create(args: BlockCreateArgs, env: &dyn crate::agent_env::Ambient) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let caller = crate::agent_env::identify_in(&repo, env);
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
    let (author, path) = raise(&repo, caller, &args.name, &args.message, &kind)?;
    eprintln!("BLOCK: this pauses the repository review cycle until the question is answered");

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
                .env("CLANK_AGENT", author.as_ref().map_or("", |a| a.as_str()))
                .env("CLANK_BLOCK_NAME", &args.name)
                .env("CLANK_REPO", &*repo.to_string_lossy())
                .stdin(Stdin::Inherit)
                .stderr(Out::Inherit),
            BLOCK_HOOK_BUDGET,
        );
    }

    println!(
        "blocked repository as {}",
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
        resolve(&repo, &args.name, None, by, How::Delivered)?
    } else if args.nudge {
        let text = text()?;
        let mut pane = crate::cli::nudge::Pane { repo: &repo };
        crate::cli::event_interrupt::blocking(|| {
            resolve(
                &repo,
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
        resolve(&repo, &args.name, None, by, How::Plain(text()?))?
    };
    match resolved.settled(&args.name)? {
        Some(path) => println!(
            "unblocked repository as {}",
            path.strip_prefix(&repo).unwrap_or(&path).display()
        ),
        None => println!(
            "`{}` is delivered: its question and answer are deleted",
            args.name
        ),
    }
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

/// What a block waits for.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Plain,
    /// A refused permission. Only the person's own message in the agent's
    /// terminal lifts it, so it is never answered by a recorded answer.
    /// The suggestion is the agent's guess at that message, never sent by
    /// itself.
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

/// A fresh ask or attempt identity. Random rather than counted, so no
/// store has to outlive a deleted block for an identity never to come
/// back.
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
    attempt: PathBuf,
    creator: PathBuf,
}

fn files(agent_dir: &Path, name: &str) -> Files {
    Files {
        question: agent_dir.join("blocks").join(format!("{name}.md")),
        answer: agent_dir.join("unblocks").join(format!("{name}.md")),
        attempt: agent_dir.join("blocks").join(format!("{name}.attempt")),
        creator: agent_dir.join("blocks").join(format!("{name}.creator")),
    }
}

fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
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
    /// occurrence, or beside an answer, which no claim can coexist with.
    /// Ignored by every reader, and removed by the next write under the
    /// lock.
    Inert,
    /// Present but unreadable: it may be an attempt in flight, so it
    /// keeps the block unresolved and is never removed.
    Unreadable(String),
}

/// One block name's files as they stand. A block exists exactly while its
/// question does: an answer, creator or attempt without one is inert.
struct Pair {
    occurrence: Option<String>,
    question: String,
    kind: Stored<Kind>,
    /// The answer belonging to this ask, never a leftover from another.
    answer: Option<String>,
    /// Why the answer file cannot be read, when it cannot.
    answer_damage: Option<String>,
    /// Unix seconds, from the same open file the answer was read from.
    answered_at: Option<u64>,
    attempt: AttemptState,
}

/// Where a lock-free read of one name pauses between its steps: tests land
/// a write there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    QuestionRead,
    AnswerRead,
}

/// A question file's identity. Every publish is a rename and a completion
/// a removal, so the same generation before and after a read means no
/// write to the question landed in between.
#[derive(PartialEq, Eq)]
struct Generation {
    dev: u64,
    ino: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl Generation {
    /// `Ok(None)` only when the question path is `NotFound`. A path that
    /// exists but is not a regular file is an error, never followed or
    /// read: it still holds the name, so it must block, not vanish.
    fn of(path: &Path) -> std::io::Result<Option<Generation>> {
        use std::os::unix::fs::MetadataExt as _;
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
            Ok(m) if !m.file_type().is_file() => Err(std::io::Error::other(format!(
                "{} is not a regular file",
                path.display()
            ))),
            Ok(m) => Ok(Some(Generation {
                dev: m.dev(),
                ino: m.ino(),
                len: m.len(),
                modified: m.modified().ok(),
            })),
        }
    }
}

/// One name as a lock-free read found it.
enum Read {
    Absent,
    /// Every part from one generation of the question, or a question that
    /// cannot be inspected, which reads as damage.
    Coherent(Box<Pair>),
    /// The question was replaced or removed during every try.
    Unsettled,
}

/// How many times a read that saw its question change is tried again.
const RETRIES: usize = 4;

/// `None` when the question changed while it was being read.
fn read_once(f: &Files, between: &mut dyn FnMut(Step)) -> Option<Read> {
    let before = match Generation::of(&f.question) {
        Ok(None) => return Some(Read::Absent),
        Err(e) => return Some(Read::Coherent(Box::new(Pair::uninspectable(f, &e)))),
        Ok(Some(g)) => g,
    };
    let (raw, question_error) = match std::fs::read_to_string(&f.question) {
        Ok(raw) => (raw, None),
        Err(e) => (
            String::new(),
            Some(format!("the question cannot be read: {e}")),
        ),
    };
    between(Step::QuestionRead);
    let q = unstamp(&raw);
    let occurrence = q.occurrence;
    let (answer, answer_damage, answered_at) = match read_answer(&f.answer) {
        Ok(None) => (None, None, None),
        Err(e) => (None, Some(format!("the answer cannot be read: {e}")), None),
        Ok(Some((raw, at))) => {
            let a = unstamp(&raw);
            if !belongs(occurrence, a.occurrence) {
                (None, None, None)
            } else if a.torn {
                (
                    None,
                    Some("the answer's header is cut short".to_string()),
                    None,
                )
            } else {
                (Some(a.body.to_string()), None, at)
            }
        }
    };
    between(Step::AnswerRead);
    let attempt = match read_attempt(&f.attempt) {
        None => AttemptState::Absent,
        Some(Stored::Unreadable(why)) => AttemptState::Unreadable(why),
        // An unread question names no occurrence to judge it against.
        Some(Stored::Valid(a)) if question_error.is_some() => AttemptState::Live(a),
        Some(Stored::Valid(a)) if Some(a.occurrence.as_str()) != occurrence || answer.is_some() => {
            AttemptState::Inert
        }
        Some(Stored::Valid(a)) => AttemptState::Live(a),
    };
    match Generation::of(&f.question) {
        Err(e) => return Some(Read::Coherent(Box::new(Pair::uninspectable(f, &e)))),
        Ok(after) if after.as_ref() != Some(&before) => return None,
        Ok(_) => {}
    }
    Some(Read::Coherent(Box::new(Pair {
        occurrence: occurrence.map(str::to_owned),
        question: q.body.to_string(),
        kind: match question_error {
            Some(why) => Stored::Unreadable(why),
            None => q.kind(),
        },
        answer,
        answer_damage,
        answered_at,
        attempt,
    })))
}

/// The answer's text and its modification time in unix seconds, both
/// from one open file, so a replacement at the path cannot date it.
/// `Ok(None)` only when there is no answer file.
fn read_answer(path: &Path) -> std::io::Result<Option<(String, Option<u64>)>> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // O_NONBLOCK so a FIFO planted at the path cannot hang the reader;
    // O_NOFOLLOW so a symlink is refused rather than read through.
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        file => file?,
    };
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other("the answer is not a regular file"));
    }
    let at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    Ok(Some((raw, at)))
}

/// `name`'s files, read without the blocks lock: an answer is paired with
/// a question only when both come from one generation of it.
fn read_pair(dir: &Path, name: &str, between: &mut dyn FnMut(Step)) -> Read {
    let f = files(dir, name);
    (0..=RETRIES)
        .find_map(|_| read_once(&f, between))
        .unwrap_or(Read::Unsettled)
}

impl Pair {
    /// Under the blocks lock, where no write can land mid-read; a read
    /// that still finds its question changing is an error.
    fn read(agent_dir: &Path, name: &str) -> anyhow::Result<Option<Pair>> {
        match read_pair(agent_dir, name, &mut |_| {}) {
            Read::Absent => Ok(None),
            Read::Coherent(pair) => Ok(Some(*pair)),
            Read::Unsettled => anyhow::bail!(
                "`{name}`'s question kept changing while it was read under the blocks lock"
            ),
        }
    }

    /// A question that exists but cannot be inspected: damage, never
    /// absence. Its attempt record is kept as live, as for an unread
    /// question.
    fn uninspectable(f: &Files, e: &std::io::Error) -> Pair {
        Pair {
            occurrence: None,
            question: String::new(),
            kind: Stored::Unreadable(format!("the question cannot be inspected: {e}")),
            answer: None,
            answer_damage: None,
            answered_at: None,
            attempt: match read_attempt(&f.attempt) {
                None => AttemptState::Absent,
                Some(Stored::Unreadable(why)) => AttemptState::Unreadable(why),
                Some(Stored::Valid(a)) => AttemptState::Live(a),
            },
        }
    }

    /// Every part of the pair that cannot be read. While there is any,
    /// the block is neither answered, retried, delivered nor deleted.
    fn damage(&self) -> Option<String> {
        let reasons: Vec<&str> = [
            self.kind.unreadable(),
            self.answer_damage.as_deref(),
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

    fn carries(&self, name: &str) -> Option<AnswerRef> {
        Some(AnswerRef {
            name: name.to_string(),
            occurrence: self.occurrence.clone(),
            question_digest: digest(&self.question),
            answer_digest: digest(self.answer.as_deref()?),
        })
    }
}

/// The names of the questions in `agent_dir`, sorted. No block directory
/// is no questions; any other failure to list it is an error.
fn question_names(agent_dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(agent_dir.join("blocks")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    names_of(entries.map(|e| e.map(|e| e.file_name())))
}

/// The question names among directory entries, whatever each entry's type:
/// a question path that is not a regular file is read as damage, not
/// skipped. An entry that failed fails the whole listing.
fn names_of(
    entries: impl Iterator<Item = std::io::Result<std::ffi::OsString>>,
) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in entries {
        if let Some(name) = entry?.to_str().and_then(|s| s.strip_suffix(".md")) {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Serializes every read-compare-write of repository blocks: ask, answer
/// and completion.
pub(crate) fn lock(repo: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(root(repo))?;
    crate::agent_store::exclusive_lock(&root(repo).join("blocks.lock"))
}

fn publish(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let _permit = crate::upgrade::writing()?;
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
    remove_under(path, &crate::upgrade::writing()?)
}

/// [`remove_if_present`] under a permit the caller holds: how a wake that
/// reached its agent deletes the block it answered.
fn remove_under(path: &Path, _permit: &crate::upgrade::Permit) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Deletes a block whose delivery the caller, holding the lock, has
/// matched. The question goes first: without it nothing is pending or
/// owed, so whatever an interruption leaves behind is inert, and the next
/// ask of the name removes it.
fn delete(f: &Files, permit: &crate::upgrade::Permit) -> std::io::Result<()> {
    for path in [&f.question, &f.answer, &f.creator, &f.attempt] {
        remove_under(path, permit)?;
    }
    Ok(())
}

/// Completes the delivery of the answer `carries` names, once a wake
/// carrying it was emitted: deletes its block if the pair on disk is still
/// exactly that answer to that ask, and otherwise deletes nothing. `true`
/// when it deleted.
pub(crate) fn complete(
    repo: &Path,
    carries: &AnswerRef,
    permit: &crate::upgrade::Permit,
) -> anyhow::Result<bool> {
    validate_name(&carries.name)?;
    let _lock = lock(repo)?;
    let dir = root(repo);
    let Some(pair) = Pair::read(&dir, &carries.name)? else {
        return Ok(false);
    };
    if pair.damage().is_some() || pair.carries(&carries.name).as_ref() != Some(carries) {
        return Ok(false);
    }
    delete(&files(&dir, &carries.name), permit)?;
    Ok(true)
}

#[cfg(test)]
pub(crate) fn test_master(repo: &Path, fallback: &clank_core::AgentLabel) {
    if master(repo).is_err() {
        crate::agent_store::repo_config::transact(repo, |cfg| {
            cfg.agents.insert(
                fallback.clone(),
                serde_json::from_value(serde_json::json!({"tool":"codex", "role":"master"}))
                    .unwrap(),
            );
            Ok(())
        })
        .unwrap();
    }
    let agent = master(repo).unwrap();
    if crate::agent_store::load_agent_config(repo, &agent)
        .unwrap()
        .and_then(|c| c.session)
        .is_none()
    {
        crate::agent_store::bind_session_to_agent(
            repo,
            &agent,
            clank_core::Tool::Codex,
            &clank_core::SessionId::parse(&format!("block-test-{agent}")).unwrap(),
        )
        .unwrap();
    }
}

/// `creator`'s question `name`, written as the repository stores it.
#[cfg(test)]
pub(crate) fn asked(repo: &Path, creator: &str, name: &str, question: &str) {
    let f = files(&root(repo), name);
    std::fs::create_dir_all(root(repo).join("blocks")).unwrap();
    std::fs::write(&f.question, question).unwrap();
    std::fs::write(&f.creator, creator).unwrap();
}

/// Ask `question` as a new plain occurrence of `name`.
#[cfg(test)]
pub(crate) fn ask(
    repo: &Path,
    author: &clank_core::AgentLabel,
    name: &str,
    question: &str,
) -> anyhow::Result<PathBuf> {
    test_master(repo, author);
    ask_with(repo, Some(author), name, question, &Kind::Plain)
}

/// Ask a new repository occurrence of `name`. Refused while a question of
/// that name exists, pending or answered and undelivered: replacing it
/// would discard an answer no one has seen.
pub(crate) fn ask_with(
    repo: &Path,
    author: Option<&clank_core::AgentLabel>,
    name: &str,
    question: &str,
    kind: &Kind,
) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    nonempty(question)?;
    let _lock = lock(repo)?;
    let f = files(&root(repo), name);
    if !absent(&f.question) {
        let state = match Pair::read(&root(repo), name)? {
            Some(p) if p.damage().is_some() => "its stored state cannot be read",
            Some(p) if p.answer.is_some() => "answered, and not yet delivered to the master",
            Some(p) if p.live_attempt().is_some() => "a nudge answering it is outstanding",
            Some(_) => "it is still waiting for an answer",
            None => "its question file cannot be read",
        };
        anyhow::bail!("block `{name}` already exists ({state}); use another name");
    }
    for leftover in [&f.answer, &f.attempt, &f.creator] {
        remove_if_present(leftover)?;
    }
    if let Some(a) = author {
        publish(&f.creator, a.as_str().as_bytes())?;
    }
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
    fn nudge(&mut self, text: &str, to: &clank_core::AgentLabel) -> Outcome;
}

#[cfg(test)]
impl<F: FnMut(&str) -> Outcome> Nudger for F {
    fn nudge(&mut self, text: &str, _to: &clank_core::AgentLabel) -> Outcome {
        self(text)
    }
}

/// How a block is answered.
pub(crate) enum How<'a> {
    /// Record the text as the answer, for the master's next wake to
    /// deliver. Refused for a permission block, and while a nudge is
    /// outstanding.
    Plain(&'a str),
    /// Type the text into the master's terminal; once it is submitted the
    /// block is delivered and deleted. `resend` replaces an outstanding
    /// attempt rather than being refused by it.
    Nudge {
        text: &'a str,
        resend: bool,
        via: &'a mut dyn Nudger,
    },
    /// The person says the master has the answer: an outstanding nudge's
    /// text arrived, or they typed it into the terminal themselves. The
    /// block is delivered and deleted, typing nothing.
    Delivered,
}

/// How a resolution ended, when nothing refused it before any typing.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// The answer is recorded there, for the master's next wake.
    Answered(PathBuf),
    /// The answer reached the master, and the block is deleted.
    Delivered,
    /// Nothing was typed; the block stays open.
    NotSent(String),
    /// The text may have arrived. The attempt stays outstanding with this
    /// diagnostic, and nothing resends it.
    Unknown(String),
    /// The text was delivered, but its claim no longer stood, so nothing
    /// was deleted.
    Superseded(String),
}

impl Resolved {
    /// The recorded answer's path, `None` once the block is delivered, or
    /// the outcome as an error naming what to run.
    pub(crate) fn settled(self, name: &str) -> anyhow::Result<Option<PathBuf>> {
        match self {
            Resolved::Answered(path) => Ok(Some(path)),
            Resolved::Delivered => Ok(None),
            Resolved::NotSent(why) => {
                anyhow::bail!("nothing was typed, so `{name}` stays open: {why}")
            }
            Resolved::Unknown(why) => anyhow::bail!(
                "nudge outcome unknown: {why}. `{name}` stays unresolved and nothing \
                 will be resent. If the text arrived: `clank block answer {name} \
                 --delivered`; if it did not: `clank block answer {name} --nudge \
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
    name: &str,
    message: &str,
    shown: Option<Shown<'_>>,
) -> anyhow::Result<PathBuf> {
    resolve(repo, name, shown, By::Unattested, How::Plain(message))?
        .settled(name)?
        .ok_or_else(|| anyhow::anyhow!("a plain answer to `{name}` is recorded, never delivered"))
}

/// The one way every answerer resolves a block. `shown` is captured by a
/// form, so an answer is recorded only for the ask the reader saw, never
/// for a re-ask that replaced it, even with the same text; without it the
/// current ask of `name` is answered. At most one answer is recorded per
/// ask, and at most one delivery is made per ask without `resend`.
pub(crate) fn resolve(
    repo: &Path,
    name: &str,
    shown: Option<Shown<'_>>,
    by: By,
    how: How<'_>,
) -> anyhow::Result<Resolved> {
    use anyhow::Context as _;
    validate_name(name)?;
    let dir = root(repo);
    let f = files(&dir, name);
    match how {
        How::Plain(text) => {
            let text = nonempty(text)?;
            let _lock = lock(repo)?;
            let open = open_pair(&dir, &f, name, shown)?;
            if let Kind::Permission { .. } = open.kind {
                anyhow::bail!(
                    "`{name}` is a permission block: only the person's own message in \
                     the agent's terminal lifts the refusal, so a person answers it with `clank \
                     block answer {name} --nudge --override-permission-classifier -m \
                     \"…\"`, or types it there and runs `clank block answer {name} \
                     --delivered --override-permission-classifier`"
                );
            }
            if let Some(a) = open.pair.live_attempt() {
                return Err(outstanding(name, a));
            }
            let occurrence = stamped(&f, &open)?;
            publish(&f.answer, stamp(&occurrence, None, text).as_bytes())
                .with_context(|| format!("recording answer for `{name}`"))?;
            Ok(Resolved::Answered(f.answer))
        }
        How::Delivered => {
            let _lock = lock(repo)?;
            let open = open_pair(&dir, &f, name, shown)?;
            attested(&open, by, name)?;
            delete(&f, &crate::upgrade::writing()?)
                .with_context(|| format!("deleting the delivered block `{name}`"))?;
            Ok(Resolved::Delivered)
        }
        How::Nudge { text, resend, via } => {
            let text = nonempty(text)?;
            let (claimed, to) = claim(repo, &dir, &f, name, shown, by, text, resend)?;
            let outcome = via.nudge(text, &to);
            let _ = crate::cli::nudge_log::append(
                &agents_root(repo).join(to.as_str()),
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
            settle(repo, &dir, &f, name, &claimed, outcome)
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

fn outstanding(name: &str, a: &Attempt) -> anyhow::Error {
    anyhow::anyhow!(
        "a nudge answering `{name}` is outstanding ({}; started {} ago, typing {:?}), and \
         nothing resends it automatically. If the text arrived: `clank block answer \
         {name} --delivered`. If it did not, or a duplicate is acceptable: `clank block answer \
         {name} --nudge --resend -m …`. A permission block also needs \
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
fn open_pair(dir: &Path, f: &Files, name: &str, shown: Option<Shown<'_>>) -> anyhow::Result<Open> {
    let Some(pair) = Pair::read(dir, name)? else {
        anyhow::bail!("the repository has no pending block named `{name}`");
    };
    if shown
        .is_some_and(|s| s.question != pair.question || s.occurrence != pair.occurrence.as_deref())
    {
        anyhow::bail!("the question for `{name}` changed; read it again before answering");
    }
    if let Some(why) = pair.damage() {
        anyhow::bail!(
            "`{name}` has stored state clank cannot read, so nothing answers or retries \
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
        anyhow::bail!("`{name}` was already answered; the master's next wake delivers the answer");
    }
    Ok(Open { pair, kind })
}

/// A permission block is resolved only by a person.
fn attested(open: &Open, by: By, name: &str) -> anyhow::Result<()> {
    if let Kind::Permission { .. } = open.kind
        && by != By::Person
    {
        anyhow::bail!(
            "`{name}` is a permission block: answering it lifts a refusal the agent's \
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

/// The published claim, and the master it is to be typed to.
#[allow(clippy::too_many_arguments)]
fn claim(
    repo: &Path,
    dir: &Path,
    f: &Files,
    name: &str,
    shown: Option<Shown<'_>>,
    by: By,
    text: &str,
    resend: bool,
) -> anyhow::Result<(Attempt, clank_core::AgentLabel)> {
    let _lock = lock(repo)?;
    let open = open_pair(dir, f, name, shown)?;
    attested(&open, by, name)?;
    if let Some(a) = open.pair.live_attempt()
        && !resend
    {
        return Err(outstanding(name, a));
    }
    let to = master(repo)?;
    let claimed = Attempt {
        occurrence: stamped(f, &open)?,
        attempt: mint_id()?,
        text: text.to_string(),
        started: crate::cli::recovery::unix_now(),
        diagnostic: None,
    };
    publish(&f.attempt, &serde_json::to_vec(&claimed)?)?;
    Ok((claimed, to))
}

/// Settle `claimed` by what its delivery could tell. Every write is fenced
/// to the claim: a record replaced by a resend or a re-ask's claim is not
/// this attempt's to settle, update or remove.
fn settle(
    repo: &Path,
    dir: &Path,
    f: &Files,
    name: &str,
    claimed: &Attempt,
    outcome: Outcome,
) -> anyhow::Result<Resolved> {
    use anyhow::Context as _;
    let _lock = lock(repo)?;
    let owned = claimed.owns(read_attempt(&f.attempt).as_ref());
    match outcome {
        Outcome::Submitted => {
            if !owned {
                return Ok(Resolved::Superseded(format!(
                    "the text was delivered to the master, but this attempt's claim on \
                     `{name}` was replaced (a --resend or --delivered) before it settled, \
                     so nothing was deleted for it"
                )));
            }
            let pair = Pair::read(dir, name)?
                .filter(|p| p.occurrence.as_deref() == Some(claimed.occurrence.as_str()));
            if let Some(why) = pair.as_ref().and_then(Pair::damage) {
                anyhow::bail!(
                    "the text was delivered to the master, but `{name}` has stored state \
                     clank cannot read, so the block was not deleted: {why}. Once it is \
                     repaired, run `clank block answer {name} --delivered \
                     --override-permission-classifier`"
                );
            }
            if pair.is_some_and(|p| p.live_attempt().is_some()) {
                delete(f, &crate::upgrade::writing()?).with_context(|| {
                    format!(
                        "the text was delivered to the master, but deleting `{name}` failed; \
                         run `clank block answer {name} --delivered`"
                    )
                })?;
                return Ok(Resolved::Delivered);
            }
            remove_if_present(&f.attempt)?;
            Ok(Resolved::Superseded(format!(
                "the text was delivered to the master for a question `{name}` that has \
                 since been asked again, answered or withdrawn; nothing was deleted, \
                 and any new ask stays open"
            )))
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

#[derive(Debug, Clone)]
pub struct BlockEntry {
    pub creator: String,
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
    /// answered, retried, delivered or deleted until a person repairs it.
    pub unreadable: Option<String>,
}

impl BlockEntry {
    pub(crate) fn pending(&self) -> bool {
        self.answer.is_none() || self.unreadable.is_some() || self.nudge.is_some()
    }
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

/// Every block not yet delivered, pending or answered, and every one
/// whose state cannot be read. It neither delivers answers nor deletes
/// questions.
pub fn scan_blocks(repo: &Path) -> Vec<BlockEntry> {
    Blocks::read(repo).into_entries()
}

/// One lock-free read of every repository block: what every serve and
/// admission reader consumes. Storage that cannot be read, a damaged
/// block and a block whose question changed throughout its read each
/// stand as a pending entry and owe nothing.
pub(crate) struct Blocks(Vec<Seen>);

struct Seen {
    entry: BlockEntry,
    /// Set only for a wholly readable answered pair.
    answered: Option<Answered>,
}

struct Answered {
    carries: AnswerRef,
    at: Option<u64>,
}

impl Blocks {
    pub(crate) fn read(repo: &Path) -> Blocks {
        Blocks::read_with(repo, &mut |_, _| {})
    }

    /// `between` runs at each [`Step`] of each name's read.
    fn read_with(repo: &Path, between: &mut dyn FnMut(&str, Step)) -> Blocks {
        let dir = root(repo);
        // Inspecting an ordinary Git repo must not initialize Clank storage.
        match std::fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Blocks(Vec::new()),
            Err(e) => return Blocks::storage_damage(&e),
            Ok(_) => {}
        }
        let names = match question_names(&dir) {
            Ok(names) => names,
            Err(e) => return Blocks::storage_damage(&e),
        };
        Blocks(
            names
                .into_iter()
                .filter_map(|name| {
                    let read = read_pair(&dir, &name, &mut |step| between(&name, step));
                    Seen::of(&dir, name, read)
                })
                .collect(),
        )
    }

    fn storage_damage(e: &std::io::Error) -> Blocks {
        Blocks(vec![Seen {
            entry: BlockEntry {
                creator: String::new(),
                name: "block-storage".into(),
                occurrence: None,
                kind: Stored::Unreadable(e.to_string()),
                question: format!("Block storage needs repair: {e}"),
                answer: None,
                nudge: None,
                unreadable: Some(format!("the block directory cannot be read: {e}")),
            },
            answered: None,
        }])
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &BlockEntry> {
        self.0.iter().map(|s| &s.entry)
    }

    pub(crate) fn into_entries(self) -> Vec<BlockEntry> {
        self.0.into_iter().map(|s| s.entry).collect()
    }

    /// The answers a wake owes `agent` while it is the master, by name.
    pub(crate) fn owed(&self, repo: &Path, agent: &clank_core::AgentLabel) -> Vec<OwedAnswer> {
        if master(repo).ok().as_ref() != Some(agent) {
            return vec![];
        }
        self.0
            .iter()
            .filter_map(|s| {
                let carries = s.answered.as_ref()?.carries.clone();
                let mut answer = s.entry.answer.clone()?;
                if s.entry.is_permission() {
                    answer = format!(
                        "Prior human decision (not permission approval for this session; \
                         obtain any approval your harness requires): {answer}"
                    );
                }
                Some(OwedAnswer { answer, carries })
            })
            .collect()
    }

    /// When each answered block was answered, by its [`key`]: the date its
    /// answer file carries, so it reads the same to a monitor that starts
    /// long after. A delivered block is deleted, and dates nothing.
    pub(crate) fn ended(&self) -> std::collections::BTreeMap<String, u64> {
        self.0
            .iter()
            .filter_map(|s| Some((key(&s.entry.name), s.answered.as_ref()?.at?)))
            .collect()
    }
}

impl Seen {
    fn of(dir: &Path, name: String, read: Read) -> Option<Seen> {
        let pair = match read {
            Read::Absent => return None,
            Read::Unsettled => {
                return Some(Seen {
                    entry: BlockEntry {
                        creator: String::new(),
                        question: format!(
                            "`{name}` is changing; it is read again on the next pass"
                        ),
                        unreadable: Some(format!(
                            "its question changed on each of {} reads",
                            RETRIES + 1
                        )),
                        name,
                        occurrence: None,
                        kind: Stored::Unreadable("its question is changing".into()),
                        answer: None,
                        nudge: None,
                    },
                    answered: None,
                });
            }
            Read::Coherent(pair) => *pair,
        };
        let unreadable = pair.damage();
        let answered = match unreadable {
            Some(_) => None,
            None => pair.carries(&name).map(|carries| Answered {
                carries,
                at: pair.answered_at,
            }),
        };
        Some(Seen {
            entry: BlockEntry {
                creator: std::fs::read_to_string(files(dir, &name).creator).unwrap_or_default(),
                name,
                occurrence: pair.occurrence,
                kind: pair.kind,
                question: pair.question,
                answer: pair.answer,
                nudge: match pair.attempt {
                    AttemptState::Live(a) => Some(Stored::Valid(a)),
                    AttemptState::Unreadable(why) => Some(Stored::Unreadable(why)),
                    _ => None,
                },
                unreadable,
            },
            answered,
        })
    }
}

pub(crate) fn key(name: &str) -> String {
    name.to_owned()
}

/// [`Blocks::ended`] of a fresh read.
pub(crate) fn ended(repo: &Path) -> std::collections::BTreeMap<String, u64> {
    Blocks::read(repo).ended()
}

/// An answer the master is owed: recorded, and not yet delivered.
pub(crate) struct OwedAnswer {
    pub(crate) answer: String,
    pub(crate) carries: AnswerRef,
}

/// [`Blocks::owed`] of a fresh read.
#[cfg(test)]
pub(crate) fn owed_answers(repo: &Path, agent: &clank_core::AgentLabel) -> Vec<OwedAnswer> {
    Blocks::read(repo).owed(repo, agent)
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
        asked(
            dir.path(),
            "alice",
            "fix-api",
            "What should the return type be?",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].creator, "alice");
        assert_eq!(blocks[0].name, "fix-api");
        assert_eq!(blocks[0].question, "What should the return type be?");
        assert!(blocks[0].answer.is_none());
    }

    #[test]
    fn scan_includes_answer_when_unblock_exists() {
        let dir = tempfile::tempdir().unwrap();
        asked(dir.path(), "alice", "fix-api", "What type?");
        write(
            &dir.path().join(".clank/unblocks/fix-api.md"),
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
        asked(repo, "alice", "why", "Which option?");
        assert!(answer_block(repo, "why", " ", None).is_err());
        assert!(answer_block(repo, "../why", "yes", None).is_err());
        assert!(answer_block(repo, "missing", "yes", None).is_err());
        assert!(
            answer_block(repo, "why", "yes", Some(legacy("Old question")))
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        assert!(scan_blocks(repo)[0].answer.is_none());
        answer_block(
            repo,
            "why",
            "  first answer  ",
            Some(legacy("Which option?")),
        )
        .unwrap();
        assert!(
            answer_block(repo, "why", "second answer", None)
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
            root.join(".clank/blocks/pause.md").exists(),
            "written FLAT, where the reader looks"
        );

        run_unblock(crate::cli::UnblockArgs {
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
            root.join(".clank/unblocks/pause.md").exists(),
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
            asked(root, agent, name, q);
        }

        run_unblock(crate::cli::UnblockArgs {
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
        assert_eq!(open[0].creator, "codex");
        assert!(
            !root.join(".clank/unblocks/second.md").exists(),
            "no canned answer written under the untouched question"
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
        test_master(repo, &alice());
        owed_answers(repo, &alice())
            .into_iter()
            .map(|o| o.carries)
            .collect()
    }

    fn asked_and_answered(repo: &Path, name: &str, q: &str, a: &str) -> AnswerRef {
        ask(repo, &alice(), name, q).unwrap();
        answer_block(repo, name, a, None).unwrap();
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

    /// A block ends when it is answered, as its answer file dates it, so
    /// it reads the same to a monitor that starts long after; a pending
    /// block has not ended, and a delivered one is deleted and dates
    /// nothing.
    #[test]
    fn a_blocks_end_is_dated_by_its_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        ask(repo, &alice(), "pending", "Still open?").unwrap();
        assert!(ended(repo).is_empty(), "a pending block has not ended");

        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        written_at(&repo.join(".clank/unblocks/q.md"), 7000);
        assert_eq!(ended(repo), [(key("q"), 7000)].into());

        assert!(complete(repo, &a, &crate::upgrade::process().settling()).unwrap());
        assert!(ended(repo).is_empty(), "delivered and deleted: undatable");
    }

    /// A process that has seen a newer clank installed takes no new
    /// question and no new answer, and still completes an answer its
    /// agent was already shown.
    #[test]
    fn a_stale_process_asks_and_answers_nothing_and_still_completes() {
        use crate::upgrade::tests::{AsProcess, Installed};
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let installed = Installed::current();
        let authority = installed.authority();
        let _as = AsProcess::of(&authority);
        let shown = asked_and_answered(repo, "q", "Proceed?", "yes");

        installed.replace(1);
        let refused = ask(repo, &alice(), "later", "And then?").unwrap_err();
        assert!(
            format!("{refused:#}").contains(crate::upgrade::STALE_BADGE),
            "{refused:#}"
        );
        assert!(answer_block(repo, "q", "no", None).is_err());
        assert!(!repo.join(".clank/blocks/later.md").exists());

        assert!(complete(repo, &shown, &authority.settling()).unwrap());
        assert!(!repo.join(".clank/blocks/q.md").exists());
    }

    /// Completion deletes the question, the answer, the creator and any
    /// attempt record, and leaves nothing to present, list or date.
    #[test]
    fn a_completed_answer_leaves_no_block_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        let f = files(&root(repo), "q");
        write(
            &f.attempt,
            &serde_json::to_string(&Attempt {
                occurrence: ID.into(),
                attempt: ID.into(),
                text: "an older ask's".into(),
                started: 0,
                diagnostic: None,
            })
            .unwrap(),
        );
        assert!(f.creator.exists() && f.answer.exists());
        assert!(complete(repo, &a, &crate::upgrade::process().settling()).unwrap());
        for path in [&f.question, &f.answer, &f.creator, &f.attempt] {
            assert!(!path.exists(), "{}", path.display());
        }
        assert!(owed(repo).is_empty());
        assert!(scan_blocks(repo).is_empty());
        assert!(
            !complete(repo, &a, &crate::upgrade::process().settling()).unwrap(),
            "a second completion finds nothing"
        );
    }

    /// The same question asked again and given the same answer is a new
    /// occurrence: a completion carried for the first, arriving after the
    /// name was delivered and asked again, deletes nothing, whether its ref
    /// is the one in memory or one read back after a restart.
    #[test]
    fn a_late_completion_after_identical_reuse_deletes_nothing() {
        for interleave in ["direct", "restart"] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let first = asked_and_answered(repo, "q", "Proceed?", "yes");
            assert!(complete(repo, &first, &crate::upgrade::process().settling()).unwrap());
            let second = asked_and_answered(repo, "q", "Proceed?", "yes");
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
            assert!(
                !complete(repo, &late, &crate::upgrade::process().settling()).unwrap(),
                "{interleave}"
            );
            assert!(repo.join(".clank/blocks/q.md").exists(), "{interleave}");
            assert_eq!(owed(repo), [second], "{interleave}: the new ask stays owed");
        }
    }

    /// A name is reused only once its block is delivered and deleted:
    /// re-asking it while its question is pending, or answered and not yet
    /// delivered, is refused, naming the block, and changes nothing.
    #[test]
    fn re_asking_a_name_is_refused_until_its_block_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        let pending = std::fs::read(question_path(repo, "q")).unwrap();
        let err = ask(repo, &alice(), "q", "Proceed?")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`q`") && err.contains("waiting"), "{err}");
        assert_eq!(std::fs::read(question_path(repo, "q")).unwrap(), pending);

        answer_block(repo, "q", "yes", None).unwrap();
        let a = owed(repo).remove(0);
        let err = ask(repo, &alice(), "q", "Proceed?")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`q`") && err.contains("not yet delivered"),
            "{err}"
        );
        assert_eq!(
            owed(repo),
            std::slice::from_ref(&a),
            "the undelivered answer is kept"
        );

        complete(repo, &a, &crate::upgrade::process().settling()).unwrap();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        assert_eq!(the_block(repo).answer, None, "owed to the human again");
        assert!(owed(repo).is_empty(), "no old answer is rendered for it");
    }

    /// A completion interrupted once the question is gone leaves its
    /// answer, creator and attempt behind. Nothing is pending or owed, and
    /// a new ask of the name removes them before it writes, so the old
    /// answer never attaches to it; an old answer written back beside the
    /// new ask names a dead occurrence and is ignored all the same.
    #[test]
    fn an_interrupted_completion_leaves_nothing_pending_or_attachable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        asked_and_answered(repo, "q", "Proceed?", "yes");
        let f = files(&root(repo), "q");
        let answer = std::fs::read(&f.answer).unwrap();
        write(&f.attempt, "{\"occurrence\":");
        std::fs::remove_file(&f.question).unwrap();
        assert!(scan_blocks(repo).is_empty());
        assert!(owed(repo).is_empty());
        assert!(ended(repo).is_empty());

        ask(repo, &alice(), "q", "Proceed?").unwrap();
        assert!(!f.answer.exists() && !f.attempt.exists());
        assert_eq!(the_block(repo).answer, None);
        assert!(owed(repo).is_empty());

        write(&f.answer, std::str::from_utf8(&answer).unwrap());
        assert_eq!(the_block(repo).answer, None);
        assert!(owed(repo).is_empty());
        answer_block(repo, "q", "no", None).unwrap();
        assert_eq!(the_block(repo).answer.as_deref(), Some("no"));
    }

    /// A pair asked and answered before occurrences existed is owed, and
    /// reading it changes nothing; it completes by its name and digests.
    #[test]
    fn a_legacy_pair_is_owed_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let q = repo.join(".clank/blocks/q.md");
        let a = repo.join(".clank/unblocks/q.md");
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

        assert!(complete(repo, &carries[0], &crate::upgrade::process().settling()).unwrap());
        assert!(!q.exists() && !a.exists());
    }

    /// An answer an older clank recorded as nudged carries a disposition
    /// in its header. It is an answer like any other: owed once, and
    /// deleted when delivered.
    #[test]
    fn an_old_nudged_answer_is_owed_like_any_answer() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(
            &question_path(repo, "q"),
            &format!("occurrence: {ID}\nWhich?"),
        );
        write(
            &answer_path(repo, "q"),
            &format!("occurrence: {ID} {{\"disposition\":\"nudged\",\"attempt\":\"{ID}\"}}\nB"),
        );
        assert_eq!(the_block(repo).answer.as_deref(), Some("B"));
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert!(complete(repo, &carries[0], &crate::upgrade::process().settling()).unwrap());
        assert!(scan_blocks(repo).is_empty());
    }

    /// A crash between stamping the answer and the question leaves the
    /// pair answered, and owed.
    #[test]
    fn a_half_stamped_legacy_pair_stays_answered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(".clank/blocks/q.md"), "Proceed?");
        write(
            &repo.join(".clank/unblocks/q.md"),
            &stamp("0123456789abcdef0123456789abcdef", None, "yes"),
        );
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("yes"));
        assert!(complete(repo, &carries[0], &crate::upgrade::process().settling()).unwrap());
        assert!(owed(repo).is_empty());
    }

    #[test]
    fn answering_a_legacy_question_stamps_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let q = repo.join(".clank/blocks/q.md");
        write(&q, "Proceed?");
        answer_block(repo, "q", "yes", Some(legacy("Proceed?"))).unwrap();
        let raw = std::fs::read_to_string(&q).unwrap();
        assert!(unstamp(&raw).occurrence.is_some(), "{raw}");
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(carries[0].occurrence.as_deref(), unstamp(&raw).occurrence);
    }

    fn permission(repo: &Path, name: &str, suggestion: Option<&str>) {
        test_master(repo, &alice());
        let f = files(&root(repo), name);
        if f.question.exists() {
            // Inject a replacement occurrence to test late claims, which must
            // remain safe even when storage was replaced outside the CLI.
            publish(
                &f.question,
                stamp_question(
                    &mint_id().unwrap(),
                    &Kind::Permission {
                        suggestion: suggestion.map(str::to_owned),
                    },
                    "git push fork was refused; I need it to publish the repro",
                )
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
            remove_if_present(&f.answer).unwrap();
            return;
        }
        ask_with(
            repo,
            Some(&alice()),
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
        resolve(repo, name, None, By::Person, How::Delivered)
    }

    fn attempt_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/blocks/{name}.attempt"))
    }

    fn answer_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/unblocks/{name}.md"))
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

    /// The answer as written: its occurrence and its text.
    fn written(repo: &Path, name: &str) -> (Option<String>, String) {
        let raw = std::fs::read_to_string(answer_path(repo, name)).unwrap();
        let s = unstamp(&raw);
        (s.occurrence.map(str::to_owned), s.body.to_string())
    }

    /// No file of block `name` is left.
    fn gone(repo: &Path, name: &str) -> bool {
        let f = files(&root(repo), name);
        [&f.question, &f.answer, &f.creator, &f.attempt]
            .iter()
            .all(|p| !p.exists())
    }

    #[test]
    fn old_question_and_answer_bytes_parse_as_they_always_did() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let id = "0123456789abcdef0123456789abcdef";
        write(
            &repo.join(".clank/blocks/q.md"),
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
        let raw = std::fs::read_to_string(repo.join(".clank/blocks/fresh.md")).unwrap();
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
        assert!(parse(&["block", "answer", "q", "--nudge", "-m", "yes"]).is_ok());
        assert!(parse(&["block", "answer", "q", "--nudge", "--resend", "-m", "yes"]).is_ok());
        assert!(parse(&["block", "answer", "q", "--resend", "-m", "yes"]).is_err());
        assert!(parse(&["block", "answer", "q", "--delivered"]).is_ok());
        assert!(parse(&["block", "answer", "q", "--delivered", "-m", "x"]).is_err());
        assert!(
            parse(&["block", "answer", "q"]).is_err(),
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

    /// Test 3: a submitted nudge is a delivery, so it deletes the block;
    /// nothing is left to present as owed.
    #[test]
    fn a_submitted_nudge_delivers_and_deletes_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", Some("yes: push it"));
        let mut typed = Vec::new();
        let mut claim_seen = None;
        let resolved = nudge(repo, "push", "  yes: push it  ", &mut |text: &str| {
            typed.push(text.to_string());
            claim_seen = record(repo, "push");
            submitted()
        })
        .unwrap();
        assert!(matches!(resolved, Resolved::Delivered), "{resolved:?}");
        assert_eq!(typed, ["yes: push it"]);
        let claim = claim_seen.expect("the claim is published before anything is typed");
        assert!(gone(repo, "push"));
        assert!(owed(repo).is_empty());
        assert!(scan_blocks(repo).is_empty());
        let history = crate::cli::nudge_log::read(&repo.join(".clank/agents/alice"));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].attempt, claim.attempt);
        assert_eq!(
            history[0].trigger,
            crate::cli::nudge_log::Trigger::Block {
                name: "push".into()
            }
        );
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
            plain = Some(resolve(repo, "push", None, By::Person, How::Plain("plain")));
            submitted()
        })
        .unwrap();
        let err = second.unwrap().unwrap_err().to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(plain.unwrap().is_err());
        assert!(gone(repo, "push"));

        for kind in [Kind::Plain, Kind::Permission { suggestion: None }] {
            ask_with(repo, Some(&alice()), "race", "Proceed?", &kind).unwrap();
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

    /// Test 7: an unconfirmed nudge deletes nothing, stays visibly
    /// unresolved and is never typed again on its own; the person's later
    /// `--delivered` completes it.
    #[test]
    fn an_unconfirmed_nudge_stays_unresolved_until_delivered_completes_it() {
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
        assert!(owed(repo).is_empty());
        let err = nudge(repo, "push", "yes", &mut never)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--delivered") && err.contains("--resend"),
            "{err}"
        );
        assert!(!answer_path(repo, "push").exists());

        assert!(matches!(
            delivered(repo, "push").unwrap(),
            Resolved::Delivered
        ));
        assert!(gone(repo, "push"));
        assert_eq!(
            crate::cli::nudge_log::read(&repo.join(".clank/agents/alice")).len(),
            1,
            "nothing was typed again"
        );
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
        let mut fresh = None;
        let resolved = resend(repo, "push", "yes, again", &mut |t: &str| {
            typed.push(t.to_string());
            fresh = record(repo, "push");
            submitted()
        })
        .unwrap();
        assert!(matches!(resolved, Resolved::Delivered), "{resolved:?}");
        assert_eq!(typed, ["yes, again"]);
        assert_ne!(fresh.unwrap().attempt, first.attempt, "a fresh claim");
        assert!(gone(repo, "push"));
        assert!(
            delivered(repo, "push").is_err(),
            "nothing is left to settle"
        );
    }

    /// Test 10.
    #[test]
    fn a_late_delivery_after_a_resend_deletes_nothing() {
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
                    std::fs::read(question_path(repo, "push")).ok(),
                ));
                submitted()
            })
            .unwrap();
            assert!(matches!(late, Resolved::Superseded(_)), "{late:?}");
            let (rec, question) = after_resend.unwrap();
            assert_eq!(
                record(repo, "push"),
                rec,
                "{resend_outcome:?}: the resend's record"
            );
            assert_eq!(
                std::fs::read(question_path(repo, "push")).ok(),
                question,
                "{resend_outcome:?}: the resend's question"
            );
            if resend_outcome == Outcome::Submitted {
                assert!(gone(repo, "push"));
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
            assert!(matches!(next, Ok(Resolved::Delivered)), "{case}: {next:?}");
            assert_eq!(fresh, 1, "{case}");
        }
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
        assert!(gone(repo, "q"), "the agent saw it typed");
        assert!(owed(repo).is_empty());

        ask(repo, &alice(), "q", "Which?").unwrap();
        nudge(repo, "q", "B", &mut |_: &str| unknown()).unwrap();
        let err = answer_block(repo, "q", "C", None).unwrap_err().to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(matches!(delivered(repo, "q").unwrap(), Resolved::Delivered));

        ask(repo, &alice(), "q", "Which?").unwrap();
        answer_block(repo, "q", "C", None).unwrap();
        assert_eq!(written(repo, "q"), (the_block(repo).occurrence, "C".into()));
        assert_eq!(owed(repo).len(), 1, "a plain answer is owed as before");
    }

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn question_path(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/blocks/{name}.md"))
    }

    /// Every How, as a person: each is refused before anything is typed.
    fn every_answer_refused(repo: &Path, name: &str) {
        let tries: [(&str, anyhow::Result<Resolved>); 4] = [
            (
                "plain",
                resolve(repo, name, None, By::Person, How::Plain("yes")),
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
        assert!(ask(repo, &alice(), "push", "again?").is_err());
        assert!(record(repo, "push").is_some(), "never removed");
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
    fn a_completion_never_deletes_unreadable_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let carries = asked_and_answered(repo, "q", "Which?", "B");
        write(&attempt_path(repo, "q"), "torn");
        assert!(the_block(repo).unreadable.is_some());
        assert!(owed(repo).is_empty());
        assert!(!complete(repo, &carries, &crate::upgrade::process().settling()).unwrap());
        assert!(question_path(repo, "q").exists() && answer_path(repo, "q").exists());

        std::fs::remove_file(attempt_path(repo, "q")).unwrap();
        write(
            &answer_path(repo, "q"),
            &format!("occurrence: {}", carries.occurrence.clone().unwrap()),
        );
        assert!(the_block(repo).unreadable.is_some(), "a cut-short answer");
        assert!(!complete(repo, &carries, &crate::upgrade::process().settling()).unwrap());
        assert!(question_path(repo, "q").exists());
    }

    /// Answering a permission block needs a person's override; an
    /// ordinary block does not.
    #[test]
    fn a_permission_block_is_resolved_only_by_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        let unattested = |how: How<'_>| resolve(repo, "push", None, By::Unattested, how);
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
            Resolved::Delivered
        ));

        ask(repo, &alice(), "q", "Which?").unwrap();
        let mut typed = 0;
        let ordinary = resolve(
            repo,
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
        let settled = resolve(repo, "q", None, By::Unattested, How::Delivered).unwrap();
        assert!(matches!(settled, Resolved::Delivered));
    }

    /// A permission answer the person types into the terminal themselves
    /// is invisible to clank: nothing completes the block until the person
    /// says so with `--delivered`, which needs the override.
    #[tokio::test]
    async fn a_permission_block_the_person_answered_completes_only_on_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        permission(repo, "push", None);
        assert!(owed(repo).is_empty());
        assert!(answer_block(repo, "push", "yes", None).is_err());
        assert!(the_block(repo).pending(), "never completed on its own");
        let settle = |flag: bool| {
            run_unblock(crate::cli::UnblockArgs {
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
        assert!(the_block(repo).pending());
        settle(true).await.unwrap();
        assert!(gone(repo, "push"));
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
        assert!(gone(repo, "push"));

        ask(repo, &alice(), "q", "Which?").unwrap();
        nudge(repo, "q", "B", &mut |_: &str| unknown()).unwrap();
        run_unblock(crate::cli::UnblockArgs {
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

    fn settling() -> crate::upgrade::Permit {
        crate::upgrade::process().settling()
    }

    /// Republishes `path` with the bytes it has: a new generation of it.
    fn republished(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        publish(path, &bytes).unwrap();
    }

    /// Runs `land` once, at the first `at` step of a read.
    fn once_at(at: Step, mut land: impl FnMut()) -> impl FnMut(&str, Step) {
        let mut landed = false;
        move |_, step| {
            if step == at && !landed {
                landed = true;
                land();
            }
        }
    }

    /// A completion and a re-ask landing after the question is read and
    /// before its answer is: the read is taken again, and the old question
    /// is never presented with the new ask's answer, whether the old
    /// question was stamped or written before occurrences.
    #[test]
    fn a_reuse_mid_read_never_pairs_across_asks() {
        for stamped in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let first = if stamped {
                asked_and_answered(repo, "q", "Proceed?", "yes")
            } else {
                write(&question_path(repo, "q"), "Proceed?");
                write(&answer_path(repo, "q"), "yes");
                owed(repo).remove(0)
            };
            let blocks = Blocks::read_with(
                repo,
                &mut once_at(Step::QuestionRead, || {
                    assert!(complete(repo, &first, &settling()).unwrap());
                    ask(repo, &alice(), "q", "Deploy?").unwrap();
                    answer_block(repo, "q", "no", None).unwrap();
                }),
            );
            let read: Vec<_> = blocks
                .entries()
                .map(|b| (b.question.as_str(), b.answer.as_deref()))
                .collect();
            assert_eq!(read, [("Deploy?", Some("no"))], "stamped: {stamped}");
            let owed = blocks.owed(repo, &alice());
            assert_eq!(owed.len(), 1, "stamped: {stamped}");
            assert_eq!(
                owed[0].carries.question_digest,
                digest("Deploy?"),
                "stamped: {stamped}"
            );
        }
    }

    /// A question replaced during every try is `Unsettled`: pending,
    /// reported as changing, presenting no answer, owing and dating
    /// nothing.
    #[test]
    fn a_question_changing_on_every_try_is_unsettled_and_owes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        asked_and_answered(repo, "q", "Proceed?", "yes");
        written_at(&answer_path(repo, "q"), 7000);
        let mut tries = 0;
        let blocks = Blocks::read_with(repo, &mut |_, step| {
            if step == Step::QuestionRead {
                tries += 1;
                republished(&question_path(repo, "q"));
            }
        });
        assert_eq!(tries, RETRIES + 1);
        let entries: Vec<_> = blocks.entries().collect();
        assert_eq!(entries.len(), 1);
        let b = entries[0];
        assert!(b.pending() && b.answer.is_none(), "{b:?}");
        assert!(
            b.unreadable
                .as_deref()
                .is_some_and(|why| why.contains("changed")),
            "{b:?}"
        );
        assert!(blocks.owed(repo, &alice()).is_empty());
        assert!(blocks.ended().is_empty());
    }

    /// A completion landing after the answer is read, before the read
    /// ends, leaves nothing owed, pending or dated.
    #[test]
    fn a_completion_between_reads_owes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        let blocks = Blocks::read_with(
            repo,
            &mut once_at(Step::AnswerRead, || {
                assert!(complete(repo, &a, &settling()).unwrap());
            }),
        );
        assert!(blocks.owed(repo, &alice()).is_empty());
        assert_eq!(blocks.entries().count(), 0);
        assert!(blocks.ended().is_empty());
    }

    /// An answer is dated only from the read that paired it: by the
    /// re-ask's answer once a re-ask lands mid-read, and by the file it was
    /// read from when another is put at its path after it was opened.
    #[test]
    fn an_answer_is_dated_only_by_its_own_read() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let first = asked_and_answered(repo, "q", "Proceed?", "yes");
        written_at(&answer_path(repo, "q"), 7000);
        let blocks = Blocks::read_with(
            repo,
            &mut once_at(Step::AnswerRead, || {
                assert!(complete(repo, &first, &settling()).unwrap());
                ask(repo, &alice(), "q", "Deploy?").unwrap();
                answer_block(repo, "q", "no", None).unwrap();
                written_at(&answer_path(repo, "q"), 9000);
            }),
        );
        assert_eq!(blocks.ended(), [(key("q"), 9000)].into());

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        asked_and_answered(repo, "q", "Proceed?", "yes");
        written_at(&answer_path(repo, "q"), 7000);
        let blocks = Blocks::read_with(
            repo,
            &mut once_at(Step::AnswerRead, || {
                republished(&answer_path(repo, "q"));
                written_at(&answer_path(repo, "q"), 9000);
            }),
        );
        assert_eq!(blocks.ended(), [(key("q"), 7000)].into());
    }

    /// Restores a directory's mode when dropped, so the tempdir can be
    /// removed.
    struct Mode<'a>(&'a Path, u32);

    impl<'a> Mode<'a> {
        fn set(path: &'a Path, mode: u32) -> Mode<'a> {
            use std::os::unix::fs::PermissionsExt as _;
            let was = std::fs::metadata(path).unwrap().permissions().mode();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            Mode(path, was)
        }
    }

    impl Drop for Mode<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(self.1));
        }
    }

    /// Every serve and admission reader holds work back for `repo`'s
    /// damage, entered under `name`, and owes nothing, though an answer
    /// sits behind it.
    fn every_reader_blocks(repo: &Path, name: &str) {
        use clank_core::wait::PlanStateLookup as _;
        let blocks = scan_blocks(repo);
        assert!(
            matches!(blocks.as_slice(), [b]
                if b.name == name && b.pending() && b.unreadable.is_some() && b.answer.is_none()),
            "{name}: {blocks:?}"
        );
        assert!(owed_answers(repo, &alice()).is_empty(), "{name}");
        assert!(ended(repo).is_empty(), "{name}");
        assert!(crate::cli::wait::blocked(repo, &alice()), "{name}");
        assert!(
            matches!(
                crate::cli::wait::block_items(repo, &alice()).as_slice(),
                [clank_core::wait::WaitItem::Blocked { .. }, ..]
            ),
            "{name}"
        );
        let plan = clank_core::ids::PlanKey::parse("p").unwrap();
        assert!(
            !crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, None)
                .blocks_for(&plan)
                .is_empty(),
            "{name}"
        );
    }

    /// A block directory that cannot be listed, a question that cannot be
    /// inspected, and a `.clank` that cannot be inspected are each damage
    /// every reader holds work back for, never an empty set.
    #[test]
    fn storage_that_cannot_be_read_blocks_every_reader() {
        for (mode, name) in [(0o000, "block-storage"), (0o400, "q")] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            asked_and_answered(repo, "q", "Proceed?", "yes");
            let blocks_dir = root(repo).join("blocks");
            let _mode = Mode::set(&blocks_dir, mode);
            every_reader_blocks(repo, name);
        }

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        asked_and_answered(&repo, "q", "Proceed?", "yes");
        let _mode = Mode::set(&repo, 0o600);
        let blocks = scan_blocks(&repo);
        assert!(
            matches!(blocks.as_slice(), [b] if b.pending() && b.unreadable.is_some()),
            "{blocks:?}"
        );
        assert!(crate::cli::wait::blocked(&repo, &alice()));
    }

    #[test]
    fn a_failed_directory_entry_fails_the_listing() {
        let entries = [
            Ok("a.md".into()),
            Err(std::io::Error::other("entry")),
            Ok("b.md".into()),
        ];
        assert!(names_of(entries.into_iter()).is_err());
        let entries = [
            Ok("b.md".into()),
            Ok("a.md".into()),
            Ok("dir.md".into()),
            Ok("a.attempt".into()),
        ];
        assert_eq!(
            names_of(entries.into_iter()).unwrap(),
            ["a", "b", "dir"],
            "a question name is listed whatever its entry's type"
        );
    }

    /// A question path that exists but is no regular file still holds its
    /// name: it is blocking damage to every reader, never followed or read,
    /// owes nothing, and an ask of the name is refused, so the state is
    /// consistent. Found at discovery, or swapped in between read steps.
    #[test]
    fn a_question_that_is_not_a_regular_file_blocks_as_damage() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = repo_dir.path().to_path_buf();
        test_master(&repo, &alice());
        let blocks = root(&repo).join("blocks");
        std::fs::create_dir_all(&blocks).unwrap();
        let elsewhere = repo.join("elsewhere.md");
        std::fs::write(&elsewhere, "Not a question").unwrap();
        use clank_core::wait::PlanStateLookup as _;
        let assert_blocking = |what: &str| {
            let found = scan_blocks(&repo);
            assert!(
                matches!(found.as_slice(), [b] if b.pending() && b.unreadable.is_some()),
                "{what}: {found:?}"
            );
            assert!(crate::cli::wait::blocked(&repo, &alice()), "{what}");
            assert!(
                matches!(
                    crate::cli::wait::block_items(&repo, &alice()).as_slice(),
                    [clank_core::wait::WaitItem::Blocked { .. }, ..]
                ),
                "{what}"
            );
            let plan = clank_core::ids::PlanKey::parse("p").unwrap();
            assert!(
                !crate::fs_plan_state_lookup::FsPlanStateLookup::new(&repo, None)
                    .blocks_for(&plan)
                    .is_empty(),
                "{what}"
            );
            assert!(
                Blocks::read(&repo).owed(&repo, &alice()).is_empty(),
                "{what}"
            );
            assert!(Blocks::read(&repo).ended().is_empty(), "{what}");
        };
        let q = files(&root(&repo), "q").question;
        std::fs::create_dir(&q).unwrap();
        std::fs::create_dir_all(root(&repo).join("unblocks")).unwrap();
        std::fs::write(files(&root(&repo), "q").answer, "yes").unwrap();
        assert_blocking("a directory");
        std::fs::remove_dir(&q).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &q).unwrap();
        assert_blocking("a symlink");
        assert!(
            ask_with(&repo, Some(&alice()), "q", "Again?", &Kind::Plain).is_err(),
            "the name is held, so an ask is refused"
        );
        std::fs::remove_file(&q).unwrap();

        asked(&repo, "alice", "q", "Which?");
        let swapped = Blocks::read_with(&repo, &mut |_, step| {
            if step == Step::QuestionRead && std::fs::symlink_metadata(&q).unwrap().is_file() {
                std::fs::remove_file(&q).unwrap();
                std::fs::create_dir(&q).unwrap();
            }
        });
        let seen: Vec<_> = swapped.entries().collect();
        assert!(
            matches!(seen.as_slice(), [b] if b.pending() && b.unreadable.is_some()),
            "{seen:?}"
        );
        assert!(swapped.owed(&repo, &alice()).is_empty());
        assert_blocking("swapped mid-read");
    }

    /// Only a missing file is absence: no `.clank`, no block directory, and
    /// a question deleted under its answer each read as no block, and
    /// reading creates nothing.
    #[test]
    fn only_not_found_is_absence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert!(scan_blocks(repo).is_empty());
        assert!(!root(repo).exists(), "reading created no storage");
        std::fs::create_dir_all(root(repo)).unwrap();
        assert!(scan_blocks(repo).is_empty());
        asked_and_answered(repo, "q", "Proceed?", "yes");
        std::fs::remove_file(question_path(repo, "q")).unwrap();
        assert!(scan_blocks(repo).is_empty());
        assert!(owed(repo).is_empty() && ended(repo).is_empty());
        assert!(!crate::cli::wait::blocked(repo, &alice()));
    }

    #[test]
    fn scan_returns_sorted_by_repository_name() {
        let dir = tempfile::tempdir().unwrap();
        asked(dir.path(), "bob", "z", "q");
        write(&dir.path().join(".clank/blocks/b.md"), "q");
        write(&dir.path().join(".clank/blocks/a.md"), "q");
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].name, "a");
        assert_eq!(blocks[1].name, "b");
        assert_eq!(blocks[2].creator, "bob");
    }

    #[test]
    fn scan_empty_repo_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let blocks = scan_blocks(dir.path());
        assert!(blocks.is_empty());
    }
}

/// Delivers `name`'s current answer, as a wake carrying it would.
#[cfg(test)]
pub(crate) fn test_complete(repo: &Path, name: &str) {
    let carries = Pair::read(&root(repo), name)
        .unwrap()
        .unwrap()
        .carries(name)
        .unwrap();
    assert!(complete(repo, &carries, &crate::upgrade::process().settling()).unwrap());
}

#[cfg(test)]
mod repo_tests;
