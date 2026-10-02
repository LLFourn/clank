//! The wait page's on-demand process observation. One running native read and
//! one desired request, overwritten by newer navigation/refresh requests. No
//! queue of historical requests, persistent worker, timer or periodic read.

use super::input::WaitPage;
use crate::proc_identity::ProcToken;
use crate::proc_identity::inspect::{Inspection, Unavailable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) generation: u64,
    pub(super) page: WaitPage,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum State {
    Loading,
    NoPid,
    NoToken,
    Ready(Inspection),
}

#[derive(Debug)]
pub(super) struct Slot {
    pub(super) request: Request,
    pub(super) state: State,
    pub(super) command: Option<std::sync::Arc<super::wait_detail::Command>>,
}

pub(super) struct Completion {
    pub(super) request: Request,
    pub(super) inspection: Inspection,
    pub(super) command: Option<std::sync::Arc<super::wait_detail::Command>>,
}

#[derive(Default)]
pub(super) struct Controller {
    generation: u64,
    /// The current page only. A Loading slot also represents the ONE
    /// pending request while an earlier native read finishes.
    pub(super) slot: Option<Slot>,
    running: Option<Request>,
}

impl Controller {
    /// Only page entry and deliberate refresh call this, never repaint or
    /// snapshot rebuild. Reopening the same page still gets a new generation.
    pub(super) fn request(&mut self, page: WaitPage) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("process request generation exhausted");
        let state = match (page.pid, page.token.as_ref()) {
            (None, _) => State::NoPid,
            (Some(pid), _) if pid <= 0 => {
                State::Ready(Inspection::Unavailable(Unavailable::InvalidPid))
            }
            (_, None) => State::NoToken,
            (_, Some(ProcToken::Unrecognised)) => {
                State::Ready(Inspection::Unavailable(Unavailable::Unsupported))
            }
            _ => State::Loading,
        };
        self.slot = Some(Slot {
            request: Request {
                generation: self.generation,
                page,
            },
            state,
            command: None,
        });
    }

    /// Logical cancellation: native syscalls already in progress may finish,
    /// but no result is retained and no pending inspection starts afterwards.
    pub(super) fn close(&mut self) {
        self.slot = None;
    }

    pub(super) fn take_job(&mut self) -> Option<Request> {
        if self.running.is_some() {
            return None;
        }
        let slot = self.slot.as_ref()?;
        if slot.state != State::Loading {
            return None;
        }
        let request = slot.request.clone();
        self.running = Some(request.clone());
        Some(request)
    }

    /// Only the matching completion releases the running slot. It becomes
    /// display data only if it also answers the still-open desired request.
    pub(super) fn complete(&mut self, completion: Completion) -> bool {
        if self.running.as_ref() != Some(&completion.request) {
            return false;
        }
        self.running = None;
        let Some(slot) = self.slot.as_mut() else {
            return false;
        };
        if slot.request != completion.request {
            return false;
        }
        slot.state = State::Ready(completion.inspection);
        slot.command = completion.command;
        true
    }
}

