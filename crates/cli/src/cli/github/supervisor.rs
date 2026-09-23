//! Driving one armed watch: the ingest lease, the follower tail, and
//! the takeover when the holder goes away.
//!
//! One process may have several agents waiting on the same repo. Only
//! the LEASE holder runs the transports and writes the WAL; the rest
//! tail what it wrote and present from there, and whoever gets the
//! lease next takes over mid-stream. This module is the arm-to-exit
//! lifecycle around that; [`super::coordinator`] is the tick it runs.

use clank_core::agent_config::GithubSource;
use clank_core::wait::WaitItem;

use super::ci::{CiReader, GhCi};
use super::coordinator::{ChecksSide, FeedSide, poll_loop};
use super::feed::{EventFetcher, GhFetcher, effective_interval, in_scope};
use super::relay::{GhForwarder, RelayGuard, RelayInbox, start_relay};
use super::session::{GhLogin, LoginResolver, shared_session};

/// Sidecar policy: CONFIG-origin watches never carry sidecars — the
/// agent config IS their prompt store, and every arm REMOVES any
/// leftover so a watch deleted from config cannot resurrect its
/// intent from stale metadata (this also self-heals sidecars
/// written before provenance existed). CLI-origin (`--event`)
/// watches write theirs — the only store such an arm has — and
/// remove it when re-armed promptless.
pub(crate) fn write_watch_sidecar(dir: &std::path::Path, src: &GithubSource, origin: SourceOrigin) {
    let meta = dir.join(format!("{}.meta.json", source_key(src)));
    let res = match (origin, &src.prompt) {
        (SourceOrigin::Cli, Some(p)) => std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&meta, serde_json::json!({ "prompt": p }).to_string())),
        _ => match std::fs::remove_file(&meta) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    };
    if let Err(e) = res {
        eprintln!("warning: watch metadata `{}`: {e}", meta.display());
    }
}

/// Where an armed source came from — the provenance that decides
/// who stores its prompt (github-watch-prompts, codex 16ea191).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceOrigin {
    Config,
    Cli,
}

/// One github wake source: the production entry point. Every source
/// in the process shares ONE host-scoped session (one token
/// acquisition, one 401 story — codex f9807c3). `delivery: realtime`
/// adds the webhook relay as the latency path; polling remains the
/// completeness backstop either way.
pub(crate) async fn run_github_source(
    src: GithubSource,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
    events_dir: Option<std::path::PathBuf>,
    origin: SourceOrigin,
) {
    // The per-watch operator prompt is stamped HERE — the single
    // choke point between this source's machinery (live ingest,
    // backlog, takeover tail) and the wait — never at
    // classification, so the WAL stores no prompt and editing the
    // watch retitles already-logged events (github-watch-prompts).
    let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
    let stamper = stamp_instructions_forwarder(irx, tx, src.prompt.clone());

    // Presentation-metadata sidecar, provenance-gated — see
    // [`write_watch_sidecar`].
    if let Some(dir) = &events_dir {
        write_watch_sidecar(dir, &src, origin);
    }

    // The supervisor arm OWNS `itx`: when it returns, the drop
    // closes the channel and the stamper drains out — the join can
    // never hang on a live-but-idle intermediate channel.
    let ((), ()) = tokio::join!(
        async {
            let session = shared_session();
            let fetcher = GhFetcher {
                repo: src.repo.clone(),
                session: session.clone(),
            };
            let ci = GhCi {
                repo: src.repo.clone(),
                prs: src.prs.clone(),
                session: session.clone(),
            };
            let login = GhLogin(session);
            let realtime = src.delivery == clank_core::agent_config::Delivery::Realtime;
            let src2 = src.clone();
            source_supervisor(
                &src,
                FeedSide {
                    fetcher: &fetcher,
                    login: &login,
                },
                &ci,
                &itx,
                events_dir,
                move || async move {
                    if realtime {
                        start_relay(&src2, GhForwarder).await
                    } else {
                        None
                    }
                },
            )
            .await;
            drop(itx);
        },
        stamper
    );
}

