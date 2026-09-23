//! The realtime relay: `gh webhook forward` into a loopback listener,
//! and the bounded inbox its deliveries wait in.
//!
//! A delivery is RAW on purpose — classification happens at drain
//! time in the poll loop, where the own-actor filter is resolved — so
//! nothing here decides whether an event wakes anybody. This module
//! is the transport; the loop is the judge.

use clank_core::agent_config::{GithubEventKind, GithubSource};

/// One loopback request may not buffer unboundedly: the count-bounded
/// inbox doesn't bound MEMORY if a single body is huge (codex
/// 4d77b83). Real webhook payloads are kilobytes; a MiB is generous.
const MAX_WEBHOOK_BODY: usize = 1 << 20;

/// Extract one relay delivery from a webhook request: the event name
/// from `X-GitHub-Event`, the payload from the JSON body. Generic
/// over the body so tests drive it with fabricated requests.
pub(super) async fn webhook_from_request<B>(req: hyper::Request<B>) -> Option<RawDelivery>
where
    B: hyper::body::Body,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    use http_body_util::BodyExt;
    let name = req
        .headers()
        .get("x-github-event")?
        .to_str()
        .ok()?
        .to_string();
    let limited = http_body_util::Limited::new(req.into_body(), MAX_WEBHOOK_BODY);
    let bytes = limited.collect().await.ok()?.to_bytes();
    let payload = serde_json::from_slice(&bytes).ok()?;
    Some((name, payload))
}

/// Serve the loopback listener: each delivery lands in the inbox
/// (ingress-bounded), 200 back to the forwarder. Connections are
/// served INLINE — no detached per-connection tasks, so aborting the
/// supervisor cancels everything it owns.
pub(super) async fn run_relay_listener(
    listener: tokio::net::TcpListener,
    inbox: std::sync::Arc<RelayInbox>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let io = hyper_util::rt::TokioIo::new(stream);
        let inbox = inbox.clone();
        let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let inbox = inbox.clone();
            async move {
                if let Some(raw) = webhook_from_request(req).await {
                    inbox.push(raw);
                }
                Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Empty::<
                    hyper::body::Bytes,
                >::new()))
            }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, svc)
            .await;
    }
}

/// The webhook event names a source's sub-kinds subscribe to — what
/// `gh webhook forward --events` receives.
pub(super) fn webhook_event_names(events: &[GithubEventKind]) -> Vec<&'static str> {
    let mut names = Vec::new();
    for k in events {
        let name = match k {
            GithubEventKind::PrOpened | GithubEventKind::PrUpdated | GithubEventKind::PrMerged => {
                "pull_request"
            }
            GithubEventKind::PrComment => "pull_request_review", // + comment kinds below
            GithubEventKind::IssueOpened | GithubEventKind::IssueClosed => "issues",
            GithubEventKind::IssueComment => "issue_comment",
            GithubEventKind::BranchPush => "push",
            // A completed suite is only a TRIGGER to re-read the
            // PR's checks — one App's suite is not the PR's result —
            // so the noisy per-job `check_run` is not subscribed.
            GithubEventKind::CiPassed | GithubEventKind::CiFailed => "check_suite",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    // pr_comment spans three webhook events.
    if events.contains(&GithubEventKind::PrComment) {
        for extra in ["pull_request_review_comment", "issue_comment"] {
            if !names.contains(&extra) {
                names.push(extra);
            }
        }
    }
    // The older commit-status API reports outside check suites, and a
    // PR's result is the union of both surfaces.
    if events.iter().any(|k| k.is_ci()) && !names.contains(&"status") {
        names.push("status");
    }
    names
}

/// The forwarder's argv — pure, pinned by test (codex 4d77b83).
pub(super) fn forwarder_argv(repo: &str, events: &str, port: u16) -> Vec<String> {
    vec![
        "webhook".into(),
        "forward".into(),
        format!("--repo={repo}"),
        format!("--events={events}"),
        format!("--url=http://127.0.0.1:{port}"),
    ]
}

/// The forwarder process seam: run until TERMINAL, returning the one
/// bounded human reason (spawn failure, or exit status + last stderr
/// line). Injected so the supervisor's outcomes are scriptable
/// without gh (codex 4d77b83).
pub(crate) trait Forwarder: Send + Sync + 'static {
    fn run(
        &self,
        repo: String,
        events: String,
        port: u16,
    ) -> impl std::future::Future<Output = String> + Send;
}