/// One short-lived thread, started ONLY after take_job reserves the single
/// running slot. Its result wakes the existing TUI channel. Dropping the join
/// handle is intentional: closing the page or terminal never waits for a
/// syscall. The controller refuses stale results and keeps native work bounded.
/// The inspector seam lets tests hold a read open without a real OS process.
pub(super) fn spawn(
    request: Request,
    inspect: impl FnOnce(i32, &ProcToken) -> Inspection + Send + 'static,
    complete: impl FnOnce(Completion) + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("clank-process-details".into())
        .spawn(move || {
            // A failed inspector must still release the running slot; a panic
            // cannot strand the page on Loading or prevent the next request.
            let inspection = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match (request.page.pid, request.page.token.as_ref()) {
                    (Some(pid), Some(token)) if pid > 0 => inspect(pid, token),
                    _ => Inspection::Unavailable(Unavailable::InvalidPid),
                }
            }))
            .unwrap_or(Inspection::Unavailable(Unavailable::Io(
                std::io::ErrorKind::Other,
            )));
            let command = super::wait_detail::Command::from_inspection(&inspection);
            complete(Completion {
                request,
                inspection,
                command,
            });
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn page(pid: i32) -> WaitPage {
        WaitPage {
            label: "reviewer".into(),
            task: "test".into(),
            at: "2026-10-03T00:00:00Z".into(),
            pid: Some(pid),
            token: Some(ProcToken::MacStart {
                sec: 100,
                usec: pid as u64,
            }),
        }
    }
    fn done(request: Request) -> Completion {
        Completion {
            command: None,
            request,
            inspection: Inspection::Ended,
        }
    }

    #[test]
    fn idle_closed_and_completed_pages_never_schedule_more_reads() {
        let mut c = Controller::default();
        assert!(c.take_job().is_none());
        c.request(page(42));
        let job = c.take_job().unwrap();
        for _ in 0..100 {
            assert!(c.take_job().is_none());
        }
        assert!(c.complete(done(job)));
        for _ in 0..100 {
            assert!(c.take_job().is_none());
        }
        c.close();
        assert!(c.slot.is_none());
        assert!(c.take_job().is_none());
    }

    #[test]
    fn rapid_navigation_keeps_only_the_latest_pending_request() {
        let mut c = Controller::default();
        c.request(page(42));
        let old = c.take_job().unwrap();
        for pid in 43..1043 {
            c.request(page(pid));
            assert!(c.take_job().is_none(), "one native read at a time");
        }
        assert!(!c.complete(done(old)), "old result is not display data");
        let latest = c.take_job().unwrap();
        assert_eq!(latest.page.pid, Some(1042));
        assert_eq!(latest.generation, 1001);
        assert!(c.complete(done(latest)));
        assert_eq!(
            c.slot.as_ref().unwrap().state,
            State::Ready(Inspection::Ended)
        );
        assert!(c.take_job().is_none());
    }

    #[test]
    fn closure_cancels_the_pending_read_and_ignores_late_results() {
        let mut c = Controller::default();
        c.request(page(42));
        let old = c.take_job().unwrap();
        c.request(page(43));
        c.close();
        assert!(!c.complete(done(old)));
        assert!(c.slot.is_none());
        assert!(c.take_job().is_none());
    }

    #[test]
    fn reopening_or_refreshing_the_same_page_rejects_its_old_generation() {
        for close in [false, true] {
            let mut c = Controller::default();
            c.request(page(42));
            let old = c.take_job().unwrap();
            if close {
                c.close();
            }
            c.request(page(42));
            assert!(!c.complete(done(old.clone())));
            let fresh = c.take_job().unwrap();
            assert_ne!(fresh.generation, old.generation);
            assert!(
                !c.complete(done(old)),
                "a duplicate cannot release fresh work"
            );
            assert!(c.take_job().is_none());
            assert!(c.complete(done(fresh)));
        }
    }

    #[test]
    fn completions_must_match_generation_and_every_attendance_and_birth_field() {
        let mut c = Controller::default();
        c.request(page(42));
        let job = c.take_job().unwrap();
        for field in 0..6 {
            let mut wrong = job.clone();
            match field {
                0 => wrong.generation += 1,
                1 => wrong.page.label.push('x'),
                2 => wrong.page.task.push('x'),
                3 => wrong.page.at.push('x'),
                4 => wrong.page.pid = Some(43),
                _ => wrong.page.token = Some(ProcToken::MacStart { sec: 100, usec: 43 }),
            }
            assert!(!c.complete(done(wrong)));
            assert!(c.take_job().is_none());
            assert_eq!(c.slot.as_ref().unwrap().state, State::Loading);
        }
        assert!(c.complete(done(job)));
    }

    #[test]
    fn missing_or_unsupported_identities_need_no_thread_or_os_read() {
        let mut c = Controller::default();
        let mut p = page(42);
        p.pid = None;
        c.request(p);
        assert_eq!(c.slot.as_ref().unwrap().state, State::NoPid);
        assert!(c.take_job().is_none());
        let mut p = page(42);
        p.token = None;
        c.request(p);
        assert_eq!(c.slot.as_ref().unwrap().state, State::NoToken);
        assert!(c.take_job().is_none());
        let mut p = page(42);
        p.token = Some(ProcToken::Unrecognised);
        c.request(p);
        assert_eq!(
            c.slot.as_ref().unwrap().state,
            State::Ready(Inspection::Unavailable(Unavailable::Unsupported))
        );
        assert!(c.take_job().is_none());
        c.request(page(0));
        assert_eq!(
            c.slot.as_ref().unwrap().state,
            State::Ready(Inspection::Unavailable(Unavailable::InvalidPid))
        );
        assert!(c.take_job().is_none());
    }

    #[test]
    fn a_blocked_inspector_leaves_navigation_responsive_and_its_answer_is_stale() {
        let mut c = Controller::default();
        c.request(page(42));
        let job = c.take_job().unwrap();
        let (started, starts) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let (send, results) = mpsc::channel();
        spawn(
            job,
            move |pid, _| {
                assert_eq!(pid, 42);
                started.send(()).unwrap();
                gate.recv().unwrap();
                Inspection::Ended
            },
            move |result| {
                send.send(result).unwrap();
            },
        )
        .unwrap();
        starts.recv_timeout(Duration::from_secs(5)).unwrap();
        // The fake native call is blocked, but the foreground can navigate.
        c.close();
        c.request(page(43));
        assert!(c.take_job().is_none());
        release.send(()).unwrap();
        assert!(!c.complete(results.recv_timeout(Duration::from_secs(5)).unwrap()));
        let next = c.take_job().unwrap();
        assert_eq!(next.page.pid, Some(43));
        assert!(c.complete(done(next)));
    }

    #[test]
    fn a_panicking_inspector_still_completes_and_allows_the_next_request() {
        let mut c = Controller::default();
        c.request(page(42));
        let job = c.take_job().unwrap();
        let (send, results) = mpsc::channel();
        spawn(
            job,
            |_, _| panic!("fixture inspection failure"),
            move |result| {
                send.send(result).unwrap();
            },
        )
        .unwrap();
        assert!(c.complete(results.recv_timeout(Duration::from_secs(5)).unwrap()));
        assert_eq!(
            c.slot.as_ref().unwrap().state,
            State::Ready(Inspection::Unavailable(Unavailable::Io(
                std::io::ErrorKind::Other
            )))
        );
        c.request(page(43));
        assert!(c.take_job().is_some());
    }

    #[test]
    fn the_worker_prepares_safe_command_text_and_the_page_reuses_that_allocation() {
        use crate::proc_identity::inspect::{ProcessDetails, ProcessState};
        use std::sync::Arc;
        let mut c = Controller::default();
        c.request(page(42));
        let job = c.take_job().unwrap();
        let (send, results) = mpsc::channel();
        let foreground = std::thread::current().id();
        spawn(
            job,
            move |pid, token| {
                assert_ne!(std::thread::current().id(), foreground);
                Inspection::Verified(ProcessDetails {
                    pid,
                    birth: token.clone(),
                    observed_at: time::OffsetDateTime::UNIX_EPOCH,
                    argv: Ok(vec!["tool".into(), "two  spaces".into(), "\x1b".into()]),
                    state: Ok(ProcessState::Sleeping),
                    parent: Ok(7),
                    group: Ok(8),
                })
            },
            move |result| send.send(result).unwrap(),
        )
        .unwrap();
        let completion = results.recv_timeout(Duration::from_secs(5)).unwrap();
        let prepared = completion.command.as_ref().unwrap().clone();
        assert_eq!(prepared.text, "tool \"two  spaces\" \"\\u{1b}\"");
        assert!(c.complete(completion));
        let cached = c.slot.as_ref().unwrap().command.as_ref().unwrap();
        assert!(Arc::ptr_eq(&prepared, cached));
        assert_eq!(cached.middle(80), prepared.text);
        assert!(
            c.take_job().is_none(),
            "displaying the result schedules no work"
        );
    }
}