/// Forward items, stamping the watch's `prompt` onto every github
/// event that passes. Pure plumbing, unit-tested without transports.
async fn stamp_instructions_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<WaitItem>,
    tx: tokio::sync::mpsc::UnboundedSender<WaitItem>,
    prompt: Option<String>,
) {
    while let Some(mut item) = rx.recv().await {
        if let WaitItem::GithubEvent {
            ref mut instructions,
            ..
        } = item
        {
            instructions.clone_from(&prompt);
        }
        if tx.send(item).is_err() {
            return;
        }
    }
}

/// The lease-gated source runtime (wal-single-ingest-writer): the
/// INGEST LEASE is acquired before ANY transport exists — only the
/// holder starts the relay or polls github; a non-holder TAILS the
/// WAL, presenting logged unhandled entries on the poll cadence, and
/// takes over (starting transports only then) when the holder dies.
/// Presentation never acknowledges: `clank wait` — leased or tailing
/// — writes no ack records, ever.
async fn source_supervisor<F, R, C, RF, RFut>(
    src: &GithubSource,
    feed_side: FeedSide<'_, F, R>,
    reader: &C,
    tx: &tokio::sync::mpsc::UnboundedSender<WaitItem>,
    events_dir: Option<std::path::PathBuf>,
    relay_start: RF,
) where
    F: EventFetcher,
    R: LoginResolver,
    C: CiReader,
    RF: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Option<(std::sync::Arc<RelayInbox>, RelayGuard)>>,
{
    // Ad-hoc unbound waits have no persistence dir: memory-only
    // ingest, lease-free — nothing durable to contend over.
    let Some(dir) = events_dir else {
        let relay = relay_start().await;
        let inbox = relay.as_ref().map(|(i, _)| i.clone());
        poll_loop(
            src,
            FeedSide {
                fetcher: feed_side.fetcher,
                login: feed_side.login,
            },
            ChecksSide {
                reader,
                state: None,
            },
            tx,
            inbox,
            None,
            Default::default(),
        )
        .await;
        return;
    };
    let key = source_key(src);
    // FnOnce discipline across the loop: the transports start at most
    // once, exactly at lease acquisition.
    let mut relay_start = Some(relay_start);
    let mut warned_tail = false;
    // Presented LOGICAL EVENT keys — (seq, ident), never seq alone:
    // damaged logs can hold distinct events at one seq and each must
    // present (codex 20cdd09). Re-arms re-present (at-least-once).
    let mut presented: std::collections::HashSet<(u64, String)> = std::collections::HashSet::new();
    let configured = src
        .poll_interval
        .as_deref()
        .and_then(|s| crate::cli::wait::parse_duration_str(s).ok().flatten());
    loop {
        let lease = match crate::cli::github::log::IngestLease::try_acquire(&dir, &key) {
            Ok(l) => l,
            Err(e) => {
                // A REAL lease failure (unopenable dir, exotic fs) is
                // not contention: ingest memory-only rather than tail
                // a holder that doesn't exist (codex 8bf682e).
                eprintln!(
                    "wait: github {} — ingest lease unavailable ({e}); \
                     polling without persistence",
                    src.repo
                );
                let relay = match relay_start.take() {
                    Some(start) => start().await,
                    None => None,
                };
                let inbox = relay.as_ref().map(|(i, _)| i.clone());
                poll_loop(
                    src,
                    FeedSide {
                        fetcher: feed_side.fetcher,
                        login: feed_side.login,
                    },
                    ChecksSide {
                        reader,
                        state: ci_state(&dir, &key),
                    },
                    tx,
                    inbox,
                    None,
                    Default::default(),
                )
                .await;
                return;
            }
        };
        if let Some(lease) = lease {
            let log = match crate::cli::github::log::EventLog::open(&dir, &key) {
                Ok(l) => Some(l),
                Err(e) => {
                    eprintln!(
                        "wait: github {} — can't open the event log dir ({e}); \
                         continuing without offline catch-up",
                        src.repo
                    );
                    None
                }
            };
            let relay = match relay_start.take() {
                Some(start) => start().await,
                None => None,
            };
            let inbox = relay.as_ref().map(|(i, _)| i.clone());
            poll_loop(
                src,
                FeedSide {
                    fetcher: feed_side.fetcher,
                    login: feed_side.login,
                },
                ChecksSide {
                    reader,
                    state: ci_state(&dir, &key),
                },
                tx,
                inbox,
                log,
                presented,
            )
            .await;
            drop(lease);
            // RelayGuard drops here (and on task abort): listener +
            // supervisor die with the source; the forwarder child by
            // group kill.
            return;
        }
        if !warned_tail {
            eprintln!(
                "wait: github {} — another wait is already watching this source for this \
                 agent; tailing its event log instead of polling",
                src.repo
            );
            warned_tail = true;
        }
        // Tail one cycle: present the leased writer's unhandled
        // entries this process hasn't presented yet. read_rows is the
        // read-only inspector — a tailer never quarantines.
        if let Ok(log) = crate::cli::github::log::EventLog::open(&dir, &key)
            && let Ok(Some(read)) = log.read_rows()
        {
            for row in read.rows {
                if !row.acked
                    && in_scope(src, &row.item)
                    && presented.insert((row.seq, row.ident.clone()))
                    && tx.send(row.item).is_err()
                {
                    return; // wait gone
                }
            }
        }
        tokio::time::sleep(effective_interval(configured, None)).await;
    }
}