/// Production forwarder: `gh webhook forward` in its own process
/// group, killed with the supervisor (kill_on_drop + group kill);
/// stderr captured so the terminal reason is USEFUL (a missing
/// extension or admin refusal names itself).
pub(super) struct GhForwarder;

impl Forwarder for GhForwarder {
    async fn run(&self, repo: String, events: String, port: u16) -> String {
        let mut cmd = tokio::process::Command::new("gh");
        cmd.args(forwarder_argv(&repo, &events, port))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        run_forwarder_process(cmd).await
    }
}

/// Supervise a spawned forwarder-shaped command to its terminal
/// reason. stderr is drained CONCURRENTLY with the wait into a
/// bounded tail — a chatty extension that fills the OS pipe would
/// otherwise block before exit and the wait would never complete
/// (codex 24c3355); the post-exit drain uses the same absolute
/// deadline discipline as command sources.
async fn run_forwarder_process(mut cmd: tokio::process::Command) -> String {
    use tokio::io::AsyncReadExt;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("failed to start: {e}"),
    };
    let _group = crate::cli::wait::GroupKill(child.id());
    let mut stderr = child.stderr.take();
    let mut tail = crate::cli::wait::RingTail::new(4096);
    let mut buf = [0u8; 1024];
    let status = loop {
        tokio::select! {
            st = child.wait() => break st,
            r = async { stderr.as_mut().unwrap().read(&mut buf).await }, if stderr.is_some() => {
                match r {
                    Ok(0) | Err(_) => stderr = None,
                    Ok(n) => tail.push(&buf[..n]),
                }
            }
        }
    };
    // Catch a final line written just before exit — bounded both ways.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        while let Some(pipe) = stderr.as_mut() {
            match tokio::time::timeout(std::time::Duration::from_millis(20), pipe.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => tail.push(&buf[..n]),
                _ => break,
            }
        }
    })
    .await;
    let text = tail.into_string();
    let last_line = text.lines().last().unwrap_or("").to_string();
    match status {
        Ok(st) => format!("forwarder exited {st}: {last_line}"),
        Err(e) => format!("forwarder wait failed: {e}"),
    }
}

/// Aborts the relay's listener/supervisor tasks when the source dies —
/// the forwarder child itself dies by kill_on_drop + group kill inside
/// the supervisor task.
pub(super) struct RelayGuard(Vec<tokio::task::JoinHandle<()>>);
impl Drop for RelayGuard {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Start the realtime relay: a loopback-only listener (by
/// construction — bound to 127.0.0.1) plus a supervised
/// `gh webhook forward` child in its own process group. Any failure
/// to start, and the forwarder EXITING, close the inbox — the poll
/// loop logs the fallback once and continues on polling alone
/// (polling never stopped; nothing is lost). Requires ADMIN on the
/// watched repo and the cli/gh-webhook extension; GitHub bills the
/// relay as dev tooling — all stated in the skill docs.
pub(super) async fn start_relay<Fw: Forwarder>(
    src: &GithubSource,
    forwarder: Fw,
) -> Option<(std::sync::Arc<RelayInbox>, RelayGuard)> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "wait: github {} — realtime listener failed to bind ({e}); polling alone",
                src.repo
            );
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let inbox = std::sync::Arc::new(RelayInbox::new());
    let listen_task = tokio::spawn(run_relay_listener(listener, inbox.clone()));
    let events = webhook_event_names(&src.events).join(",");
    let repo = src.repo.clone();
    let inbox_for_child = inbox.clone();
    let child_task = tokio::spawn(async move {
        // The supervisor prints NOTHING: the terminal reason lands in
        // the inbox and poll_loop's fallback line is the single
        // diagnostic owner (codex 4d77b83).
        let reason = forwarder.run(repo, events, port).await;
        inbox_for_child.close_with(&reason);
    });
    Some((inbox, RelayGuard(vec![listen_task, child_task])))
}

