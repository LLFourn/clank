//! Wake receipt routing, bounded stdin construction and hook-owned callback
//! dispatch after successful presentation. No idle worker or retry queue.

use clank_core::{agent_config::CommandSource, ids::AgentLabel};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) const MAX_IDS: usize = 256;
pub(crate) const MAX_BYTES: usize = 64 * 1024;
pub(crate) const DIAGNOSTIC_BYTES: usize = 4096;

/// Captured from the current author's configured source and resolved WAL
/// key. The presentation route is independent of a row's saved reply route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub source: String,
    pub key: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
}
impl Route {
    pub(crate) fn capture(
        source: &CommandSource,
        key: &str,
        cwd: &Path,
        agent_dir: &Path,
    ) -> anyhow::Result<Option<Self>> {
        let Some(argv) = &source.receipt_command else {
            return Ok(None);
        };
        if let Some(why) = source.receipt_refusal() {
            anyhow::bail!("{why}");
        }
        anyhow::ensure!(
            key == source.source_key(),
            "receipt route does not match the configured source key"
        );
        Ok(Some(Self {
            source: source.name.clone(),
            key: key.into(),
            argv: argv.clone(),
            cwd: cwd.into(),
            state_dir: super::command_source::backend_state_dir(agent_dir, key),
        }))
    }
}

/// The presenter supplies original IDs from authoritative selected WAL
/// rows. Rendered hints, loose source names and ack selectors are not IDs.
pub(crate) struct Presented<'a> {
    pub route: &'a Route,
    pub id: &'a str,
}
pub(crate) struct Batch<'a> {
    pub route: &'a Route,
    pub stdin: Vec<u8>,
    ids: Vec<&'a str>,
}
#[derive(Default)]
pub(crate) struct Projection<'a> {
    pub batches: Vec<Batch<'a>>,
    pub omitted: usize,
    pub bytes: usize,
    pub ids: usize,
}

#[derive(serde::Serialize)]
struct Request<'a> {
    version: u8,
    kind: &'static str,
    source: &'a str,
    author: &'a str,
    ids: &'a [&'a str],
}
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("wake receipt stdin budget exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(value: &impl serde::Serialize, limit: usize) -> Option<Vec<u8>> {
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.bytes)
}

/// Each ID is encoded once into bounded storage before admission. Appending
/// preserves a valid object, avoids reencoding the batch for every ID and
/// retains borrowed IDs only for bounded per-route deduplication.
pub(crate) fn project<'a>(
    author: &AgentLabel,
    presented: impl IntoIterator<Item = Presented<'a>>,
) -> Projection<'a> {
    let mut result = Projection::default();
    for item in presented {
        let existing = result.batches.iter().position(|b| b.route == item.route);
        if existing.is_some_and(|i| result.batches[i].ids.contains(&item.id)) {
            continue;
        }
        let remaining = MAX_BYTES - result.bytes;
        if result.ids == MAX_IDS || item.id.len().saturating_add(2) > remaining {
            result.omitted += 1;
            continue;
        }
        let Some(id) = encode(&item.id, remaining) else {
            result.omitted += 1;
            continue;
        };
        let index = match existing {
            Some(index) => index,
            None => {
                // Check raw lengths before the serializer scans long names.
                if item
                    .route
                    .source
                    .len()
                    .saturating_add(author.as_str().len())
                    .saturating_add(id.len())
                    > remaining
                {
                    result.omitted += 1;
                    continue;
                }
                let Some(stdin) = encode(
                    &Request {
                        version: 1,
                        kind: "wake",
                        source: &item.route.source,
                        author: author.as_str(),
                        ids: &[],
                    },
                    remaining - id.len(),
                ) else {
                    result.omitted += 1;
                    continue;
                };
                result.bytes += stdin.len();
                result.batches.push(Batch {
                    route: item.route,
                    stdin,
                    ids: Vec::new(),
                });
                result.batches.len() - 1
            }
        };
        let batch = &mut result.batches[index];
        let extra = id.len() + usize::from(!batch.ids.is_empty());
        if extra > MAX_BYTES - result.bytes {
            result.omitted += 1;
            continue;
        }
        // Request's ids field is last, so only its closing ]} is removed.
        debug_assert!(batch.stdin.ends_with(b"]}"));
        batch.stdin.truncate(batch.stdin.len() - 2);
        if !batch.ids.is_empty() {
            batch.stdin.push(b',');
        }
        batch.stdin.extend_from_slice(&id);
        batch.stdin.extend_from_slice(b"]}");
        batch.ids.push(item.id);
        result.bytes += extra;
        result.ids += 1;
    }
    result
}

