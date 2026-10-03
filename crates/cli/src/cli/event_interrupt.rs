//! One recipient's eligible inbox batch, independent of waiter admission.
//! Call only on explicit ingestion/recovery/invalidation, never an idle beat.
//! Provider handoff must be ready before a caller admits work here.

use super::github::log::{EventLog, InterruptOutcome};
use std::{future::Future, path::PathBuf, time::Duration};

#[derive(Debug)]
struct Claim {
    source: String,
    seq: u64,
    ident: String,
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) claimed: usize,
    pub(crate) outcome: Option<InterruptOutcome>,
    /// Persistence errors never authorize a side effect or change delivery.
    pub(crate) errors: Vec<String>,
}

/// Claim every eligible logical delivery, then request one cancellation for the
/// recipient. Shared monitor and direct-sender contenders use the same ledger.
/// `perform` must address this recipient's verified live harness and arrange its
/// cancellation-to-wake handoff. No payload is submitted here: custody stays in
/// the inbox, not in a second terminal prompt containing the user's instruction.
///
/// The driver owns all spawned I/O and must cancel it when dropped. A deadline
/// means unknown acceptance, never failure or permission to replay. Claims survive
/// caller cancellation/panic/process death; only a new event grants another try.
pub(crate) async fn dispatch<F>(
    dir: PathBuf,
    within: Duration,
    perform: impl FnOnce() -> F,
) -> anyhow::Result<Report>
where
    F: Future<Output = InterruptOutcome>,
{
    let claim_dir = dir.clone();
    let (claims, mut errors) = blocking(|| claim_pending(claim_dir))?;
    if claims.is_empty() {
        return Ok(Report {
            errors,
            ..Report::default()
        });
    }
    let claimed = claims.len();
    let outcome = match tokio::time::timeout(within, perform()).await {
        Ok(outcome) => outcome,
        Err(_) => InterruptOutcome::Indeterminate,
    };
    let recorded = outcome.clone();
    // No terminal/network I/O runs under the WAL lock. Even recording the result
    // runs off the owner loop; a slow disk cannot stall unrelated recipients.
    errors.extend(blocking(|| finish(dir, claims, recorded)));
    Ok(Report {
        claimed,
        outcome: Some(outcome),
        errors,
    })
}

/// Scoped blocking work cannot outlive its owning dispatch future. Unlike a
/// detached spawn_blocking job, cancellation/shutdown observes it finishing.
/// Multi-thread runtimes lend this worker to the blocking pool for the scope;
/// small current-thread test/CLI runtimes execute the scope directly.
pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

/// One explicit recipient request, including sender fallback without a monitor.
/// The driver contains no payload; missing transport preserves normal delivery.
pub(crate) async fn run(repo: PathBuf, label: clank_core::ids::AgentLabel) {
    let dir = crate::agent_store::agents_root(&repo)
        .join(label.as_str())
        .join("events");
    let result = dispatch(dir, Duration::from_secs(12), || async {
        if crate::agent_store::resolve_role(&repo, &label).is_err() {
            return InterruptOutcome::Unavailable;
        }
        match super::codex_interrupt::try_wake(&repo, &label).await {
            Ok(true) => return InterruptOutcome::Submitted,
            Ok(false) => {}, // No native mutation: verified terminal fallback is safe.
            Err(error) => {
                eprintln!("interrupt `{label}`: native acceptance unconfirmed ({error:#}); normal delivery remains pending");
                return InterruptOutcome::Indeterminate;
            }
        }
        let (target, tool) = match super::pane_target::discover(repo.clone(), label.clone()).await {
            Ok(target) => target,
            Err(error) => {
                eprintln!("interrupt `{label}`: unavailable ({error:#}); normal delivery remains pending");
                return InterruptOutcome::Unavailable;
            }
        };
        match super::pane_input::wake_for_events_async(&target.session, &target.pane, tool).await {
            Ok(()) => InterruptOutcome::Submitted,
            Err(error) => {
                eprintln!("interrupt `{label}`: acceptance unconfirmed ({error:#}); normal delivery remains pending");
                InterruptOutcome::Indeterminate
            }
        }
    }).await;
    match result {
        Ok(report) => {
            if report.claimed > 0 {
                eprintln!(
                    "interrupt `{label}`: {:?} for {} event(s)",
                    report.outcome, report.claimed
                );
            }
            for error in report.errors {
                eprintln!("interrupt `{label}`: {error}; custody preserved");
            }
        }
        Err(error) => eprintln!("interrupt `{label}`: {error:#}; custody preserved"),
    }
}

