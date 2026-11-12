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
        BlockCmd::List(a) => list(a.repo.as_deref()),
    }
}

mod migration;

fn root(repo: &Path) -> PathBuf {
    repo.join(".clank")
}

fn hold_roster(repo: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(root(repo))?;
    crate::agent_store::exclusive_lock(&root(repo).join("config.lock"))
}

async fn run_migrate(args: BlockMigrateArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let _lock = raw_lock(&repo)?;
    migration::run(&repo, args.dry)
}

/// No identity is needed to inspect repository decisions, including received
/// answers kept for a successor until explicit cleanup.
fn list(repo: Option<&Path>) -> anyhow::Result<()> {
    let repo = resolve_repo(repo)?;
    let _lock = lock(&repo)?;
    for name in question_names(&root(&repo)) {
        if let Some(pair) = Pair::read(&root(&repo), &name) {
            println!("{name}: {}", pair.question);
            if let Some(answer) = &pair.answer {
                println!("  answer: {answer}");
            }
            if let Some(why) = pair.damage() {
                println!("  unresolved: {why}");
            }
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

/// Delivery evidence, not block ownership. A new attempt must capture the
/// current master's bound session; old evidence never confirms a new session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Destination {
    pub agent: clank_core::AgentLabel,
    pub tool: clank_core::Tool,
    pub session: clank_core::SessionId,
}

fn destination(repo: &Path) -> anyhow::Result<Destination> {
    let agent = master(repo)?;
    let cfg = crate::agent_store::load_agent_config(repo, &agent)?
        .ok_or_else(|| anyhow::anyhow!("the master has no bound session"))?;
    let session = cfg
        .session
        .ok_or_else(|| anyhow::anyhow!("the master has no bound session"))?;
    Ok(Destination {
        agent,
        tool: session.tool,
        session: session.id,
    })
}

/// The user's `blocked` hook may page someone and wait on the answer.
const BLOCK_HOOK_BUDGET: std::time::Duration = std::time::Duration::from_secs(10 * 60);

async fn run_create(args: BlockCreateArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let author = crate::agent_env::resolve_identity_from_env(&repo).ok();
    eprintln!("BLOCK: this pauses the repository review cycle until the question is answered");
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
    let path = ask_with(&repo, author.as_ref(), &args.name, &args.message, &kind)?;

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
    let path = resolved.into_answer(&args.name)?;
    let verb = if args.nudge || args.delivered {
        "unblocked by nudge"
    } else {
        "unblocked"
    };
    println!(
        "{verb} repository as {}",
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

fn stamp_nudged(
    occurrence: &str,
    attempt: &str,
    body: &str,
    destination: Option<Destination>,
) -> anyhow::Result<String> {
    let meta = serde_json::to_string(&Disposition::Nudged {
        attempt: attempt.to_string(),
        destination,
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
    Nudged {
        attempt: String,
        #[serde(default)]
        destination: Option<Destination>,
    },
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
    creator: PathBuf,
}

fn files(agent_dir: &Path, name: &str) -> Files {
    Files {
        question: agent_dir.join("blocks").join(format!("{name}.md")),
        answer: agent_dir.join("unblocks").join(format!("{name}.md")),
        marker: agent_dir.join("unblocks").join(format!("{name}.received")),
        attempt: agent_dir.join("blocks").join(format!("{name}.attempt")),
        creator: agent_dir.join("blocks").join(format!("{name}.creator")),
    }
}

/// The durable receipt: present and matching the current pair, it is
/// what retires that pair.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Marker {
    occurrence: String,
    question_digest: String,
    answer_digest: String,
    #[serde(default)]
    destination: Option<Destination>,
}

/// Missing is the only state with no receipt evidence. A file that cannot be
/// read or parsed may record a completed delivery, so it needs repair first.
fn read_marker(path: &Path) -> Option<Stored<Marker>> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && absent(path) => None,
        Err(e) => Some(Stored::Unreadable(format!(
            "its receipt `{}` cannot be read: {e}",
            path.display()
        ))),
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_or_else(
            |e| {
                Stored::Unreadable(format!(
                    "its receipt `{}` is malformed: {e}",
                    path.display()
                ))
            },
            Stored::Valid,
        )),
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
    #[serde(default)]
    pub(crate) destination: Option<Destination>,
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
    marker: Option<Stored<Marker>>,
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
            Stored::Valid(Some(Disposition::Nudged { attempt, .. })) => Some(attempt.as_str()),
            _ => None,
        };
        let marker = read_marker(&f.marker);
        let received = nudged_by.is_some()
            || match (occurrence, &answer) {
                (Some(occurrence), Some(answer)) => {
                    marker.as_ref().and_then(Stored::valid).is_some_and(|m| {
                        m.occurrence == occurrence
                            && m.question_digest == digest(q.body)
                            && m.answer_digest == digest(answer)
                    })
                }
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
            marker,
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
            self.marker.as_ref().and_then(Stored::unreadable),
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

/// Serializes every read-compare-write of repository blocks: ask, answer,
/// clean and receive.
fn raw_lock(repo: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(root(repo))?;
    crate::agent_store::exclusive_lock(&root(repo).join("blocks.lock"))
}

pub(crate) fn lock(repo: &Path) -> anyhow::Result<std::fs::File> {
    let guard = raw_lock(repo)?;
    migration::run(repo, false)?;
    Ok(guard)
}

fn publish(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    publish_under(path, bytes, &crate::upgrade::writing()?)
}

/// [`publish`] under a permit the caller holds: how an answer that
/// reached its agent is recorded as received.
fn publish_under(
    path: &Path,
    bytes: &[u8],
    _permit: &crate::upgrade::Permit,
) -> anyhow::Result<()> {
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
    let _permit = crate::upgrade::writing()?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
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

/// Ask a new repository occurrence. Reuse is allowed only after the current
/// master received the earlier answer and no attempt remains unsettled.
pub(crate) fn ask_with(
    repo: &Path,
    author: Option<&clank_core::AgentLabel>,
    name: &str,
    question: &str,
    kind: &Kind,
) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    nonempty(question)?;
    let _roster = hold_roster(repo)?;
    let _lock = lock(repo)?;
    let current = destination(repo).ok();
    let _binding = current
        .as_ref()
        .map(|to| crate::agent_store::hold_binding(repo, &to.agent))
        .transpose()?;
    let dir = root(repo);
    if let Some(pair) = Pair::read(&dir, name) {
        anyhow::ensure!(
            pair.received
                && received_here(repo, &pair)
                && pair.damage().is_none()
                && pair.live_attempt().is_none(),
            "block `{name}` is still open or its answer is owed; use another name"
        );
    }
    let f = files(&dir, name);
    remove_if_present(&f.answer)?;
    remove_if_present(&f.marker)?;
    remove_if_present(&f.attempt)?;
    match author {
        Some(a) => publish(&f.creator, a.as_str().as_bytes())?,
        None => {
            remove_if_present(&f.creator)?;
        }
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
    fn nudge(&mut self, text: &str, destination: &Destination) -> Outcome;
}

#[cfg(test)]
impl<F: FnMut(&str) -> Outcome> Nudger for F {
    fn nudge(&mut self, text: &str, _destination: &Destination) -> Outcome {
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
    pub(crate) fn into_answer(self, name: &str) -> anyhow::Result<PathBuf> {
        match self {
            Resolved::Answered(path) => Ok(path),
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
    resolve(repo, name, shown, By::Unattested, How::Plain(message))?.into_answer(name)
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
                     \"…\"`"
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
            let _roster = hold_roster(repo)?;
            let _lock = lock(repo)?;
            let open = open_pair(&dir, &f, name, shown)?;
            attested(&open, by, name)?;
            let Some(a) = open.pair.live_attempt() else {
                anyhow::bail!(
                    "no nudge answering `{name}` is outstanding, so there is nothing \
                     to settle as delivered"
                );
            };
            let _binding = a
                .destination
                .as_ref()
                .map(|to| crate::agent_store::hold_binding(repo, &to.agent))
                .transpose()?;
            anyhow::ensure!(
                a.destination.is_some() && a.destination == destination(repo).ok(),
                "the outstanding attempt belongs to another or unknown master session; it cannot confirm delivery here; use --nudge --resend for the current master"
            );
            publish(
                &f.answer,
                stamp_nudged(&a.occurrence, &a.attempt, &a.text, a.destination.clone())?.as_bytes(),
            )
            .with_context(|| format!("recording answer for `{name}`"))?;
            // The nudged answer already makes the record inert.
            let _ = remove_if_present(&f.attempt);
            Ok(Resolved::Answered(f.answer))
        }
        How::Nudge { text, resend, via } => {
            let text = nonempty(text)?;
            let claimed = claim(repo, &dir, &f, name, shown, by, text, resend)?;
            let outcome = via.nudge(
                text,
                claimed
                    .destination
                    .as_ref()
                    .expect("new claim has destination"),
            );
            let _ = crate::cli::nudge_log::append(
                &agents_root(repo).join(
                    claimed
                        .destination
                        .as_ref()
                        .expect("new claim has destination")
                        .agent
                        .as_str(),
                ),
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
    let Some(pair) = Pair::read(dir, name) else {
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
        anyhow::bail!("`{name}` was already answered");
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
) -> anyhow::Result<Attempt> {
    let _roster = hold_roster(repo)?;
    let _lock = lock(repo)?;
    let open = open_pair(dir, f, name, shown)?;
    attested(&open, by, name)?;
    if let Some(a) = open.pair.live_attempt()
        && !resend
    {
        return Err(outstanding(name, a));
    }
    let claimed = Attempt {
        occurrence: stamped(f, &open)?,
        attempt: mint_id()?,
        text: text.to_string(),
        started: crate::cli::recovery::unix_now(),
        diagnostic: None,
        destination: Some(destination(repo)?),
    };
    publish(&f.attempt, &serde_json::to_vec(&claimed)?)?;
    Ok(claimed)
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
    let _roster = hold_roster(repo)?;
    let _lock = lock(repo)?;
    let _binding = claimed
        .destination
        .as_ref()
        .map(|to| crate::agent_store::hold_binding(repo, &to.agent))
        .transpose()?;
    let owned = claimed.owns(read_attempt(&f.attempt).as_ref());
    match outcome {
        Outcome::Submitted => {
            if claimed.destination.is_none() || claimed.destination != destination(repo).ok() {
                return Ok(Resolved::Unknown(
                    "master session changed during delivery; the attempt remains unsettled".into(),
                ));
            }
            if !owned {
                return Ok(Resolved::Superseded(format!(
                    "the text was delivered to the master, but this attempt's claim on \
                     `{name}` was replaced (a --resend or --delivered) before it settled, \
                     so nothing was recorded for it"
                )));
            }
            let pair = Pair::read(dir, name)
                .filter(|p| p.occurrence.as_deref() == Some(claimed.occurrence.as_str()));
            if let Some(why) = pair.as_ref().and_then(Pair::damage) {
                anyhow::bail!(
                    "the text was delivered to the master, but `{name}` has stored state \
                     clank cannot read, so no answer was recorded: {why}. Once it is repaired \
                     and you have seen the text arrive, run `clank block answer {name} \
                     --delivered --override-permission-classifier`"
                );
            }
            match pair {
                Some(p) if p.answer.is_none() => {
                    publish(
                        &f.answer,
                        stamp_nudged(
                            &claimed.occurrence,
                            &claimed.attempt,
                            &claimed.text,
                            claimed.destination.clone(),
                        )?
                        .as_bytes(),
                    )
                    .with_context(|| {
                        format!(
                            "the text was delivered to the master, but recording the answer for \
                             `{name}` failed; once you have seen it arrive, run `clank \
                             block answer {name} --delivered`"
                        )
                    })?;
                    // The nudged answer already makes the record inert.
                    let _ = remove_if_present(&f.attempt);
                    Ok(Resolved::Answered(f.answer.clone()))
                }
                _ => {
                    remove_if_present(&f.attempt)?;
                    Ok(Resolved::Superseded(format!(
                        "the text was delivered to the master for a question `{name}` that has \
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
    delivered: &Destination,
    current: impl Fn() -> bool,
    permit: &crate::upgrade::Permit,
) -> anyhow::Result<Receipt> {
    validate_name(&carries.name)?;
    let _roster = hold_roster(repo)?;
    let _lock = lock(repo)?;
    let _binding = crate::agent_store::hold_binding(repo, &delivered.agent)?;
    if !current() || destination(repo).ok().as_ref() != Some(delivered) {
        return Ok(Receipt::Unauthorized);
    }
    let dir = root(repo);
    let pair = Pair::read(&dir, &carries.name);
    let now = pair.as_ref().and_then(|p| p.carries(&carries.name));
    let Some(pair) = pair.filter(|_| now.as_ref() == Some(carries)) else {
        eprintln!(
            "clank: received nothing for block `{}`: it carried occurrence {} (question {}, \
             answer {}), and the block now holds {}",
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
            publish_under(&f.answer, stamp(&o, None, answer).as_bytes(), permit)?;
            publish_under(
                &f.question,
                stamp_question(&o, kind, &pair.question)?.as_bytes(),
                permit,
            )?;
            o
        }
    };
    let marker = Marker {
        occurrence,
        question_digest: carries.question_digest.clone(),
        answer_digest: carries.answer_digest.clone(),
        destination: Some(delivered.clone()),
    };
    publish_under(&f.marker, &serde_json::to_vec(&marker)?, permit)?;
    Ok(Receipt::Retired)
}

async fn run_clean(args: super::BlockCleanArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let removed = clean(&repo)?;
    println!("cleaned {removed} file(s)");
    Ok(())
}

/// Remove the repository's received pairs and every answer, receipt or attempt
/// record no current ask owns. Housekeeping only: it never decides what
/// is owed, and never removes an outstanding attempt.
pub(crate) fn clean(repo: &Path) -> anyhow::Result<usize> {
    let _roster = hold_roster(repo)?;
    let _lock = lock(repo)?;
    let current = destination(repo).ok();
    let _binding = current
        .as_ref()
        .map(|to| crate::agent_store::hold_binding(repo, &to.agent))
        .transpose()?;
    let dir = root(repo);
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
        if pair.received && received_here(repo, &pair) && pair.live_attempt().is_none() {
            // The question first: a leftover answer or marker names a
            // dead occurrence, which no reader takes for a live one.
            for path in [&f.question, &f.answer, &f.marker, &f.attempt, &f.creator] {
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
    /// answered, retried, owed or received until a person repairs it.
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

/// Every block not yet received, pending or answered, and every one
/// whose state cannot be read. Migrates legacy custody first; otherwise it
/// neither receives answers nor removes questions.
pub fn scan_blocks(repo: &Path) -> Vec<BlockEntry> {
    // Inspecting an ordinary Git repo must not initialize Clank storage.
    if !root(repo).exists() {
        return Vec::new();
    }
    let _lock = match lock(repo) {
        Ok(lock) => lock,
        Err(e) => {
            return vec![BlockEntry {
                creator: String::new(),
                name: "block-storage".into(),
                occurrence: None,
                kind: Stored::Unreadable(e.to_string()),
                question: format!("Block storage needs repair: {e:#}"),
                answer: None,
                nudge: None,
                unreadable: Some(e.to_string()),
            }];
        }
    };
    let dir = root(repo);
    question_names(&dir)
        .into_iter()
        .filter_map(|name| {
            let pair = Pair::read(&dir, &name)?;
            if pair.received && pair.damage().is_none() && received_here(repo, &pair) {
                return None;
            }
            let unreadable = pair.damage();
            Some(BlockEntry {
                creator: std::fs::read_to_string(files(&dir, &name).creator).unwrap_or_default(),
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
            })
        })
        .collect()
}

fn received_here(repo: &Path, pair: &Pair) -> bool {
    if pair.damage().is_some() {
        return false;
    }
    let marker = pair.marker.as_ref().and_then(Stored::valid);
    // Pre-upgrade receipts already settled this occurrence. Preserve their
    // disposition; migration does not invent a new delivery or permission.
    if pair.received
        && (marker.as_ref().is_some_and(|m| m.destination.is_none())
            || matches!(
                pair.disposition,
                Stored::Valid(Some(Disposition::Nudged {
                    destination: None,
                    ..
                }))
            ))
    {
        return true;
    }
    let Ok(now) = destination(repo) else {
        return false;
    };
    if let Stored::Valid(Some(Disposition::Nudged {
        destination: Some(to),
        ..
    })) = &pair.disposition
        && to == &now
    {
        return true;
    }
    marker.is_some_and(|m| pair.received && m.destination.as_ref() == Some(&now))
}

pub(crate) fn key(name: &str) -> String {
    name.to_owned()
}

pub(crate) fn ended(
    repo: &Path,
    author: &clank_core::AgentLabel,
) -> std::collections::BTreeMap<String, u64> {
    let Ok(_lock) = lock(repo) else {
        return Default::default();
    };
    let dir = root(repo);
    let is_master = master(repo).ok().as_ref() == Some(author);
    question_names(&dir)
        .into_iter()
        .filter_map(|name| {
            let pair = Pair::read(&dir, &name).filter(|p| p.damage().is_none())?;
            let f = files(&dir, &name);
            let path = if is_master {
                if !received_here(repo, &pair) {
                    return None;
                }
                if f.marker.exists() {
                    f.marker
                } else {
                    f.answer
                }
            } else {
                pair.answer.as_ref()?;
                f.answer
            };
            let at = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
            Some((
                key(&name),
                at.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs(),
            ))
        })
        .collect()
}

/// An answer `agent` is owed: answered and not yet received.
pub(crate) struct OwedAnswer {
    pub(crate) answer: String,
    pub(crate) carries: AnswerRef,
}

/// Decisions owed to the current master, by name. Legacy custody is migrated
/// first. Delivery to a different session never proves this one received it;
/// unreadable disposition stays unresolved rather than being delivered again.
pub(crate) fn owed_answers(repo: &Path, agent: &clank_core::AgentLabel) -> Vec<OwedAnswer> {
    if master(repo).ok().as_ref() != Some(agent) {
        return vec![];
    }
    let Ok(_lock) = lock(repo) else {
        return vec![];
    };
    let dir = root(repo);
    question_names(&dir).into_iter().filter_map(|name| {
        let pair = Pair::read(&dir, &name).filter(|p| p.damage().is_none())?;
        if received_here(repo, &pair) { return None; }
        let carries = pair.carries(&name)?;
        let mut answer = pair.answer?;
        if matches!(pair.kind, Stored::Valid(Kind::Permission { .. })) {
            answer = format!("Prior human decision (not permission approval for this session; obtain any approval your harness requires): {answer}");
        }
        Some(OwedAnswer { answer, carries })
    }).collect()
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
        assert_eq!(blocks[0].creator, "alice");
        assert_eq!(blocks[0].name, "fix-api");
        assert_eq!(blocks[0].question, "What should the return type be?");
        assert!(blocks[0].answer.is_none());
    }

    #[test]
    fn scan_migrates_legacy_plan_scoped_questions_without_losing_them() {
        // Even an old plan-scoped question survives the ownership migration.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path()
                .join(".clank/agents/bob/blocks/auth/need-creds.md"),
            "Which OAuth provider?",
        );
        let blocks = scan_blocks(dir.path());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "need-creds");
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
            write(
                &root.join(format!(".clank/agents/{agent}/blocks/{name}.md")),
                q,
            );
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
            !root.join(".clank/agents/codex/unblocks/second.md").exists(),
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

    fn marker(repo: &Path, name: &str) -> PathBuf {
        repo.join(format!(".clank/unblocks/{name}.received"))
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
        written_at(&repo.join(".clank/unblocks/q.md"), 7000);
        assert_eq!(
            ended(repo, &bob),
            [(key("q"), 7000)].into(),
            "for another agent, at the answer"
        );
        assert!(
            ended(repo, &alice()).is_empty(),
            "for its asker, not before the answer is received"
        );

        assert_eq!(
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Retired
        );
        written_at(&marker(repo, "q"), 7600);
        assert_eq!(ended(repo, &alice()), [(key("q"), 7600)].into());
        assert_eq!(ended(repo, &bob)[&key("q")], 7000);

        clean(repo).unwrap();
        assert!(ended(repo, &alice()).is_empty(), "cleaned away: undatable");
        assert!(ended(repo, &bob).is_empty());
    }

    /// A process that has seen a newer clank installed takes no new
    /// question and no new answer, and still marks as received an answer
    /// its agent was already shown.
    #[test]
    fn a_stale_process_asks_and_answers_nothing_and_still_receives() {
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

        assert_eq!(
            receive(
                repo,
                &shown,
                &destination(repo).unwrap(),
                || true,
                &authority.settling()
            )
            .unwrap(),
            Receipt::Retired
        );
        assert!(marker(repo, "q").exists());
    }

    #[test]
    fn a_received_answer_is_retired_by_its_marker_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("yes"));
        assert_eq!(
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Retired
        );
        assert!(marker(repo, "q").exists());
        assert!(
            repo.join(".clank/blocks/q.md").exists() && repo.join(".clank/unblocks/q.md").exists(),
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
            let held = lock(repo).unwrap();
            std::thread::scope(|s| {
                s.spawn(|| {
                    crate::cli::presentation::record_delivered(
                        repo,
                        &alice(),
                        clank_core::Tool::Codex,
                        "block-test-alice",
                        &carried,
                        || {
                            let _ = observed.send(());
                            authorized.load(Ordering::SeqCst)
                        },
                        &crate::upgrade::process().settling(),
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
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || {
                    rebind.get_or_init(|| {
                        s.spawn(|| {
                            crate::agent_store::update_agent_config(repo, &alice(), |_| {})
                                .unwrap();
                            marker(repo, "q").exists()
                        })
                    });
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    true
                },
                &crate::upgrade::process().settling(),
            )
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
        assert_eq!(
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Retired
        );
        let first = std::fs::read(marker(repo, "q")).unwrap();
        assert_eq!(
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Retired
        );
        assert_eq!(std::fs::read(marker(repo, "q")).unwrap(), first);
        let markers = std::fs::read_dir(repo.join(".clank/unblocks"))
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
            receive(
                repo,
                &first,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling(),
            )
            .unwrap();
            if interleave == "clean" {
                clean(repo).unwrap();
            }
            let second = asked_and_answered(repo, "q", "Proceed?", "yes");
            if interleave == "clean" {
                clean(repo).unwrap();
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
                receive(
                    repo,
                    &late,
                    &destination(repo).unwrap(),
                    || true,
                    &crate::upgrade::process().settling()
                )
                .unwrap(),
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
        receive(
            repo,
            &a,
            &destination(repo).unwrap(),
            || true,
            &crate::upgrade::process().settling(),
        )
        .unwrap();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        assert!(!repo.join(".clank/unblocks/q.md").exists());
        assert!(!marker(repo, "q").exists());
        let blocks = scan_blocks(repo);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].answer, None, "owed to the human again");
        assert!(owed(repo).is_empty(), "no old answer is rendered for it");
        assert_eq!(
            receive(
                repo,
                &a,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Stale
        );
        answer_block(repo, "q", "no", None).unwrap();
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("no"));
    }

    /// Whatever an interrupted clean leaves behind names a dead
    /// occurrence, so a new ask ignores it even where nothing removed it.
    #[test]
    fn a_leftover_from_another_occurrence_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let a = asked_and_answered(repo, "q", "Proceed?", "yes");
        receive(
            repo,
            &a,
            &destination(repo).unwrap(),
            || true,
            &crate::upgrade::process().settling(),
        )
        .unwrap();
        let answer = std::fs::read(repo.join(".clank/unblocks/q.md")).unwrap();
        let receipt = std::fs::read(marker(repo, "q")).unwrap();
        ask(repo, &alice(), "q", "Proceed?").unwrap();
        write(
            &repo.join(".clank/unblocks/q.md"),
            std::str::from_utf8(&answer).unwrap(),
        );
        write(&marker(repo, "q"), std::str::from_utf8(&receipt).unwrap());
        assert_eq!(scan_blocks(repo)[0].answer, None);
        assert!(owed(repo).is_empty());
        answer_block(repo, "q", "no", None).unwrap();
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("no"));
    }

    /// A pair asked and answered before occurrences existed is owed once
    /// more, and reading it changes nothing; receiving it stamps both
    /// halves with one occurrence and keeps the human's answer.
    #[test]
    fn a_legacy_pair_is_owed_until_received_and_keeps_its_answer() {
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

        assert_eq!(
            receive(
                repo,
                &carries[0],
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
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
        write(&repo.join(".clank/blocks/q.md"), "Proceed?");
        write(
            &repo.join(".clank/unblocks/q.md"),
            &stamp("0123456789abcdef0123456789abcdef", None, "yes"),
        );
        let carries = owed(repo);
        assert_eq!(carries.len(), 1);
        assert_eq!(scan_blocks(repo)[0].answer.as_deref(), Some("yes"));
        assert_eq!(
            receive(
                repo,
                &carries[0],
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Retired
        );
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

    #[test]
    fn clean_removes_received_pairs_and_leftovers_and_keeps_what_is_owed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let done = asked_and_answered(repo, "done", "Proceed?", "yes");
        receive(
            repo,
            &done,
            &destination(repo).unwrap(),
            || true,
            &crate::upgrade::process().settling(),
        )
        .unwrap();
        let owed_now = asked_and_answered(repo, "owed", "Which?", "B");
        ask(repo, &alice(), "pending", "Why?").unwrap();
        write(
            &repo.join(".clank/unblocks/orphan.md"),
            &stamp("0123456789abcdef0123456789abcdef", None, "stale"),
        );
        assert_eq!(clean(repo).unwrap(), 5);
        let left: Vec<_> = scan_blocks(repo)
            .into_iter()
            .map(|b| (b.name, b.answer))
            .collect();
        assert_eq!(
            left,
            [("owed".into(), Some("B".into())), ("pending".into(), None)]
        );
        assert_eq!(owed(repo), [owed_now]);
        assert!(!repo.join(".clank/blocks/done.md").exists());
        assert!(!repo.join(".clank/unblocks/orphan.md").exists());
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
            remove_if_present(&f.marker).unwrap();
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

    /// The answer as written: its occurrence, its attempt when nudged,
    /// and its text.
    fn written(repo: &Path, name: &str) -> (Option<String>, Option<String>, String) {
        let raw = std::fs::read_to_string(answer_path(repo, name)).unwrap();
        let s = unstamp(&raw);
        let Stored::Valid(disposition) = s.disposition() else {
            panic!("an unreadable disposition: {raw}");
        };
        let attempt = disposition.map(|Disposition::Nudged { attempt, .. }| attempt);
        (s.occurrence.map(str::to_owned), attempt, s.body.to_string())
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
            plain = Some(resolve(repo, "push", None, By::Person, How::Plain("plain")));
            submitted()
        })
        .unwrap();
        let err = second.unwrap().unwrap_err().to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(plain.unwrap().is_err());
        assert_eq!(written(repo, "push").2, "first");

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
        let unblocks = repo.join(".clank/unblocks");
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
            assert!(resolve(repo, "push", None, By::Person, how).is_err());
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
        let err = answer_block(repo, "q", "C", None).unwrap_err().to_string();
        assert!(err.contains("outstanding"), "{err}");
        assert!(matches!(
            delivered(repo, "q").unwrap(),
            Resolved::Answered(_)
        ));

        ask(repo, &alice(), "q", "Which?").unwrap();
        answer_block(repo, "q", "C", None).unwrap();
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
        assert_eq!(clean(repo).unwrap(), 2);
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
        clean(repo).unwrap();
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
            assert_eq!(clean(repo).unwrap(), 0, "{meta}");
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
            clean(repo).unwrap();
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
        assert_eq!(clean(repo).unwrap(), 0);
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
            clean(repo).unwrap();
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
        assert_eq!(
            receive(
                repo,
                &carries,
                &destination(repo).unwrap(),
                || true,
                &crate::upgrade::process().settling()
            )
            .unwrap(),
            Receipt::Stale
        );
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
        assert_eq!(clean(repo).unwrap(), 0);
        assert!(question_path(repo, "push").exists() && attempt_path(repo, "push").exists());
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
            Resolved::Answered(_)
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
    fn scan_returns_sorted_by_repository_name() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".clank/agents/bob/blocks/z.md"), "q");
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

// Test fixtures consume an actual answer before legally reusing its name.
#[cfg(test)]
pub(crate) fn test_receive(repo: &Path, name: &str) {
    let target = destination(repo).unwrap();
    let pair = Pair::read(&root(repo), name).unwrap();
    let carries = pair.carries(name).unwrap();
    assert_eq!(
        receive(
            repo,
            &carries,
            &target,
            || true,
            &crate::upgrade::process().settling()
        )
        .unwrap(),
        Receipt::Retired
    );
}

#[cfg(test)]
mod repo_tests;