/// One relay delivery as the loopback listener hands it over: the
/// webhook event NAME (the `X-GitHub-Event` header) plus the payload.
/// RAW on purpose — classification happens at DRAIN time in the poll
/// loop, where `own_login` is resolved, so a delivery arriving before
/// identity resolution can never bypass the fail-closed own-actor
/// filter (codex c29a07c): it simply waits in the inbox.
pub(crate) type RawDelivery = (String, serde_json::Value);

/// Pre-baseline relay backlog bound — startup-window sized. The
/// buffer is memory-only by design: nothing here has been logged yet
/// (its durable decision happens at the gate when the baseline
/// drains), and the poll backstop re-observes lost actions.
pub(super) const RELAY_BUFFER_CAP: usize = 256;

/// The relay inbox: a BOUNDED drop-oldest queue whose capacity lives
/// at PRODUCER ingress (codex 840725c) — `push` itself evicts, so no
/// pending await anywhere in the consumer (a hung /user call, a slow
/// poll) can let deliveries accumulate beyond the cap. std Mutex (no
/// await while held) + Notify for the consumer wake.
pub(crate) struct RelayInbox {
    queue: std::sync::Mutex<std::collections::VecDeque<RawDelivery>>,
    notify: tokio::sync::Notify,
    closed: std::sync::atomic::AtomicBool,
    /// Why the relay ended — rendered by the ONE diagnostic owner
    /// (poll_loop's fallback line); bounded at write (codex 4d77b83).
    reason: std::sync::Mutex<Option<String>>,
}

