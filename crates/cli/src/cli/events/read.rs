//! Explicit CLI presentation: a checked content write, not an acknowledgement.
use super::{Sourced, events_context, read_all, render_full, resolve};
use crate::agent_env::{Ambient, Process};
use clank_core::{
    AgentLabel, SessionId, Tool,
    wait::{EventPresentation, WaitItem},
};
use std::{io::Write, path::Path};

struct Reader {
    author: AgentLabel,
    tool: Tool,
    session: SessionId,
}
impl Reader {
    fn capture(repo: &Path, requested: Option<&str>, env: &dyn Ambient) -> anyhow::Result<Self> {
        let author = crate::agent_env::resolve_identity_in(repo, env)?;
        anyhow::ensure!(
            requested.is_none_or(|label| label == author.as_str()),
            "events read can only present this session's own inbox; use events show for metadata"
        );
        let (tool, session) = crate::agent_env::detect_session_in(env)?.ok_or_else(|| {
            anyhow::anyhow!("events read needs a bound agent session; use events show for metadata")
        })?;
        let reader = Self {
            author,
            tool,
            session,
        };
        anyhow::ensure!(
            reader.current(repo),
            "this agent session is no longer bound; no content was read"
        );
        Ok(reader)
    }
    fn current(&self, repo: &Path) -> bool {
        crate::agent_store::binding_matches(repo, &self.author, self.tool, self.session.as_str())
    }
}

pub(super) async fn run(args: crate::cli::EventsShowArgs) -> anyhow::Result<()> {
    let repo = crate::cli::resolve_repo(args.repo.as_deref())?;
    let reader = Reader::capture(&repo, args.author.as_deref(), &Process)?;
    let (dir, prompts) = events_context(Some(&repo), Some(reader.author.as_str()))?;
    let rows = read_all(&dir)?;
    let selected = resolve(&rows, &args.id)?;
    let selector = super::Selector::qualified(&selected.source, selected.row.seq).to_string();
    let prepared = crate::cli::wake_receipt::prepare(
        &repo,
        &reader.author,
        match &selected.row.item {
            WaitItem::CommandEvent {
                name,
                id,
                reply_route,
                source_cwd,
                failure: false,
                ..
            } => Some(crate::cli::wake_receipt::Candidate {
                source: name,
                id,
                selector: &selector,
                cwd: source_cwd
                    .as_deref()
                    .or_else(|| reply_route.as_ref().map(|route| route.cwd.as_path())),
            }),
            _ => None,
        }
        .into_iter(),
    );
    {
        let stdout = std::io::stdout();
        present(
            &repo,
            &reader,
            &rows,
            &prompts,
            &args.id,
            args.json,
            &mut stdout.lock(),
        )?;
    }
    crate::cli::wake_receipt::dispatch(&repo, &reader.author, prepared, reader.tool, || {
        reader.current(&repo)
    })
    .await;
    Ok(())
}