/// One owned task per recipient, plus a coalesced invalidation during its work.
/// Completed tasks are explicitly collected; retirement takes their handles.
#[derive(Default)]
pub(crate) struct Scheduler {
    tasks: futures_util::stream::FuturesUnordered<Job>,
    active: std::collections::BTreeSet<clank_core::ids::AgentLabel>,
    dirty: std::collections::BTreeSet<clank_core::ids::AgentLabel>,
}
struct Job {
    label: clank_core::ids::AgentLabel,
    task: tokio::task::JoinHandle<()>,
}
impl Future for Job {
    type Output = clank_core::ids::AgentLabel;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match std::pin::Pin::new(&mut self.task).poll(cx) {
            std::task::Poll::Ready(_) => std::task::Poll::Ready(self.label.clone()),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
impl Scheduler {
    pub(crate) fn notify(&mut self, repo: &std::path::Path, label: clank_core::ids::AgentLabel) {
        let repo = repo.to_owned();
        self.notify_with(label.clone(), || tokio::spawn(run(repo, label)));
    }
    fn notify_with(
        &mut self,
        label: clank_core::ids::AgentLabel,
        spawn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) {
        if !self.active.insert(label.clone()) {
            self.dirty.insert(label);
            return;
        }
        self.tasks.push(Job {
            label,
            task: spawn(),
        });
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
    pub(crate) async fn next(&mut self) -> Option<clank_core::ids::AgentLabel> {
        use futures_util::StreamExt;
        self.tasks.next().await
    }
    pub(crate) fn completed(&mut self, repo: &std::path::Path, label: clank_core::ids::AgentLabel) {
        self.active.remove(&label);
        if self.dirty.remove(&label) {
            self.notify(repo, label);
        }
    }
    pub(crate) fn take_handles(self) -> Vec<tokio::task::JoinHandle<()>> {
        self.tasks
            .into_iter()
            .map(|job| {
                job.task.abort();
                job.task
            })
            .collect()
    }
}

fn claim_pending(dir: PathBuf) -> anyhow::Result<(Vec<Claim>, Vec<String>)> {
    let candidates = super::events::read_all(&dir)?;
    let mut claims = Vec::new();
    let mut errors = Vec::new();
    for candidate in candidates {
        let row = candidate.row;
        if !row.interrupt || row.acked || row.interrupt_attempt.is_some() {
            continue;
        }
        // Re-read under the exclusive transaction: inspector snapshots do not
        // grant authority, including when an acknowledgement won the race.
        let result = EventLog::open(&dir, &candidate.source)
            .and_then(|mut log| log.claim_interrupt(row.seq, &row.ident));
        match result {
            Ok(true) => claims.push(Claim {
                source: candidate.source,
                seq: row.seq,
                ident: row.ident,
            }),
            Ok(false) => {}
            Err(error) => errors.push(format!("{}@{}: {error}", candidate.source, row.seq)),
        }
    }
    Ok((claims, errors))
}

fn finish(dir: PathBuf, claims: Vec<Claim>, outcome: InterruptOutcome) -> Vec<String> {
    let mut errors = Vec::new();
    for claim in claims {
        if let Err(error) = EventLog::open(&dir, &claim.source)
            .and_then(|mut log| log.finish_interrupt(claim.seq, &claim.ident, outcome.clone()))
        {
            errors.push(format!("{}@{}: {error}", claim.source, claim.seq));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::super::github::log::Transport;
    use super::*;
    use clank_core::wait::WaitItem;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn add(dir: &std::path::Path, source: &str, id: &str, interrupt: bool) -> u64 {
        let mut log = EventLog::open(dir, source).unwrap();
        log.load();
        log.append_interruptible_inbox(
            Transport::Message,
            None,
            Some(id),
            None,
            &WaitItem::Message {
                handle: id.into(),
                from: None,
                repo: None,
                subject: "work".into(),
                body: "work".into(),
                selector: None,
            },
            interrupt,
        )
        .unwrap()
    }
    fn rows(dir: &std::path::Path, source: &str) -> Vec<super::super::github::log::EventRow> {
        EventLog::open(dir, source)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows
    }

    #[tokio::test]
    async fn one_batch_across_sources_preserves_custody_and_does_not_replay() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "messages", "one", true);
        add(dir.path(), "command-a", "two", true);
        add(dir.path(), "messages", "ordinary", false);
        let calls = AtomicUsize::new(0);
        let report = dispatch(dir.path().into(), Duration::from_secs(1), || async {
            calls.fetch_add(1, Ordering::SeqCst);
            // Another writer can acquire the same lock inside transport I/O.
            // This would deadlock if the dispatch retained an exclusive claim lock.
            let first = rows(dir.path(), "messages").remove(0);
            EventLog::open(dir.path(), "messages")
                .unwrap()
                .append_ack_ident(first.seq, &first.ident)
                .unwrap();
            InterruptOutcome::Submitted
        })
        .await
        .unwrap();
        assert_eq!(report.claimed, 2);
        assert!(report.errors.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let command = rows(dir.path(), "command-a").remove(0);
        assert!(!command.acked);
        assert_eq!(
            command.interrupt_attempt.unwrap().outcome,
            Some(InterruptOutcome::Submitted)
        );
        assert!(rows(dir.path(), "messages")[1].interrupt_attempt.is_none());
        let again = dispatch(dir.path().into(), Duration::from_secs(1), || async {
            panic!("no eligible event means no target discovery or transport");
        })
        .await
        .unwrap();
        assert_eq!(again.claimed, 0);
    }

    #[tokio::test]
    async fn unavailable_and_failed_attempts_leave_work_pending_without_retry() {
        for outcome in [InterruptOutcome::Unavailable, InterruptOutcome::Failed] {
            let dir = tempfile::tempdir().unwrap();
            add(dir.path(), "messages", "one", true);
            let report = dispatch(dir.path().into(), Duration::from_secs(1), || async {
                outcome.clone()
            })
            .await
            .unwrap();
            assert_eq!(report.outcome, Some(outcome.clone()));
            let row = rows(dir.path(), "messages").remove(0);
            assert!(!row.acked);
            assert_eq!(row.interrupt_attempt.unwrap().outcome, Some(outcome));
            assert_eq!(
                dispatch(dir.path().into(), Duration::from_secs(1), || async {
                    panic!("transport failures do not retry on replay");
                })
                .await
                .unwrap()
                .claimed,
                0
            );
        }
    }

    #[tokio::test]
    async fn contenders_do_not_duplicate_an_attempt_for_the_same_event() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "messages", "one", true);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let dir = dir.path().to_owned();
            let calls = calls.clone();
            tasks.spawn(dispatch(dir, Duration::from_secs(1), move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                InterruptOutcome::Submitted
            }));
        }
        let mut claimed = 0;
        while let Some(report) = tasks.join_next().await {
            claimed += report.unwrap().unwrap().claimed;
        }
        assert_eq!(claimed, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timeout_records_unknown_acceptance_and_drops_owned_io() {
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "messages", "one", true);
        let dropped = Arc::new(AtomicUsize::new(0));
        let report = dispatch(dir.path().into(), Duration::from_millis(20), || async {
            let _owned = Dropped(dropped.clone());
            std::future::pending().await
        })
        .await
        .unwrap();
        assert_eq!(report.outcome, Some(InterruptOutcome::Indeterminate));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(!rows(dir.path(), "messages")[0].acked);
    }

    #[tokio::test]
    async fn caller_cancellation_keeps_an_unconfirmed_non_retryable_claim() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "messages", "one", true);
        let (entered, ready) = tokio::sync::oneshot::channel();
        let running = tokio::spawn(dispatch(
            dir.path().into(),
            Duration::from_secs(1),
            || async move {
                entered.send(()).unwrap();
                std::future::pending().await
            },
        ));
        ready.await.unwrap();
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        let row = rows(dir.path(), "messages").remove(0);
        assert!(!row.acked);
        assert_eq!(row.interrupt_attempt.unwrap().outcome, None);
        let again = dispatch(dir.path().into(), Duration::from_secs(1), || async {
            panic!("cancelled caller does not grant another transport attempt");
        })
        .await
        .unwrap();
        assert_eq!(again.claimed, 0);
    }

    #[tokio::test]
    async fn a_corrupt_source_does_not_block_healthy_custody_or_get_repaired() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "messages", "one", true);
        let corrupt = dir.path().join("command-broken.jsonl");
        std::fs::write(&corrupt, "bad interior line\n").unwrap();
        let report = dispatch(dir.path().into(), Duration::from_secs(1), || async {
            InterruptOutcome::Unavailable
        })
        .await
        .unwrap();
        assert_eq!(report.claimed, 1);
        assert_eq!(
            std::fs::read_to_string(corrupt).unwrap(),
            "bad interior line\n"
        );
        assert!(!rows(dir.path(), "messages")[0].acked);
    }

    #[tokio::test]
    async fn recipient_invalidations_coalesce_and_retirement_owns_the_task() {
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let label = clank_core::ids::AgentLabel::parse("grok").unwrap();
        let mut scheduler = Scheduler::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let counter = drops.clone();
        scheduler.notify_with(label.clone(), || {
            tokio::spawn(async move {
                let _owned = Dropped(counter);
                entered.send(()).unwrap();
                std::future::pending::<()>().await;
            })
        });
        ready.await.unwrap();
        for _ in 0..100 {
            scheduler.notify_with(label.clone(), || {
                panic!("active recipient must not spawn another task")
            });
        }
        assert_eq!(scheduler.tasks.len(), 1);
        assert_eq!(scheduler.dirty.len(), 1);
        let handles = scheduler.take_handles();
        assert_eq!(handles.len(), 1);
        for handle in handles {
            assert!(handle.await.unwrap_err().is_cancelled());
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_completed_recipient_gets_one_coalesced_followup_without_an_idle_task() {
        let repo = tempfile::tempdir().unwrap();
        let label = clank_core::ids::AgentLabel::parse("grok").unwrap();
        let mut scheduler = Scheduler::default();
        scheduler.notify_with(label.clone(), || tokio::spawn(async {}));
        scheduler.notify_with(label.clone(), || panic!("must coalesce"));
        let finished = scheduler.next().await.unwrap();
        scheduler.completed(repo.path(), finished);
        assert_eq!(scheduler.tasks.len(), 1);
        assert!(scheduler.dirty.is_empty());
        let finished = scheduler.next().await.unwrap();
        scheduler.completed(repo.path(), finished);
        assert!(scheduler.is_empty());
        assert!(scheduler.active.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_task_cannot_detach_its_blocking_scope() {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let ended = Arc::new(AtomicUsize::new(0));
        let observed = ended.clone();
        let task = tokio::spawn(async move {
            blocking(|| {
                entered.send(()).unwrap();
                held.recv().unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
            });
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        task.abort();
        assert!(
            !task.is_finished(),
            "owning task cannot finish while its scope still owns resources"
        );
        release.send(()).unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn already_handled_events_never_invoke_transport() {
        let dir = tempfile::tempdir().unwrap();
        let seq = add(dir.path(), "messages", "one", true);
        EventLog::open(dir.path(), "messages")
            .unwrap()
            .append_ack(seq)
            .unwrap();
        assert_eq!(
            dispatch(dir.path().into(), Duration::from_secs(1), || async {
                panic!("handled event must not interrupt");
            })
            .await
            .unwrap()
            .claimed,
            0
        );
    }
}