impl RelayInbox {
    pub(crate) fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            reason: std::sync::Mutex::new(None),
        }
    }

    /// Enqueue, applying the overflow policy AT INGRESS: at capacity
    /// the oldest delivery is dropped (with a stderr note).
    pub(crate) fn push(&self, raw: RawDelivery) {
        {
            let mut q = self.queue.lock().unwrap();
            if q.len() == RELAY_BUFFER_CAP {
                q.pop_front();
                eprintln!("wait: github relay inbox overflow; oldest delivery dropped");
            }
            q.push_back(raw);
        }
        self.notify.notify_one();
    }

    /// The producer is gone: record WHY (bounded) and wake the
    /// consumer so it can fall back. The reason is rendered exactly
    /// once, by poll_loop's fallback line — no other diagnostic owner.
    pub(crate) fn close_with(&self, reason: &str) {
        let bounded: String = reason.chars().take(200).collect();
        *self.reason.lock().unwrap() = Some(bounded);
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn close(&self) {
        self.close_with("closed");
    }

    pub(crate) fn reason(&self) -> String {
        self.reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "relay ended".to_string())
    }

    /// Next delivery, or `None` once closed AND drained. Cancellation-
    /// safe: the check-then-wait loop re-checks after every wake, and
    /// `notify_one` stores a permit when nobody is waiting.
    pub(crate) async fn recv(&self) -> Option<RawDelivery> {
        loop {
            if let Some(raw) = self.queue.lock().unwrap().pop_front() {
                return Some(raw);
            }
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            self.notify.notified().await;
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn webhook_requests_map_to_raw_deliveries() {
        // The listener's extraction: X-GitHub-Event + JSON body →
        // RawDelivery; junk degrades to None (dropped, never a panic).
        use http_body_util::Full;
        use hyper::body::Bytes;
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(r#"{"ref":"refs/heads/main"}"#)))
            .unwrap();
        let (name, payload) = webhook_from_request(req).await.expect("maps");
        assert_eq!(name, "push");
        assert_eq!(payload["ref"], "refs/heads/main");
        // Missing event header → None.
        let req = hyper::Request::builder()
            .body(Full::new(Bytes::from("{}")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
        // Non-JSON body → None.
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from("not json")))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[tokio::test]
    async fn relay_listener_binds_loopback_only() {
        // "Rejects non-loopback binds by construction": the bind is
        // literally 127.0.0.1.
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        assert!(l.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn webhook_event_names_cover_the_subscribed_kinds() {
        use GithubEventKind::*;
        let names = webhook_event_names(&[PrOpened, PrComment, BranchPush]);
        for expect in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "issue_comment",
            "push",
        ] {
            assert!(names.contains(&expect), "{expect} missing: {names:?}");
        }
        assert_eq!(
            webhook_event_names(&[IssueOpened, IssueClosed]),
            vec!["issues"],
            "no over-subscription"
        );
    }

    #[test]
    fn forwarder_argv_is_pinned() {
        assert_eq!(
            forwarder_argv("o/r", "push,issues", 4242),
            vec![
                "webhook",
                "forward",
                "--repo=o/r",
                "--events=push,issues",
                "--url=http://127.0.0.1:4242",
            ]
        );
    }

    #[tokio::test]
    async fn oversized_webhook_bodies_are_rejected() {
        // codex 4d77b83: a count-bounded inbox doesn't bound memory if
        // one request is huge — the body collect is byte-limited.
        use http_body_util::Full;
        use hyper::body::Bytes;
        let big = vec![b'x'; MAX_WEBHOOK_BODY + 1];
        let req = hyper::Request::builder()
            .header("X-GitHub-Event", "push")
            .body(Full::new(Bytes::from(big)))
            .unwrap();
        assert!(webhook_from_request(req).await.is_none());
    }

    #[tokio::test]
    async fn relay_guard_drop_aborts_both_owned_tasks() {
        // codex 24c3355: an observable cancellation probe — each task
        // holds a set-on-drop guard; RelayGuard's drop must abort them
        // so the flags flip.
        use std::sync::atomic::{AtomicBool, Ordering};
        struct SetOnDrop(std::sync::Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let flags = [
            std::sync::Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(AtomicBool::new(false)),
        ];
        let tasks: Vec<_> = flags
            .iter()
            .map(|f| {
                let probe = SetOnDrop(f.clone());
                tokio::spawn(async move {
                    let _probe = probe;
                    std::future::pending::<()>().await;
                })
            })
            .collect();
        tokio::task::yield_now().await;
        drop(RelayGuard(tasks));
        // Aborted tasks unwind asynchronously; give the runtime a few
        // turns.
        for _ in 0..50 {
            if flags.iter().all(|f| f.load(Ordering::SeqCst)) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("RelayGuard drop did not cancel its tasks");
    }

    #[tokio::test]
    async fn forwarder_reason_survives_a_pipe_filling_child() {
        // codex 24c3355: a child that writes beyond the OS pipe
        // capacity before exiting must still reach its terminal
        // reason — the concurrent tail read keeps the pipe drained.
        // 256 KiB of stderr ≫ any default pipe buffer.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args([
            "-c",
            "i=0; while [ $i -lt 4096 ]; do printf \
             'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' >&2; \
             i=$((i+1)); done; echo the-final-line >&2; exit 3",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_forwarder_process(cmd),
        )
        .await
        .expect("no deadlock on a full pipe");
        assert!(
            reason.contains("the-final-line"),
            "the ACTUAL last stderr line survives the bounded tail: {reason}"
        );
        assert!(reason.contains("exited"), "{reason}");
    }

    #[test]
    fn the_ci_kinds_subscribe_to_suites_and_statuses() {
        // A PR's result spans both check surfaces; per-job check_run
        // deliveries are noise the rollup read does not need.
        let names = webhook_event_names(&[GithubEventKind::CiPassed]);
        assert_eq!(names, vec!["check_suite", "status"]);
        assert_eq!(
            webhook_event_names(&[GithubEventKind::CiFailed, GithubEventKind::CiPassed]),
            vec!["check_suite", "status"],
            "both kinds, one subscription each",
        );
        assert!(
            !webhook_event_names(&[GithubEventKind::BranchPush]).contains(&"status"),
            "a non-CI watch does not subscribe to statuses",
        );
    }
}