/// Where a source's CI horizon persists: beside its WAL, keyed the
/// same way, so a config edit that re-keys the log re-establishes the
/// checks too.
fn ci_state(dir: &std::path::Path, key: &str) -> Option<std::path::PathBuf> {
    Some(dir.join(format!("{key}.ci.json")))
}

/// Stable per-source WAL key: the sanitized repo plus a short hash of
/// the source's full identity. Different kinds/branches/PR scopes/
/// own-action settings classify differently — a different presentable
/// set is a DIFFERENT log (a config edit re-baselines rather than
/// misreading an old cursor). FNV-1a inlined for cross-release
/// stability, like the zellij session hash.
pub(crate) fn source_key(src: &GithubSource) -> String {
    let slug: String = src
        .repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut identity = format!(
        "{}|{:?}|{:?}|{}",
        src.repo, src.events, src.branches, src.include_own_actions
    );
    // Appended only when set, so every watch that predates the PR
    // scope keeps its key byte for byte. Re-keying is not free: the
    // old log's rows stay listed as unhandled with nothing to
    // re-present them, the prompt sidecar orphans, and — while both
    // configs are armed — the ingest lease is a different file, so
    // two loops ingest the same repo into two logs.
    if !src.prs.is_empty() {
        identity.push_str(&format!("|prs{:?}", src.prs));
    }
    let mut h: u32 = 0x811c9dc5;
    for b in identity.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("github-{slug}-{h:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use clank_core::agent_config::GithubEventKind;

    use crate::cli::github::ci::tests::NoCi;
    use crate::cli::github::feed::tests::{ScriptFetcher, classified, ev, gh_item, ok, src};
    use crate::cli::github::feed::{GhResponse, classify_event};
    use crate::cli::github::log::tests::wal_tempdir;
    use crate::cli::github::session::tests::FixedLogin;

    #[tokio::test(start_paused = true)]
    async fn non_holder_tails_without_transports_and_takes_over() {
        let dir = wal_tempdir();
        let key = source_key(&src(&[GithubEventKind::IssueOpened], true));
        // The "holder": a leased writer that logged one unhandled
        // event, standing in for a live wait's ingest.
        let lease = crate::cli::github::log::IngestLease::try_acquire(&dir, &key).unwrap();
        {
            let mut log = crate::cli::github::log::EventLog::open(&dir, &key).unwrap();
            log.append_inbox(
                crate::cli::github::log::Transport::Poll,
                Some("1"),
                None,
                None,
                &classified(7, None).item,
            )
            .unwrap();
            // Duplicate-writer damage: a SECOND distinct event at the
            // SAME seq (codex 20cdd09) — both must tail-present and
            // both must suppress at takeover.
            let path = dir.join(format!("{key}.jsonl"));
            let text = std::fs::read_to_string(&path).unwrap();
            let line = text
                .lines()
                .last()
                .unwrap()
                .replace("\"1\"", "\"2\"")
                .replace("\"number\":7", "\"number\":8");
            std::fs::write(&path, format!("{text}{line}\n")).unwrap();
        }
        // The second wait's supervisor: its relay starter and fetcher
        // must go UNUSED while tailing.
        let relay_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fetched = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct CountFetcher(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl EventFetcher for CountFetcher {
            async fn fetch(&self, _p: u32, _e: Option<String>) -> anyhow::Result<GhResponse> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ok(vec![], None, None))
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let s = src(&[GithubEventKind::IssueOpened], true);
        let d = dir.clone();
        let rs = relay_started.clone();
        let f = CountFetcher(fetched.clone());
        let run = tokio::spawn(async move {
            source_supervisor(
                &s,
                FeedSide {
                    fetcher: &f,
                    login: &FixedLogin("me"),
                },
                &NoCi,
                &tx,
                Some(d),
                move || async move {
                    rs.store(true, std::sync::atomic::Ordering::SeqCst);
                    None
                },
            )
            .await;
        });
        // The tailer presents BOTH logical events at the damaged seq…
        let mut got = Vec::new();
        for _ in 0..2 {
            let woke = tokio::time::timeout(std::time::Duration::from_secs(300), rx.recv())
                .await
                .expect("the tailer must present the leased writer's entries")
                .expect("open");
            if let WaitItem::GithubEvent { number, .. } = &woke {
                got.push(number.unwrap());
            }
        }
        got.sort();
        assert_eq!(got, vec![7, 8], "both distinct same-seq events present");
        // …without starting transports or touching github.
        assert!(!relay_started.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(fetched.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Holder dies → the tailer's next cycle takes the lease and
        // becomes the ingester: transports start, github is polled.
        drop(lease);
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        assert!(
            relay_started.load(std::sync::atomic::Ordering::SeqCst),
            "takeover must start transports after acquiring the lease"
        );
        assert!(
            fetched.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "takeover must begin polling"
        );
        // No ack records were ever written by presentation.
        let text = std::fs::read_to_string(dir.join(format!("{key}.jsonl"))).unwrap_or_default();
        assert!(
            !text.contains("\"t\":\"ack\""),
            "wait must never acknowledge"
        );
        // Exactly-once WITHIN this wait (codex 8bf682e): the takeover's
        // warm start suppresses what the tail phase already presented —
        // the channel holds no second copy of the event.
        let mut extra = 0;
        while let Ok(item) = rx.try_recv() {
            if matches!(
                &item,
                WaitItem::GithubEvent {
                    number: Some(7 | 8),
                    ..
                }
            ) {
                extra += 1;
            }
        }
        assert_eq!(
            extra, 0,
            "takeover re-presented entries the tail already delivered"
        );
        run.abort();
        let _ = run.await;
    }

    /// The FOLLOWER's path: a second wait that cannot take the lease
    /// tails the holder's log and presents its rows directly, with no
    /// classifier between. codex 8043f14 named it alongside the
    /// backlog replay for exactly this reason.
    #[tokio::test(start_paused = true)]
    async fn the_follower_tail_is_filtered_by_the_scope() {
        let dir = wal_tempdir();
        let mut s = src(&[GithubEventKind::PrComment], true);
        s.prs = vec![7];
        let key = source_key(&s);
        // Somebody else holds the lease and has logged both PRs.
        let _lease = crate::cli::github::log::IngestLease::try_acquire(&dir, &key).unwrap();
        {
            let mut log = crate::cli::github::log::EventLog::open(&dir, &key).unwrap();
            for number in [8u64, 7] {
                log.append_inbox(
                    crate::cli::github::log::Transport::Poll,
                    Some(&format!("feed{number}")),
                    Some(&format!("pr_issue_comment#{number}")),
                    None,
                    &gh_item("pr_comment", Some(number)),
                )
                .unwrap();
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let d = dir.clone();
        let s2 = s.clone();
        let handle = tokio::spawn(async move {
            source_supervisor(
                &s2,
                FeedSide {
                    fetcher: &ScriptFetcher::new(vec![]),
                    login: &FixedLogin("me"),
                },
                &NoCi,
                &tx,
                Some(d),
                || async { None },
            )
            .await;
        });
        tokio::time::sleep(Duration::from_secs(120)).await;
        handle.abort();
        let _ = handle.await;

        let mut numbers = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let WaitItem::GithubEvent { number, .. } = item {
                numbers.push(number);
            }
        }
        assert_eq!(
            numbers,
            vec![Some(7)],
            "the tail presents through the scope, not around it",
        );
    }

    #[tokio::test]
    async fn stamper_stamps_github_items_at_presentation_only() {
        // The WAL side: a classified item serializes WITHOUT the
        // instructions field (presentation-only; the log format is
        // untouched by github-watch-prompts)…
        let c = classify_event(
            "o/r",
            &ev(
                "1",
                "IssuesEvent",
                serde_json::json!({"action": "opened", "issue": {"number": 7}}),
                "hubot",
            ),
            &[GithubEventKind::IssueOpened],
            None,
            false,
            &[],
        )
        .expect("classified");
        let stored = serde_json::to_string(&c.item).unwrap();
        assert!(!stored.contains("instructions"), "WAL bytes: {stored}");

        // …and the forwarder is the single stamping choke point.
        let (itx, irx) = tokio::sync::mpsc::unbounded_channel();
        let (otx, mut orx) = tokio::sync::mpsc::unbounded_channel();
        let fwd = tokio::spawn(stamp_instructions_forwarder(
            irx,
            otx,
            Some("triage and reply".into()),
        ));
        itx.send(c.item).unwrap();
        drop(itx);
        let out = orx.recv().await.expect("forwarded");
        let WaitItem::GithubEvent { instructions, .. } = out else {
            panic!("github item");
        };
        assert_eq!(instructions.as_deref(), Some("triage and reply"));
        fwd.await.unwrap();
    }

    #[test]
    fn source_key_ignores_the_prompt() {
        // Editing a watch's prompt must NOT re-key its WAL (the
        // prompt is presentation config; identity is repo/kinds/
        // branches/prs/own-actions).
        let mut a = src(&[GithubEventKind::IssueOpened], false);
        let key = source_key(&a);
        a.prompt = Some("do the thing".into());
        assert_eq!(source_key(&a), key);
    }

    #[test]
    fn a_watch_without_a_pr_scope_keeps_the_key_it_had() {
        // Pinned: this is the key this source had before `prs`
        // existed. Moving it orphans every armed watch's WAL, its
        // cursor and its prompt sidecar at once.
        assert_eq!(
            source_key(&src(&[GithubEventKind::IssueOpened], false)),
            "github-o-r-4586904a",
        );
    }

    #[test]
    fn source_key_separates_a_narrowed_pr_scope() {
        // Narrowing the PR scope changes which events are
        // presentable, so it is a DIFFERENT log — reusing the old
        // cursor would read a baseline taken for another set. This is
        // also the claim the `in_scope` rationale must NOT contradict:
        // an EDITED list re-keys, so the replay filtering exists for
        // the unchanged-config upgrade, not for edits (ruthless
        // 8388d12).
        let mut a = src(&[GithubEventKind::CiFailed], false);
        let all = source_key(&a);
        a.prs = vec![7];
        let one = source_key(&a);
        assert_ne!(one, all);
        a.prs = vec![7, 9];
        assert_ne!(source_key(&a), one);
    }
}