/// All coordination lives in the existing trusted per-uid runtime namespace,
/// keyed by repo plus validated author/source. These paths are not backend state.
pub(crate) fn lease_path(repo: &Path, author: &AgentLabel, key: &str) -> PathBuf {
    super::monitor::runtime_path(
        repo,
        &format!("wake-receipt-{}-{key}.lease", author.as_str()),
    )
}
pub(crate) fn owner_path(repo: &Path, author: &AgentLabel, key: &str) -> PathBuf {
    super::monitor::runtime_path(
        repo,
        &format!("wake-receipt-{}-{key}.owner.json", author.as_str()),
    )
}
pub(crate) fn diagnostic_path(repo: &Path, author: &AgentLabel) -> PathBuf {
    super::monitor::runtime_path(
        repo,
        &format!("wake-receipt-{}.diagnostic", author.as_str()),
    )
}

/// Selected typed wait identities. Cwd comes from the source host through
/// wait metadata, never from the hook caller or an immutable answer route.
pub(crate) struct Candidate<'a> {
    pub source: &'a str,
    pub id: &'a str,
    pub selector: &'a str,
    pub cwd: Option<&'a Path>,
}
struct OwnedBatch {
    route: Route,
    stdin: Vec<u8>,
}
#[derive(Default)]
pub(crate) struct Prepared {
    batches: Vec<OwnedBatch>,
    diagnostics: Diagnostics,
}
#[derive(Default)]
struct Diagnostics(String);
impl std::fmt::Write for Diagnostics {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let mut take = value.len().min(DIAGNOSTIC_BYTES - self.0.len());
        while !value.is_char_boundary(take) {
            take -= 1;
        }
        self.0.push_str(&value[..take]);
        Ok(())
    }
}