fn present(
    repo: &Path,
    reader: &Reader,
    rows: &[Sourced],
    prompts: &std::collections::HashMap<String, String>,
    id: &str,
    json: bool,
    writer: &mut impl Write,
) -> anyhow::Result<()> {
    let selected = resolve(rows, id)?;
    anyhow::ensure!(
        reader.current(repo),
        "agent binding changed before content output"
    );
    render_full(rows, prompts, id, json, writer)?;
    writer.flush()?;
    // Rebinding during the write cannot claim delivery to the replacement.
    anyhow::ensure!(
        reader.current(repo),
        "agent binding changed during content output; no receipt recorded"
    );
    crate::cli::presentation::record(
        repo,
        &reader.author,
        &[EventPresentation {
            source: selected.source.clone(),
            seq: selected.row.seq,
            ident: selected.row.ident.clone(),
        }],
        || reader.current(repo),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_env::Stated;
    use crate::cli::github::log::{EventLog, Transport};

    fn fixture() -> (tempfile::TempDir, Reader, Vec<Sourced>) {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("reader").unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &author,
            Tool::Codex,
            &SessionId::parse("read-session").unwrap(),
        )
        .unwrap();
        let reader = Reader::capture(
            repo.path(),
            None,
            &Stated::saying(&[
                ("CLANK_AGENT", "reader"),
                ("CODEX_THREAD_ID", "read-session"),
            ]),
        )
        .unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("reader/events");
        let mut log = EventLog::open(&dir, "messages").unwrap();
        log.append_inbox(
            Transport::Message,
            Some("handle"),
            None,
            None,
            &WaitItem::Message {
                presentation: None,
                handle: "handle".into(),
                from: Some("sender".into()),
                repo: None,
                subject: "secret subject".into(),
                body: "secret subject\nfull content 雪".into(),
                requires_reply: true,
                reply_route: Some(clank_core::wait::MessageReplyRoute {
                    repo: repo.path().to_path_buf(),
                    agent: author,
                }),
                selector: None,
            },
        )
        .unwrap();
        let rows = read_all(&dir).unwrap();
        (repo, reader, rows)
    }
    fn state(repo: &Path) -> Sourced {
        read_all(&crate::agent_store::agents_root(repo).join("reader/events"))
            .unwrap()
            .remove(0)
    }
    #[test]
    fn metadata_hides_content_and_successful_read_only_stamps_delivery() {
        let (repo, reader, rows) = fixture();
        let metadata = super::super::metadata_row(&rows, &rows[0]).to_string();
        assert!(!metadata.contains("secret"));
        assert!(!metadata.contains("subject"));
        assert!(!metadata.contains("body"));
        assert!(!super::super::metadata_description(&rows[0].row.item).contains("secret"));
        assert!(state(repo.path()).row.woken_at.is_none());
        let mut output = Vec::new();
        present(
            repo.path(),
            &reader,
            &rows,
            &Default::default(),
            "messages@1",
            true,
            &mut output,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(body["item"]["body"], "secret subject\nfull content 雪");
        let stamped = state(repo.path());
        assert!(stamped.row.woken_at.is_some());
        assert!(!stamped.row.acked);
        let before = std::fs::read(
            crate::agent_store::agents_root(repo.path()).join("reader/events/messages.jsonl"),
        )
        .unwrap();
        present(
            repo.path(),
            &reader,
            &rows,
            &Default::default(),
            "messages@1",
            false,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            before,
            std::fs::read(
                crate::agent_store::agents_root(repo.path()).join("reader/events/messages.jsonl")
            )
            .unwrap()
        );
    }
    /// A RECEIPT-AWARE read hands over the whole message, lines intact.
    ///
    /// The event is built by real ingestion from one JSONL record, so this
    /// is the body that command sources actually store — not a body typed
    /// into the fixture. Read is where a person or an agent goes for the
    /// full text when the wake or the row only summarised it, so if a line
    /// were lost anywhere upstream, this is where it would show.
    ///
    /// Lives here rather than in the integration harness because the
    /// receipt is recorded against a bound SESSION, and the harness clears
    /// session identity on purpose; this module's `Ambient` seam supplies
    /// one without touching the process environment other tests share.
    #[test]
    fn a_read_hands_over_the_whole_ingested_message_and_stamps_its_receipt() {
        let (repo, reader, _) = fixture();
        let dir = crate::agent_store::agents_root(repo.path()).join("reader/events");
        let src = clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: "signal".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let raw = "Para one.\n\n  indented\n\tTabbed\tcols\nüñí 中文 ✓\n\
                   Colour \u{1b}[31mred\u{1b}[0m done\r\nlast";
        let expected = "Para one.\n\n  indented\n\tTabbed\tcols\nüñí 中文 ✓\n\
                        Colour red done\nlast";
        let record = serde_json::json!({"id": "m1", "kind": "message", "content": raw}).to_string();
        let seen = crate::cli::source_ids::Seen::at(&dir, "command-signal-0");
        let Some(crate::cli::command_source::Ingested::Emit { item, .. }) =
            crate::cli::command_source::ingest_line(&src, &seen, &record)
        else {
            panic!("the record ingests")
        };
        let mut log = EventLog::open(&dir, "command-signal-0").unwrap();
        log.append_inbox(Transport::Command, Some("m1"), None, None, &item)
            .unwrap();
        let rows = read_all(&dir).unwrap();
        let log_path = dir.join("command-signal-0.jsonl");

        // What an agent reading `--json` is handed.
        let mut json = Vec::new();
        present(
            repo.path(),
            &reader,
            &rows,
            &Default::default(),
            "command-signal-0@1",
            true,
            &mut json,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(json["item"]["content"], expected);

        // What a person reads: the body block, its continuation lines
        // indented under `body:` and recovered exactly by removing that.
        let mut human = Vec::new();
        present(
            repo.path(),
            &reader,
            &rows,
            &Default::default(),
            "command-signal-0@1",
            false,
            &mut human,
        )
        .unwrap();
        let human = String::from_utf8(human).unwrap();
        let start =
            human.find("body:      ").expect("the full body is shown") + "body:      ".len();
        let mut lines = human[start..].split('\n');
        let mut body = vec![lines.next().unwrap().to_string()];
        body.extend(
            lines
                .map_while(|l| l.strip_prefix("           "))
                .map(str::to_string),
        );
        assert_eq!(body.join("\n"), expected, "{human}");

        assert!(
            state_of(&dir).row.woken_at.is_some(),
            "delivery was recorded"
        );
        assert!(
            String::from_utf8(std::fs::read(&log_path).unwrap())
                .unwrap()
                .contains(&serde_json::to_string(expected).unwrap()),
            "and reading never rewrote the stored body"
        );
    }
    fn state_of(dir: &Path) -> Sourced {
        read_all(dir).unwrap().remove(0)
    }
    struct Broken {
        flush: bool,
    }
    impl Write for Broken {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.flush {
                Ok(b.len())
            } else {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("flush failed"))
        }
    }
    #[test]
    fn failed_write_or_flush_does_not_stamp() {
        let (repo, reader, rows) = fixture();
        for flush in [false, true] {
            assert!(
                present(
                    repo.path(),
                    &reader,
                    &rows,
                    &Default::default(),
                    "1",
                    true,
                    &mut Broken { flush }
                )
                .is_err()
            );
            assert!(state(repo.path()).row.woken_at.is_none());
        }
    }
    #[test]
    fn foreign_author_or_stale_binding_cannot_read_content() {
        let (repo, reader, rows) = fixture();
        let env = Stated::saying(&[
            ("CLANK_AGENT", "reader"),
            ("CODEX_THREAD_ID", "read-session"),
        ]);
        assert!(Reader::capture(repo.path(), Some("other"), &env).is_err());
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &reader.author,
            Tool::Codex,
            &SessionId::parse("replacement-session").unwrap(),
        )
        .unwrap();
        let mut output = Vec::new();
        assert!(
            present(
                repo.path(),
                &reader,
                &rows,
                &Default::default(),
                "1",
                true,
                &mut output
            )
            .is_err()
        );
        assert!(output.is_empty());
        assert!(state(repo.path()).row.woken_at.is_none());
    }
    #[test]
    fn rebinding_during_flush_does_not_stamp_replacement() {
        let (repo, reader, rows) = fixture();
        struct Rebind<'a> {
            repo: &'a Path,
            author: &'a AgentLabel,
        }
        impl Write for Rebind<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                crate::agent_store::bind_session_to_agent(
                    self.repo,
                    self.author,
                    Tool::Codex,
                    &SessionId::parse("replacement-session").unwrap(),
                )
                .unwrap();
                Ok(())
            }
        }
        assert!(
            present(
                repo.path(),
                &reader,
                &rows,
                &Default::default(),
                "1",
                true,
                &mut Rebind {
                    repo: repo.path(),
                    author: &reader.author
                }
            )
            .is_err()
        );
        assert!(state(repo.path()).row.woken_at.is_none());
    }
}