/// The private wait subprocess already obtained these identities from the
/// authoritative author WAL. Preserve that selected snapshot: re-reading the
/// inbox here would add locks/parsing and could erase a receipt for work that
/// really was presented but was concurrently acknowledged or compacted.
pub(crate) fn prepare<'a>(
    repo: &Path,
    author: &AgentLabel,
    candidates: impl Iterator<Item = Candidate<'a>>,
) -> Prepared {
    use clank_core::agent_config::WaitEventSource;
    use std::fmt::Write;
    let mut prepared = Prepared::default();
    let mut candidates = candidates.peekable();
    if candidates.peek().is_none() {
        return prepared;
    }
    let config = match crate::agent_store::load_agent_config(repo, author) {
        Ok(Some(config)) => config,
        Ok(None) => return prepared,
        Err(error) => {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt configuration: {error:#}"
            );
            return prepared;
        }
    };
    let agent_dir = crate::agent_store::agents_root(repo).join(author.as_str());
    // Borrow existing selected identities; neither whole WALs nor oversized
    // producer IDs are cloned into a second collection for receipts.
    let mut selected = Vec::new();
    for candidate in candidates {
        let Some((key, sequence)) = candidate.selector.rsplit_once('@') else {
            continue;
        };
        if !sequence.parse::<u64>().is_ok_and(|seq| seq > 0) {
            continue;
        }
        let Some(source) = config.wait_events.iter().find_map(|source| match source {
            WaitEventSource::Command(source)
                if source.receipt_command.is_some()
                    && source.name == candidate.source
                    && source.source_key() == key =>
            {
                Some(source)
            }
            _ => None,
        }) else {
            continue;
        };
        let Some(cwd) = candidate.cwd else {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt source execution context unavailable: {}; restart the monitor with the current CLI",
                source.name
            );
            continue;
        };
        if candidate.id.len().saturating_add(2) > MAX_BYTES {
            let _ = writeln!(prepared.diagnostics, "wake receipt oversized ID omitted");
            continue;
        }
        if selected.len() == MAX_IDS {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt candidate budget exceeded"
            );
            break;
        }
        selected.push((source, key, cwd, candidate.id));
    }
    let mut routes = Vec::new();
    let mut ids = Vec::new();
    for (source, key, cwd, id) in selected {
        // At most one captured route per configured source. Conflicting host
        // metadata cannot cause this hook to invoke two backend routes.
        let existing = routes.iter().position(|route: &Route| route.key == key);
        let index = match existing {
            Some(index) if routes[index].cwd == cwd => index,
            Some(_) => {
                let _ = writeln!(
                    prepared.diagnostics,
                    "wake receipt source execution context disagrees: {}",
                    source.name
                );
                continue;
            }
            None => match Route::capture(source, key, cwd, &agent_dir) {
                Ok(Some(route)) => {
                    routes.push(route);
                    routes.len() - 1
                }
                Ok(None) => continue,
                Err(error) => {
                    let _ = writeln!(prepared.diagnostics, "wake receipt route: {error:#}");
                    continue;
                }
            },
        };
        ids.push((index, id));
    }
    let projection = project(
        author,
        ids.into_iter().map(|(index, id)| Presented {
            route: &routes[index],
            id,
        }),
    );
    if projection.omitted != 0 {
        let _ = writeln!(
            prepared.diagnostics,
            "wake receipt omitted {} IDs within its byte/ID budget",
            projection.omitted
        );
    }
    prepared.batches = projection
        .batches
        .into_iter()
        .map(|batch| OwnedBatch {
            route: batch.route.clone(),
            stdin: batch.stdin,
        })
        .collect();
    prepared
}

struct Lease {
    _file: std::fs::File,
}
impl Lease {
    fn acquire(path: &Path) -> std::io::Result<Option<Self>> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // Locks are never unlinked: the open inode is dispatch authority.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

/// Called only after successful wake flush. No retry task, detached child,
/// per-event subprocess, or idle work. One absolute deadline covers all routes.
pub(crate) async fn dispatch(
    repo: &Path,
    author: &AgentLabel,
    mut prepared: Prepared,
    tool: clank_core::Tool,
    binding_current: impl Fn() -> bool,
) {
    use std::fmt::Write;
    if prepared.batches.is_empty() && prepared.diagnostics.0.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let call_deadline = deadline - std::time::Duration::from_millis(200);
    if let Err(error) = super::monitor::ensure_runtime_dir() {
        let _ = writeln!(
            prepared.diagnostics,
            "wake receipt runtime unavailable: {error}"
        );
        // An untrusted runtime directory must never be used for diagnostics.
        if tool != clank_core::Tool::Claude {
            eprint!("{}", prepared.diagnostics.0);
        }
        return;
    }
    for batch in prepared.batches {
        if !binding_current() {
            break;
        }
        if tokio::time::Instant::now() >= call_deadline {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt phase budget exhausted; later routes skipped"
            );
            break;
        }
        let _lease = match Lease::acquire(&lease_path(repo, author, &batch.route.key)) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                let _ = writeln!(
                    prepared.diagnostics,
                    "wake receipt source busy: {}",
                    batch.route.source
                );
                continue;
            }
            Err(error) => {
                let _ = writeln!(prepared.diagnostics, "wake receipt lease: {error}");
                continue;
            }
        };
        // The backend owns the contents. A legacy backlog may reach a
        // SessionStart receipt before any current producer created this dir.
        if let Err(error) = std::fs::create_dir_all(&batch.route.state_dir) {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt backend state directory: {error}"
            );
            continue;
        }
        let state_dir = Some(batch.route.state_dir.clone());
        let result = super::owned_callback::deliver(
            super::owned_callback::Request {
                argv: &batch.route.argv,
                cwd: &batch.route.cwd,
                state_dir: &state_dir,
                stdin: &batch.stdin,
            },
            call_deadline,
            deadline,
            &owner_path(repo, author, &batch.route.key),
        )
        .await;
        if let Err(error) = result {
            let _ = writeln!(
                prepared.diagnostics,
                "wake receipt {}: {error:#}",
                batch.route.source
            );
        }
    }
    if tool == clank_core::Tool::Claude {
        // One replaceable bounded diagnostic file; never append to Claude's
        // wake stderr. A successful phase clears stale failure text.
        let _ = std::fs::write(
            diagnostic_path(repo, author),
            prepared.diagnostics.0.as_bytes(),
        );
    } else if !prepared.diagnostics.0.is_empty() {
        eprint!("{}", prepared.diagnostics.0);
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    use super::*;
    use clank_core::{
        SessionId, Tool,
        agent_config::{AgentConfig, WaitEventSource},
        wait::WaitItem,
    };
    pub struct Fixture {
        _dir: tempfile::TempDir,
        pub repo: PathBuf,
        pub cwd: PathBuf,
        pub author: AgentLabel,
        pub source: CommandSource,
        pub key: String,
        pub id: String,
        pub selector: String,
        pub session: String,
        pub output: PathBuf,
        pub item: WaitItem,
    }
    impl Fixture {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = dunce::canonicalize(dir.path()).unwrap();
            let cwd = repo.join("source-cwd");
            std::fs::create_dir(&cwd).unwrap();
            let output = repo.join("receipt.json");
            let author = AgentLabel::parse("codex").unwrap();
            let source = CommandSource { interrupt_message: None, interrupt: false,
                name: "voice/a".into(), command: vec!["producer".into(), "events".into(), "--flag".into()],
                reply_command: Some(vec!["answer-backend".into()]), glyph: None,
                receipt_command: Some(vec!["/usr/bin/python3".into(), "-c".into(),
                    "import json,os,pathlib,sys; p=pathlib.Path(sys.argv[1]); assert pathlib.Path(str(p)+'.flushed').exists(); data=json.load(sys.stdin); data['cwd']=os.getcwd(); data['state']=os.environ['CLANK_SOURCE_STATE_DIR']; p.write_text(json.dumps(data)); print('callback stdout'); print('callback stderr',file=sys.stderr)".into(),
                    output.to_str().unwrap().into()]),
            };
            let key = source.source_key();
            let session = "receipt-session-a1".to_string();
            let cfg = AgentConfig {
                wait_events: vec![WaitEventSource::Command(source.clone())],
                ..AgentConfig::default()
            };
            crate::agent_store::save_agent_config(&repo, &author, &cfg).unwrap();
            crate::agent_store::bind_session_to_agent(
                &repo,
                &author,
                Tool::Codex,
                &SessionId::parse(&session).unwrap(),
            )
            .unwrap();
            let id = "utt/a/雪\n\"$()".to_string();
            let mut item = WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: source.name.clone(),
                id: id.clone(),
                event: "voice".into(),
                content: "do this".into(),
                at: None,
                failure: false,
                requires_reply: true,
                reply_route: Some(clank_core::wait::CommandReplyRoute {
                    command: source.reply_command.clone().unwrap(),
                    cwd: cwd.clone(),
                    state_dir: None,
                }),
                selector: None,
            };
            let mut log = super::super::github::log::EventLog::open(
                &crate::agent_store::agents_root(&repo)
                    .join(author.as_str())
                    .join("events"),
                &key,
            )
            .unwrap();
            let seq = log
                .append_inbox(
                    super::super::github::log::Transport::Command,
                    None,
                    None,
                    None,
                    &item,
                )
                .unwrap();
            item.set_presentation(log.presentation_key(
                seq,
                super::super::github::log::Transport::Command,
                None,
                None,
                None,
                &item,
            ));
            let selector = format!("{key}@{seq}");
            if let WaitItem::CommandEvent { selector: slot, .. } = &mut item {
                *slot = Some(selector.clone());
            }
            Self {
                _dir: dir,
                repo,
                cwd,
                author,
                source,
                key,
                id,
                selector,
                session,
                output,
                item,
            }
        }
        pub fn candidate(&self) -> Candidate<'_> {
            Candidate {
                source: &self.source.name,
                id: &self.id,
                selector: &self.selector,
                cwd: Some(&self.cwd),
            }
        }
        pub fn prepared(&self) -> Prepared {
            prepare(&self.repo, &self.author, std::iter::once(self.candidate()))
        }
        pub fn rows(&self) -> Vec<super::super::github::log::EventRow> {
            super::super::github::log::EventLog::open(
                &crate::agent_store::agents_root(&self.repo)
                    .join(self.author.as_str())
                    .join("events"),
                &self.key,
            )
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
        }
        pub fn marker(&self) -> PathBuf {
            PathBuf::from(format!("{}.flushed", self.output.display()))
        }
        pub fn update_source(&mut self, edit: impl FnOnce(&mut CommandSource)) {
            edit(&mut self.source);
            let mut cfg = crate::agent_store::load_agent_config(&self.repo, &self.author)
                .unwrap()
                .unwrap();
            cfg.wait_events = vec![WaitEventSource::Command(self.source.clone())];
            crate::agent_store::save_agent_config(&self.repo, &self.author, &cfg).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let owner = owner_path(&self.repo, &self.author, &self.key);
            for path in [
                lease_path(&self.repo, &self.author, &self.key),
                diagnostic_path(&self.repo, &self.author),
                owner.with_extension("tmp"),
                owner,
            ] {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(name: &str) -> CommandSource {
        CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: name.into(),
            command: vec!["producer".into(), "events".into(), "--flag".into()],
            reply_command: None,
            receipt_command: Some(vec!["receipt".into(), "--verbatim".into()]),
            glyph: None,
        }
    }
    fn route(name: &str) -> Route {
        let source = source(name);
        Route::capture(
            &source,
            &source.source_key(),
            Path::new("/repo"),
            Path::new("/repo/.clank/agents/codex"),
        )
        .unwrap()
        .unwrap()
    }
    #[test]
    fn prepare_checks_exact_source_scope_and_snapshots_selected_identity_routing() {
        let mut fixture = fixtures::Fixture::new();
        let prepared = fixture.prepared();
        assert_eq!(prepared.batches.len(), 1);
        assert_eq!(prepared.batches[0].route.cwd, fixture.cwd);
        assert_eq!(
            prepared.batches[0].route.argv,
            *fixture.source.receipt_command.as_ref().unwrap()
        );
        fixture
            .update_source(|source| source.receipt_command = Some(vec!["edited-handler".into()]));
        assert_ne!(
            prepared.batches[0].route.argv,
            *fixture.source.receipt_command.as_ref().unwrap()
        );
        for (name, id, selector) in [
            (
                fixture.source.name.as_str(),
                fixture.id.as_str(),
                format!("{}@0", fixture.key),
            ),
            (
                fixture.source.name.as_str(),
                fixture.id.as_str(),
                "command-other@1".into(),
            ),
            ("voice-a", fixture.id.as_str(), fixture.selector.clone()),
        ] {
            let prepared = prepare(
                &fixture.repo,
                &fixture.author,
                std::iter::once(Candidate {
                    source: name,
                    id,
                    selector: &selector,
                    cwd: Some(&fixture.cwd),
                }),
            );
            assert!(prepared.batches.is_empty());
        }
        fixture.update_source(|source| source.receipt_command = None);
        assert!(fixture.prepared().batches.is_empty());
        assert!(fixture.prepared().diagnostics.0.is_empty());
        assert_eq!(fixture.rows()[0].item, {
            let mut item = fixture.item.clone();
            if let clank_core::wait::WaitItem::CommandEvent {
                selector,
                presentation,
                ..
            } = &mut item
            {
                *presentation = None;
                *selector = None;
            }
            item
        });
    }

    #[test]
    fn selected_snapshot_survives_later_wal_changes_and_missing_context_is_diagnosed() {
        let fixture = fixtures::Fixture::new();
        let events = crate::agent_store::agents_root(&fixture.repo)
            .join(fixture.author.as_str())
            .join("events");
        std::fs::remove_dir_all(events).unwrap();
        let prepared = fixture.prepared();
        assert_eq!(
            prepared.batches.len(),
            1,
            "selected wake identity needs no second WAL read"
        );
        let missing = prepare(
            &fixture.repo,
            &fixture.author,
            std::iter::once(Candidate {
                source: &fixture.source.name,
                id: &fixture.id,
                selector: &fixture.selector,
                cwd: None,
            }),
        );
        assert!(missing.batches.is_empty());
        assert!(
            missing
                .diagnostics
                .0
                .contains("execution context unavailable")
        );
    }

    #[tokio::test]
    async fn dispatch_uses_original_ids_context_and_never_acknowledges_or_answers() {
        let fixture = fixtures::Fixture::new();
        std::fs::write(fixture.marker(), b"flushed").unwrap();
        dispatch(
            &fixture.repo,
            &fixture.author,
            fixture.prepared(),
            clank_core::Tool::Codex,
            || true,
        )
        .await;
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.output).unwrap()).unwrap();
        assert_eq!(receipt["ids"], serde_json::json!([fixture.id]));
        assert_eq!(receipt["cwd"], fixture.cwd.to_str().unwrap());
        assert_eq!(
            receipt["state"],
            super::super::command_source::backend_state_dir(
                &crate::agent_store::agents_root(&fixture.repo).join(fixture.author.as_str()),
                &fixture.key
            )
            .to_str()
            .unwrap()
        );
        assert!(
            super::super::command_source::backend_state_dir(
                &crate::agent_store::agents_root(&fixture.repo).join(fixture.author.as_str()),
                &fixture.key
            )
            .is_dir()
        );
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].acked);
        assert!(rows[0].reply.is_none());
        assert!(!rows[0].reply_accepted);
        assert!(!owner_path(&fixture.repo, &fixture.author, &fixture.key).exists());
        // A repeat is another permitted best-effort receipt, never completion.
        dispatch(
            &fixture.repo,
            &fixture.author,
            fixture.prepared(),
            clank_core::Tool::Codex,
            || true,
        )
        .await;
        assert!(!fixture.rows()[0].acked);
    }

    #[tokio::test]
    async fn busy_lease_and_uncertain_owner_prevent_another_callback() {
        let fixture = fixtures::Fixture::new();
        super::super::monitor::ensure_runtime_dir().unwrap();
        let lease = Lease::acquire(&lease_path(&fixture.repo, &fixture.author, &fixture.key))
            .unwrap()
            .unwrap();
        dispatch(
            &fixture.repo,
            &fixture.author,
            fixture.prepared(),
            clank_core::Tool::Claude,
            || true,
        )
        .await;
        assert!(!fixture.output.exists());
        assert!(
            std::fs::read_to_string(diagnostic_path(&fixture.repo, &fixture.author))
                .unwrap()
                .contains("source busy")
        );
        drop(lease);
        std::fs::write(
            owner_path(&fixture.repo, &fixture.author, &fixture.key),
            b"unrecognizable previous owner",
        )
        .unwrap();
        dispatch(
            &fixture.repo,
            &fixture.author,
            fixture.prepared(),
            clank_core::Tool::Claude,
            || true,
        )
        .await;
        assert!(!fixture.output.exists());
        assert!(owner_path(&fixture.repo, &fixture.author, &fixture.key).exists());
        assert!(!fixture.rows()[0].acked);
    }

    #[tokio::test]
    async fn one_phase_deadline_covers_blocked_stdin_and_descendant_cleanup() {
        let mut fixture = fixtures::Fixture::new();
        let pid_file = fixture.repo.join("group.pid");
        let descendant = fixture.repo.join("descendant.pid");
        fixture.update_source(|source| {
            source.receipt_command = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo $$ > \"$1\"; sleep 300 & echo $! > \"$2\"; wait".into(),
                "fixture".into(),
                pid_file.to_str().unwrap().into(),
                descendant.to_str().unwrap().into(),
            ])
        });
        let mut prepared = fixture.prepared();
        assert_eq!(prepared.batches.len(), 1);
        // A maximum-size stdin request fills a non-reading callback's pipe.
        prepared.batches[0].stdin = vec![b'x'; MAX_BYTES];
        // A later route must not get a fresh one-second budget.
        let mut later = fixture.source.clone();
        later.name = "later".into();
        later.receipt_command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "touch \"$1\"".into(),
            "fixture".into(),
            fixture.output.to_str().unwrap().into(),
        ]);
        prepared.batches.push(OwnedBatch {
            route: Route::capture(
                &later,
                &later.source_key(),
                &fixture.cwd,
                &crate::agent_store::agents_root(&fixture.repo).join(fixture.author.as_str()),
            )
            .unwrap()
            .unwrap(),
            stdin: b"{}".to_vec(),
        });
        let start = std::time::Instant::now();
        dispatch(
            &fixture.repo,
            &fixture.author,
            prepared,
            clank_core::Tool::Claude,
            || true,
        )
        .await;
        assert!(start.elapsed() < std::time::Duration::from_millis(1500));
        assert!(start.elapsed() >= std::time::Duration::from_millis(700));
        assert!(!fixture.output.exists());
        let pgid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let descendant: i32 = std::fs::read_to_string(descendant)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            crate::signal::probe_group(crate::signal::Pgid::new(pgid).unwrap()),
            crate::signal::Probe::Gone
        );
        assert!(matches!(
            crate::signal::probe_process(descendant),
            crate::signal::Probe::Gone
        ));
        assert!(!owner_path(&fixture.repo, &fixture.author, &fixture.key).exists());
        assert!(!fixture.rows()[0].acked);
    }

    #[test]
    fn batching_preserves_original_ids_and_separates_routes_without_ack_fields() {
        let author = AgentLabel::parse("codex").unwrap();
        let a = route("voice/a");
        let b = route("voice-a");
        let id = "utt/a/雪\n\"$()`";
        let projection = project(
            &author,
            [
                Presented { route: &a, id },
                Presented { route: &a, id },
                Presented { route: &b, id },
                Presented {
                    route: &a,
                    id: "second",
                },
            ],
        );
        assert_eq!(projection.ids, 3);
        assert_eq!(projection.batches.len(), 2);
        assert_eq!(projection.omitted, 0);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&projection.batches[0].stdin).unwrap(),
            serde_json::json!({"version":1,"kind":"wake","source":"voice/a","author":"codex","ids":[id,"second"]})
        );
        assert_ne!(a.key, b.key);
        assert_ne!(a.state_dir, b.state_dir);
        assert_eq!(
            projection.bytes,
            projection
                .batches
                .iter()
                .map(|b| b.stdin.len())
                .sum::<usize>()
        );
    }
    #[test]
    fn whole_wake_budgets_omit_complete_ids_before_retaining_them() {
        let author = AgentLabel::parse("codex").unwrap();
        let a = route("voice");
        let huge = "x".repeat(MAX_BYTES * 10);
        let ids: Vec<_> = (0..300).map(|i| format!("utt/{i}")).collect();
        let projection = project(
            &author,
            std::iter::once(Presented {
                route: &a,
                id: &huge,
            })
            .chain(ids.iter().map(|id| Presented { route: &a, id })),
        );
        assert_eq!(projection.ids, MAX_IDS);
        assert_eq!(projection.omitted, 45);
        assert!(projection.bytes <= MAX_BYTES);
        let escaped = "\n".repeat(MAX_BYTES / 2);
        let projection = project(
            &author,
            [
                Presented {
                    route: &a,
                    id: &escaped,
                },
                Presented {
                    route: &a,
                    id: "small",
                },
            ],
        );
        assert_eq!(projection.ids, 1);
        assert_eq!(projection.omitted, 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&projection.batches[0].stdin).unwrap()["ids"],
            serde_json::json!(["small"])
        );
    }
    #[test]
    fn encoded_byte_budget_is_shared_by_routes_and_keeps_whole_ids() {
        let author = AgentLabel::parse("codex").unwrap();
        let a = route("voice/a");
        let b = route("voice/b");
        let overhead = project(&author, [Presented { route: &a, id: "" }]).bytes;
        let exact = "x".repeat(MAX_BYTES - overhead);
        let full = project(
            &author,
            [
                Presented {
                    route: &a,
                    id: &exact,
                },
                Presented {
                    route: &b,
                    id: "next",
                },
            ],
        );
        assert_eq!(full.bytes, MAX_BYTES);
        assert_eq!(full.ids, 1);
        assert_eq!(full.omitted, 1);
        assert_eq!(full.batches.len(), 1);
        let parsed: serde_json::Value = serde_json::from_slice(&full.batches[0].stdin).unwrap();
        assert_eq!(parsed["ids"][0], exact);
        let over = format!("{exact}x");
        let skipped = project(
            &author,
            [
                Presented {
                    route: &a,
                    id: &over,
                },
                Presented {
                    route: &b,
                    id: "next",
                },
            ],
        );
        assert_eq!(skipped.ids, 1);
        assert_eq!(skipped.omitted, 1);
        assert_eq!(skipped.batches[0].route.key, b.key);
    }
    #[test]
    fn capture_uses_exact_key_and_does_nothing_without_a_handler() {
        let mut source = source("voice");
        let before = source.source_key();
        let argv = source.command.clone();
        assert!(Route::capture(&source, "wrong", Path::new("/repo"), Path::new("/agent")).is_err());
        let captured = Route::capture(
            &source,
            &before,
            Path::new("/resolved/cwd"),
            Path::new("/agent"),
        )
        .unwrap()
        .unwrap();
        source.receipt_command = Some(vec!["edited".into()]);
        assert_eq!(captured.argv, vec!["receipt", "--verbatim"]);
        assert_eq!(captured.cwd, Path::new("/resolved/cwd"));
        assert_eq!(source.source_key(), before);
        assert_eq!(source.command, argv);
        source.receipt_command = None;
        assert!(
            Route::capture(&source, "wrong", Path::new("/repo"), Path::new("/agent"))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn runtime_paths_are_scoped_outside_backend_state() {
        let a = AgentLabel::parse("codex").unwrap();
        let b = AgentLabel::parse("claude").unwrap();
        let route = route("voice");
        let path = diagnostic_path(Path::new("/repo"), &a);
        assert_ne!(path, diagnostic_path(Path::new("/other"), &a));
        assert_ne!(path, diagnostic_path(Path::new("/repo"), &b));
        assert_eq!(
            path.parent(),
            Some(super::super::monitor::runtime_dir(unsafe { libc::getuid() }).as_path())
        );
        assert_ne!(
            lease_path(Path::new("/repo"), &a, &route.key),
            owner_path(Path::new("/repo"), &a, &route.key)
        );
    }
}
