//! Enforcement gate (no-polls-but-github, I10): every production timed
//! wait in the workspace is a [`Site`] in [`SITES`], with its class and
//! why, and none but GitHub's is a poll unless I10 says so.
//!
//! A site is (file, function, form, ordinal): the ordinal counts the
//! function's timed waits in source order, every form together. Adding a
//! timed wait to any function, a listed one included, removing one, or
//! putting one of another form ahead of it changes the inventory and
//! fails [`every_timed_wait_is_classified`], which is what forces a
//! reclassification.
//!
//! Limits:
//! - It is syntactic. A form is a call whose name is in [`FORMS`], or
//!   the name of a [`Class::Helper`] site's function, with at least one
//!   argument: no listed form takes none, and a zero-argument
//!   `timeout()` is a getter. A timed wait reached through a name it
//!   does not list (an imported bare `poll`, a function pointer, a
//!   trait object's method under another name) is not seen.
//! - Two sites of the same form swapped within one function, or a site
//!   moved inside its function, leave the tuple unchanged.
//! - It does not prove what a loop does. A class is a reviewed claim
//!   about what ends a wait and what re-arms it; the gate only makes
//!   every change to the set of sites ask for that review again.
//! - Production means `src/` of each crate, less `#[cfg(test)]` and
//!   `#[test]` items, files only test code includes, and items behind a
//!   `test-*` feature, which only the test build enables.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::visit::Visit;

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};

/// The timed-wait primitives, by the name a call spells. A qualified
/// entry matches a path ending in those segments.
const FORMS: &[&str] = &[
    "sleep",
    "sleep_until",
    "interval",
    "interval_at",
    "timeout",
    "timeout_at",
    "recv_timeout",
    "recv_deadline",
    "wait_timeout",
    "wait_timeout_while",
    "park_timeout",
    "set_read_timeout",
    "set_write_timeout",
    "event::poll",
];

const CRATES: &[&str] = &["cli", "core", "user-config", "web", "web-cloudflare"];

/// What ends a timed wait, and what re-arms it (no-polls-but-github).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// For a time the user or the domain chose.
    Scheduled,
    /// Bounds one wait for an event; its expiry does not re-arm it.
    OneShot,
    /// Retries a failed operation a finite number of times, or within a
    /// finite total duration, then fails or reports.
    BoundedRetry,
    /// Delays work an event already triggered; never wakes without one.
    Throttle,
    /// Wakes again while the observed state may be unchanged, with no
    /// bound. Allowed only for GitHub, or as a residual I10 names, with
    /// the queued plan that removes it.
    Poll(Residual),
    /// Wraps a timed wait for its callers, whose calls are sites of
    /// their own and carry the class.
    Helper,
    /// Spelled like a form but waits for nothing in production.
    NotAWait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Residual {
    Github,
    /// A poll still owed a replacement, by the plan that owes it.
    Plan(&'static str),
}

use Class::*;

const FAILURES: Residual = Residual::Plan("failed-work-waits-for-a-cause");
const RECOVERY: Residual = Residual::Plan("recovery-looks-when-something-moves");

struct Site {
    /// Relative to `crates/`.
    file: &'static str,
    /// The enclosing function's path within the file: modules, the
    /// impl's or trait's type, the function, `::`-separated.
    function: &'static str,
    form: &'static str,
    ordinal: usize,
    class: Class,
    why: &'static str,
}

const fn site(
    file: &'static str,
    function: &'static str,
    form: &'static str,
    ordinal: usize,
    class: Class,
    why: &'static str,
) -> Site {
    Site {
        file,
        function,
        form,
        ordinal,
        class,
        why,
    }
}

#[rustfmt::skip]
const SITES: &[Site] = &[
    // cli/src/cli
    site("cli/src/cli/agent_state.rs", "View::changes_at", "passes", 0, NotAWait, "a local closure that computes an instant"),
    site("cli/src/cli/agent_state.rs", "View::changes_at", "passes", 1, NotAWait, "a local closure that computes an instant"),
    site("cli/src/cli/agent_state.rs", "View::changes_at", "passes", 2, NotAWait, "a local closure that computes an instant"),
    site("cli/src/cli/agent_state.rs", "View::changes_at", "passes", 3, NotAWait, "a local closure that computes an instant"),
    site("cli/src/cli/claude_trust.rs", "NativeLock::acquire", "sleep", 0, BoundedRetry, "six fixed pauses for Claude's config lock, then it fails"),
    site("cli/src/cli/codex_chat.rs", "connect", "set_read_timeout", 0, OneShot, "bounds each read of one Codex control exchange"),
    site("cli/src/cli/codex_chat.rs", "connect", "set_write_timeout", 1, OneShot, "bounds each write of one Codex control exchange"),
    site("cli/src/cli/codex_chat.rs", "send", "sleep", 0, BoundedRetry, "looks for Codex to release its interrupted turn for 3s, then fails"),
    site("cli/src/cli/codex_interrupt.rs", "start_if_failed", "timeout", 0, OneShot, "bounds connecting to Codex for one recovery start"),
    site("cli/src/cli/codex_interrupt.rs", "start_after", "timeout", 0, OneShot, "bounds the preflight read of one recovery start"),
    site("cli/src/cli/codex_interrupt.rs", "start_after", "timeout", 1, OneShot, "bounds Codex's answer to one turn/start"),
    site("cli/src/cli/command_source.rs", "Os::until_a_member_exits", "pause", 0, Helper, "where no exit can be watched, the drain deadline is waited out"),
    site("cli/src/cli/command_source.rs", "Native::pause", "sleep", 0, Helper, "the real clock behind `Os::pause`"),
    site("cli/src/cli/command_source.rs", "Native::until_a_member_exits", "sleep", 0, Helper, "no exit watch could be made, so the deadline is waited out"),
    site("cli/src/cli/command_source.rs", "Native::until_a_member_exits", "recv_timeout", 1, Helper, "waits for a member's exit, bounded by the caller's deadline"),
    site("cli/src/cli/command_source.rs", "await_work", "until_a_member_exits", 0, OneShot, "each look waits for an exit; the drain deadline ends the loop"),
    site("cli/src/cli/command_source.rs", "await_work", "pause", 1, OneShot, "with no group to watch, sleeps once to the drain deadline"),
    site("cli/src/cli/command_source.rs", "retire_after", "pause", 0, BoundedRetry, "settles a transition in at most SETTLE_LOOKS steps"),
    site("cli/src/cli/command_source.rs", "retire_after", "pause", 1, OneShot, "the grace between SIGTERM and SIGKILL, once"),
    site("cli/src/cli/event_interrupt.rs", "dispatch", "timeout", 0, OneShot, "bounds one interrupt's transport"),
    site("cli/src/cli/github/coordinator.rs", "poll_loop", "sleep_until", 0, Poll(Residual::Github), "GitHub's feed and checks, at its cadence and X-Poll-Interval"),
    site("cli/src/cli/github/session.rs", "github_client_with", "timeout", 0, OneShot, "bounds each HTTP request"),
    site("cli/src/cli/github/session.rs", "GithubSession::token_within_deadline", "timeout", 0, OneShot, "bounds one token acquisition"),
    site("cli/src/cli/github/session.rs", "GithubSession::send_within_deadline", "timeout", 0, OneShot, "bounds one send"),
    site("cli/src/cli/grok_launch.rs", "acp_rpc", "timeout", 0, OneShot, "bounds one ACP preparation"),
    site("cli/src/cli/monitor.rs", "connect_within", "wait_ready", 0, OneShot, "a non-blocking connect, within the caller's one deadline"),
    site("cli/src/cli/monitor.rs", "wait_ready", "event::poll", 0, Helper, "readiness of one descriptor, within the caller's deadline"),
    site("cli/src/cli/monitor.rs", "write_frame", "wait_ready", 0, OneShot, "a full socket buffer, within the frame's one deadline"),
    site("cli/src/cli/monitor.rs", "read_frame", "wait_ready", 0, OneShot, "the next bytes of a frame, within its one deadline"),
    site("cli/src/cli/monitor.rs", "wait_for_ownership_by", "sleep", 0, Helper, "steps between lease probes, within the handoff's one deadline"),
    site("cli/src/cli/monitor.rs", "attach_or_spawn_until", "look_after", 0, BoundedRetry, "looks for the repairer's monitor until the handoff deadline"),
    site("cli/src/cli/monitor.rs", "attach_or_spawn_until", "wait_for_ownership_by", 1, BoundedRetry, "probes the lease until the handoff deadline"),
    site("cli/src/cli/monitor.rs", "attach_or_spawn_until", "look_after", 2, BoundedRetry, "looks for the started monitor until the handoff deadline"),
    site("cli/src/cli/monitor.rs", "look_after", "sleep", 0, Helper, "one step, never past the caller's deadline"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 0, Poll(RECOVERY), "recovery re-looks every RETRY_LOOK at an episode it cannot watch or whose gate is not Go, with no bound"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 1, Scheduled, "attendance check-ins and report deadlines the agents set"),
    site("cli/src/cli/monitor.rs", "start", "sleep_until", 2, Scheduled, "the hosted timers' due_at and repeat_every"),
    site("cli/src/cli/monitor.rs", "start", "sleep_until", 3, Poll(FAILURES), "TimerSet::retry_failed re-arms with backoff capped at RETRY_CAP while an occurrence will not write; a held lock is a release event, not this"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 4, BoundedRetry, "a failing fold backs off; the watchdog stands the monitor down after BUDGET of them"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 5, OneShot, "once, WAITERS_SETTLE after the monitor began serving"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 6, OneShot, "a retirement's quiesce deadline"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 7, OneShot, "a user command request's expiry, or a released park's settling"),
    site("cli/src/cli/monitor.rs", "start", "deadline", 8, BoundedRetry, "a held lock's waiter that failed to start is tried again; after WAITER_STARTS the loop ends with an error"),
    site("cli/src/cli/monitor.rs", "greet_within", "timeout", 0, OneShot, "bounds one client's hello"),
    site("cli/src/cli/monitor.rs", "write_queued", "timeout", 0, OneShot, "bounds one frame's write to a client"),
    site("cli/src/cli/monitor.rs", "OwnedTasks::end_greetings", "timeout", 0, OneShot, "one bound for ending every greeting at retirement"),
    site("cli/src/cli/monitor.rs", "retire", "sleep", 0, OneShot, "the retirement's budget for writers to drain"),
    site("cli/src/cli/monitor.rs", "retire", "timeout", 1, OneShot, "the retirement's budget for aborted tasks to settle"),
    site("cli/src/cli/monitor_keeper.rs", "Live::hear", "recv_timeout", 0, OneShot, "the outage's one report deadline; an attempt waits for its failure's events"),
    site("cli/src/cli/monitor_outage.rs", "Absence::after", "sleep_until", 0, OneShot, "the outage's one report deadline, which neither events nor failed attempts move; an attempt waits for its failure's events"),
    site("cli/src/cli/monitor_upgrade.rs", "drive", "recv_timeout", 0, OneShot, "discovery, within the upgrade's one deadline"),
    site("cli/src/cli/monitor_upgrade.rs", "drive", "recv_timeout", 1, OneShot, "the next operation ending, within the upgrade's one deadline"),
    site("cli/src/cli/monitor_upgrade.rs", "retire_with", "sleep", 0, BoundedRetry, "looks for an old monitor's exit until the grace or the deadline"),
    site("cli/src/cli/monitor_watchdog.rs", "start", "wait_timeout", 0, OneShot, "armed only while a turn or failing fold is in flight; a change notifies it"),
    site("cli/src/cli/owned_callback.rs", "deliver", "timeout_at", 0, OneShot, "bounds one backend callback"),
    site("cli/src/cli/owned_callback.rs", "deliver", "timeout_at", 1, OneShot, "bounds the callback's reap, within the same deadline"),
    site("cli/src/cli/owned_callback.rs", "wait_for_group_exit", "sleep_until", 0, BoundedRetry, "looks for the callback's group to go until the deadline, then fails"),
    site("cli/src/cli/pane_input.rs", "deliver", "within", 0, OneShot, "marks the receipts, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "deliver", "pause", 1, OneShot, "once, BEFORE_SUBMIT, between typing and submitting"),
    site("cli/src/cli/pane_input.rs", "Attempt::within", "timeout_at", 0, Helper, "one operation, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::pause", "sleep_until", 0, Helper, "a pause, never past the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::look", "within", 0, OneShot, "one screen dump, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::write", "within", 0, OneShot, "one write, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::interrupt", "within", 0, OneShot, "one interrupt, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::confirm", "within", 0, OneShot, "one receipt read, within the attempt's deadline"),
    site("cli/src/cli/pane_input.rs", "Attempt::confirm", "pause", 1, BoundedRetry, "re-reads the receipts until the attempt's deadline, then reports it unconfirmed"),
    site("cli/src/cli/pane_input.rs", "say_on", "sleep", 0, OneShot, "once, Codex's paste window, before submitting"),
    site("cli/src/cli/question_hook.rs", "broker", "timeout", 0, OneShot, "bounds the connect to the question broker"),
    site("cli/src/cli/question_hook.rs", "broker", "timeout", 1, OneShot, "bounds the frame's write"),
    site("cli/src/cli/question_hook.rs", "broker", "timeout", 2, OneShot, "bounds an unparked answer's read"),
    site("cli/src/cli/remote_control.rs", "Watched::next", "recv_timeout", 0, OneShot, "an owner's exit, within the caller's deadline"),
    site("cli/src/cli/source_diagnostic.rs", "Reader::finish_by", "timeout_at", 0, OneShot, "lets a run's stderr reader finish until its cutoff"),
    site("cli/src/cli/source_group.rs", "Owners::terminate", "wait_timeout", 0, OneShot, "groups settling, notified, until the drain's cutoff"),
    site("cli/src/cli/source_group.rs", "Draining::finish", "timeout_at", 0, OneShot, "the drain's wake, notified, until its deadline"),
    site("cli/src/cli/source_group.rs", "Recorded::arm", "timeout", 0, OneShot, "bounds the anchor's acknowledgement"),
    site("cli/src/cli/status_tui/mod.rs", "recv_until", "recv_timeout", 0, Helper, "the next event, or the caller's wait"),
    site("cli/src/cli/status_tui/mod.rs", "run_tui", "recv_until", 0, Scheduled, "an overlay's copied hint, until it is due to go"),
    site("cli/src/cli/status_tui/mod.rs", "run_tui", "recv_until", 1, Poll(FAILURES), "a failed rebuild retries with backoff capped at 30s, and a failed replacement too, with no bound; the spinner, clocks on screen and throttle are domain or event work"),
    site("cli/src/cli/status_watch.rs", "StatusWatch::wait", "recv_timeout", 0, Poll(FAILURES), "a failed refresh, or a failed replacement, retries with capped backoff and no bound"),
    site("cli/src/cli/status_watch.rs", "StatusWatch::wait", "recv_timeout", 1, Throttle, "after a wake, takes the burst within COALESCE"),
    site("cli/src/cli/stop_hook.rs", "OwnedWait::verdict_until", "timeout", 0, OneShot, "the wait's last words once its stdout ended"),
    site("cli/src/cli/stop_hook.rs", "OwnedWait::shutdown", "timeout", 0, OneShot, "the wait's exit, for ABANDON_GRACE"),
    site("cli/src/cli/stop_hook.rs", "under_deadline", "timeout_at", 0, Helper, "the caller's future, until its deadline if it has one"),
    site("cli/src/cli/stop_hook.rs", "await_verdict_under_deadline", "under_deadline", 0, OneShot, "an OpenCode Stop's verdict, until the hook's ceiling"),
    site("cli/src/cli/timer_source/clock.rs", "sleep_until", "deadline", 0, Helper, "the timers' clock, real outside a test-clock build"),
    site("cli/src/cli/timer_source/recurrence.rs", "Recurrence::parse", "interval", 0, NotAWait, "parses a timer's recurrence"),
    site("cli/src/cli/timer_source.rs", "parse", "interval", 0, NotAWait, "parses a timer's recurrence"),
    site("cli/src/cli/timer_source.rs", "deadline", "sleep_until", 0, Helper, "a deadline future that never fires for `None`"),
    site("cli/src/cli/turn_child.rs", "carry", "passes", 0, OneShot, "the report's last word, ending an attach still out"),
    site("cli/src/cli/turn_child.rs", "carry", "passes", 1, OneShot, "the report's last word, ending its wait for an outage's events"),
    site("cli/src/cli/turn_child.rs", "passes", "sleep_until", 0, Helper, "a deadline future that never fires for `None`"),
    site("cli/src/cli/turn_child.rs", "shared", "passes", 0, OneShot, "the report's last word, while the monitor holds it"),
    site("cli/src/cli/turn_child.rs", "result_of", "timeout", 0, OneShot, "bounds the hook's word on one presentation"),
    site("cli/src/cli/wait.rs", "items_with_context", "sleep_until", 0, Scheduled, "the timers' due_at and repeat_every"),
    site("cli/src/cli/wait.rs", "items_with_context", "sleep_until", 1, Poll(FAILURES), "TimerSet::retry_failed re-arms with backoff capped at RETRY_CAP while an occurrence will not write"),
    site("cli/src/cli/wait.rs", "items_with_context", "sleep", 2, Throttle, "debounces a filesystem wake"),
    site("cli/src/cli/wait.rs", "Beat::settle", "sleep", 0, Throttle, "debounces a filesystem wake"),
    site("cli/src/cli/wait.rs", "until_retirement_may_settle", "pause_unless_stopped", 0, Poll(FAILURES), "a deferred retirement with no group to watch is retried on a capped backoff, with no bound"),
    site("cli/src/cli/wait.rs", "until_retirement_may_settle", "until_a_member_exits", 1, Poll(FAILURES), "a member's exit, or the capped deferral backoff, re-armed while the retirement defers"),
    site("cli/src/cli/wait.rs", "pause_unless_stopped", "sleep", 0, Helper, "a pause a stop request cuts short"),
    site("cli/src/cli/wait.rs", "run_leased_command_source", "timeout_at", 0, OneShot, "reads a finished run's buffered output until its cutoff"),
    site("cli/src/cli/wait.rs", "run_leased_command_source", "pause_unless_stopped", 1, Poll(FAILURES), "a command source that keeps failing is restarted on a backoff capped at BACKOFF_CAP, with no bound"),
    site("cli/src/cli/web/bridge_instance.rs", "OwnedHost::stop", "timeout", 0, OneShot, "the host's exit after shutdown, then it is killed"),
    site("cli/src/cli/web/bridge_instance.rs", "BridgeInstance::admin_request", "timeout", 0, OneShot, "bounds one admin request"),
    site("cli/src/cli/web/bridge_instance.rs", "BridgeInstance::stop", "timeout_at", 0, OneShot, "the tasks' ends, within the stop's grace"),
    site("cli/src/cli/web/bridge_instance.rs", "connector", "sleep", 0, Poll(FAILURES), "reconnects to the web host every second while it refuses or drops, with no bound"),
    site("cli/src/cli/web/bridge_instance.rs", "start_host", "timeout", 0, OneShot, "bounds the host's readiness"),
    site("cli/src/cli/web/claude_questions.rs", "Registry::serve", "timeout", 0, OneShot, "bounds the hook's frame"),
    site("cli/src/cli/web/claude_questions.rs", "Registry::serve", "timeout", 1, OneShot, "bounds the reply's write"),
    site("cli/src/cli/web/mod.rs", "sample_initial", "recv_timeout", 0, OneShot, "the panes' first viewports, within one deadline"),
    site("cli/src/cli/web/mod.rs", "pane_supervisor", "sleep_until", 0, Poll(FAILURES), "a dead pane subscription is restarted on a backoff capped at RESTART_MAX, with no bound; verification samples are on demand"),
    site("cli/src/cli/web/native_questions.rs", "subscribe", "timeout", 0, OneShot, "bounds attaching to the native question server"),
    site("cli/src/cli/web/native_questions.rs", "send_answer", "timeout", 0, OneShot, "bounds one answer's send"),
    site("cli/src/cli/web/repo_session.rs", "RepoSession::reopen", "timeout", 0, OneShot, "bounds one pane reopen"),
    // cli/src
    site("cli/src/owner_sentinel.rs", "block_until_owner_gone", "sleep", 0, Throttle, "only after bytes arrived, so a stream of them cannot spin a core"),
    site("cli/src/proc_run.rs", "poll_readable", "event::poll", 0, Helper, "descriptors' readiness, until the caller's instant"),
    site("cli/src/proc_run.rs", "Managed::wait_timeout", "poll_readable", 0, OneShot, "the child's exit, within the caller's timeout"),
    site("cli/src/proc_run.rs", "Managed::terminate_async", "timeout", 0, OneShot, "the reap after a kill"),
    site("cli/src/proc_run.rs", "Managed::drop", "wait_timeout", 0, OneShot, "the reap after a kill, then the reaper adopts it"),
    site("cli/src/proc_run.rs", "run", "sleep", 0, NotAWait, "reader_delay is a test seam, always `None` outside cfg(test)"),
    site("cli/src/proc_run.rs", "run", "sleep_until", 1, OneShot, "the run's budget, beside its exit and pipes"),
    site("cli/src/proc_run.rs", "run", "sleep_until", 2, OneShot, "the held pipes' patience, beside their readiness"),
    site("cli/src/proc_run.rs", "run", "timeout", 3, OneShot, "the reap after a failed run's kill"),
    site("cli/src/proc_run.rs", "run_blocking", "sleep", 0, NotAWait, "reader_delay is a test seam, always `None` outside cfg(test)"),
    site("cli/src/proc_run.rs", "run_blocking", "poll_readable", 1, OneShot, "the exit or a pipe, within the run's budget"),
    site("cli/src/proc_run.rs", "run_blocking", "poll_readable", 2, OneShot, "the held pipes, within their patience"),
    site("cli/src/proc_run.rs", "run_blocking", "wait_timeout", 3, OneShot, "the reap after a failed run's kill"),
    site("cli/src/upgrade.rs", "Replacing::pass_until", "sleep", 0, Poll(FAILURES), "a failed replacement retries with backoff capped at RETRY_CAP, with no bound; an installation change wakes it as an event"),
    // core, user-config, web, web-cloudflare
    site("core/src/turn_policy.rs", "SessionState::next_deadline", "deadline", 0, NotAWait, "names an entry's deadline accessor"),
    site("user-config/src/lease.rs", "take", "sleep", 0, BoundedRetry, "a Busy lease is retried for PATIENCE, then believed"),
    site("web/src/bridge.rs", "connect_signaled", "timeout", 0, OneShot, "bounds the connector's WebSocket establishment"),
    site("web/src/bridge.rs", "run_connector", "timeout", 0, OneShot, "bounds the protocol handshake"),
    site("web/src/bridge.rs", "Session::accept", "timeout", 0, OneShot, "bounds the connector's hello"),
    site("web-cloudflare/src/dns.rs", "address", "timeout_at", 0, OneShot, "the whole lookup, within the start's grace"),
    site("web-cloudflare/src/dns.rs", "seek", "sleep", 0, BoundedRetry, "asks the authoritative servers each second, within the caller's grace"),
    site("web-cloudflare/src/door.rs", "Door::admit", "deadline", 0, NotAWait, "computes a session's expiry instant"),
    site("web-cloudflare/src/host.rs", "hold", "sleep_until", 0, OneShot, "the session's own expiry; a revocation, this host's or another's through the store watch, is an event"),
    site("web-cloudflare/src/tunnel.rs", "Isolated::start", "timeout", 0, OneShot, "bounds the SDK tunnel's close"),
    site("web-cloudflare/src/tunnel.rs", "Isolated::shutdown", "timeout", 0, OneShot, "bounds joining the tunnel's thread"),
    site("web-cloudflare/src/tunnel.rs", "Command::start", "timeout_at", 0, OneShot, "the child's URL, within the start's grace"),
    site("web-cloudflare/src/tunnel.rs", "said_url", "sleep", 0, BoundedRetry, "re-reads the output tail; only its caller's grace bounds it"),
    site("web-cloudflare/src/tunnel.rs", "CommandHandle::stop", "timeout", 0, OneShot, "the leader's exit after SIGTERM"),
    site("web-cloudflare/src/tunnel.rs", "CommandHandle::stop", "sleep", 1, BoundedRetry, "looks for the group to go for 2s after SIGKILL"),
    site("web-cloudflare/src/tunnel.rs", "CommandHandle::stop", "timeout", 2, OneShot, "each reader's end"),
    site("web-cloudflare/src/tunnel.rs", "CommandHandle::drop", "sleep", 0, BoundedRetry, "looks for the group to go for 500ms after SIGTERM"),
    site("web-cloudflare/src/tunnel.rs", "Lease::take", "sleep", 0, BoundedRetry, "two tries at the tunnel's lease, then it fails"),
    site("web-cloudflare/src/tunnel.rs", "probe", "timeout", 0, OneShot, "bounds each probe request"),
    site("web-cloudflare/src/tunnel.rs", "probe", "sleep", 1, BoundedRetry, "re-asks the public URL each second until the deadline"),
    site("web-cloudflare/src/tunnel.rs", "probe", "timeout_at", 2, OneShot, "the stream's first frame, within the deadline"),
    site("web-cloudflare/src/tunnel.rs", "Tunnel::up", "timeout_at", 0, OneShot, "the provider's start, a backstop past its own deadline"),
    site("web-cloudflare/src/tunnel.rs", "end", "timeout", 0, OneShot, "bounds a handle's stop"),
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Found {
    file: String,
    function: String,
    form: String,
    ordinal: usize,
    line: usize,
}

struct Scan<'a> {
    forms: &'a [&'a str],
    path: Vec<String>,
    found: Vec<(String, &'a str, usize)>,
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        other => quote::quote!(#other).to_string(),
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        is_cfg_test(a)
            || a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && matches!(&a.meta, syn::Meta::List(l)
                    if l.tokens.to_string().starts_with("feature = \"test-")))
    })
}

/// The form a call through `path` spells, if it is one of `forms`.
fn form_of<'f>(forms: &[&'f str], path: &[String]) -> Option<&'f str> {
    forms.iter().copied().find(|f| {
        let want: Vec<&str> = f.split("::").collect();
        path.len() >= want.len()
            && path[path.len() - want.len()..]
                .iter()
                .zip(&want)
                .all(|(p, w)| p == w)
    })
}

impl<'a> Scan<'a> {
    fn hit(&mut self, path: &[String], args: usize, line: usize) {
        if args == 0 {
            return;
        }
        if let Some(form) = form_of(self.forms, path) {
            self.found.push((self.path.join("::"), form, line));
        }
    }

    fn nested(&mut self, name: String, inner: impl FnOnce(&mut Self)) {
        self.path.push(name);
        inner(self);
        self.path.pop();
    }

    /// Macro bodies are tokens, not syntax: a call there is a name, its
    /// `::`-joined qualifiers, and a non-empty parenthesised group.
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, TokenTree};
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (i, t) in tokens.iter().enumerate() {
            match t {
                TokenTree::Ident(id) => {
                    let Some(TokenTree::Group(g)) = tokens.get(i + 1) else {
                        continue;
                    };
                    if g.delimiter() != Delimiter::Parenthesis
                        || matches!(i.checked_sub(1).map(|j| &tokens[j]), Some(TokenTree::Ident(f)) if f == "fn")
                    {
                        continue;
                    }
                    let mut path = vec![id.to_string()];
                    let mut j = i;
                    while j >= 3
                        && matches!(&tokens[j - 1], TokenTree::Punct(p) if p.as_char() == ':')
                        && matches!(&tokens[j - 2], TokenTree::Punct(p) if p.as_char() == ':')
                        && let TokenTree::Ident(q) = &tokens[j - 3]
                    {
                        path.insert(0, q.to_string());
                        j -= 3;
                    }
                    let args = usize::from(!g.stream().is_empty());
                    self.hit(&path, args, id.span().start().line);
                }
                TokenTree::Group(g) => self.tokens(g.stream()),
                _ => {}
            }
        }
    }
}

/// The attributes a statement-position expression carries, for the
/// kinds a `#[cfg]` is put on.
fn expr_attrs(e: &syn::Expr) -> &[syn::Attribute] {
    match e {
        syn::Expr::If(x) => &x.attrs,
        syn::Expr::Return(x) => &x.attrs,
        syn::Expr::Call(x) => &x.attrs,
        syn::Expr::MethodCall(x) => &x.attrs,
        syn::Expr::Block(x) => &x.attrs,
        syn::Expr::Macro(x) => &x.attrs,
        syn::Expr::Match(x) => &x.attrs,
        syn::Expr::Await(x) => &x.attrs,
        syn::Expr::Assign(x) => &x.attrs,
        syn::Expr::Loop(x) => &x.attrs,
        syn::Expr::While(x) => &x.attrs,
        syn::Expr::ForLoop(x) => &x.attrs,
        _ => &[],
    }
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let (attrs, name) = match item {
            syn::Item::Fn(i) => (&i.attrs, i.sig.ident.to_string()),
            syn::Item::Mod(i) => (&i.attrs, i.ident.to_string()),
            syn::Item::Impl(i) => (&i.attrs, type_name(&i.self_ty)),
            syn::Item::Trait(i) => (&i.attrs, i.ident.to_string()),
            syn::Item::Const(i) => (&i.attrs, i.ident.to_string()),
            syn::Item::Static(i) => (&i.attrs, i.ident.to_string()),
            syn::Item::Use(_) => return,
            _ => return syn::visit::visit_item(self, item),
        };
        if !test_only(attrs) {
            self.nested(name, |s| syn::visit::visit_item(s, item));
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        match item {
            syn::ImplItem::Fn(f) if test_only(&f.attrs) => {}
            syn::ImplItem::Fn(f) => self.nested(f.sig.ident.to_string(), |s| {
                syn::visit::visit_impl_item(s, item)
            }),
            _ => syn::visit::visit_impl_item(self, item),
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        match item {
            syn::TraitItem::Fn(f) if test_only(&f.attrs) => {}
            syn::TraitItem::Fn(f) => self.nested(f.sig.ident.to_string(), |s| {
                syn::visit::visit_trait_item(s, item)
            }),
            _ => syn::visit::visit_trait_item(self, item),
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(l) => &l.attrs[..],
            syn::Stmt::Macro(m) => &m.attrs[..],
            syn::Stmt::Expr(e, _) => expr_attrs(e),
            syn::Stmt::Item(_) => &[],
        };
        if !test_only(attrs) {
            syn::visit::visit_stmt(self, stmt);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        match &*call.func {
            syn::Expr::Path(p) => {
                let line = p
                    .path
                    .segments
                    .last()
                    .map_or(0, |s| s.ident.span().start().line);
                self.hit(&segments(&p.path), call.args.len(), line);
            }
            other => self.visit_expr(other),
        }
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    /// A form named as a value, as in `.map(tokio::time::sleep)`. Only a
    /// qualified path: a bare name is as likely a local called `timeout`.
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        if p.path.segments.len() > 1 {
            let line = p
                .path
                .segments
                .last()
                .map_or(0, |s| s.ident.span().start().line);
            self.hit(&segments(&p.path), 1, line);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.visit_expr(&call.receiver);
        self.hit(
            &[call.method.to_string()],
            call.args.len(),
            call.method.span().start().line,
        );
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
    }
}

/// Every timed wait `src` holds, as `file`, numbered within its
/// function.
fn scan(file: &str, src: &str, forms: &[&str]) -> Result<Vec<Found>, String> {
    let parsed = syn::parse_file(src).map_err(|e| format!("{file}: does not parse: {e}"))?;
    let mut s = Scan {
        forms,
        path: Vec::new(),
        found: Vec::new(),
    };
    s.visit_file(&parsed);
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    Ok(s.found
        .into_iter()
        .map(|(function, form, line)| {
            let n = seen.entry(function.clone()).or_default();
            let ordinal = *n;
            *n += 1;
            Found {
                file: file.to_string(),
                function,
                form: form.to_string(),
                ordinal,
                line,
            }
        })
        .collect())
}

fn relative(file: &Path) -> String {
    file.strip_prefix(crates_dir())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn production_sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for krate in CRATES {
        rs_files(&crates_dir().join(krate).join("src"), &mut files);
    }
    files.sort();
    files
}

/// The names a scan looks for: the primitives, and every helper's own
/// name, so a call of a helper is a site wherever it is.
fn forms(sites: &[Site]) -> Vec<&'static str> {
    let helpers: BTreeSet<&'static str> = sites
        .iter()
        .filter(|s| s.class == Helper)
        .map(|s| s.function.rsplit("::").next().unwrap_or(s.function))
        .collect();
    FORMS.iter().copied().chain(helpers).collect()
}

/// The inventory of `sources`, each read as the file it is relative to
/// `crates/`.
fn inventory(sources: &[(String, String)], sites: &[Site]) -> Result<Vec<Found>, String> {
    let forms = forms(sites);
    let mut out = Vec::new();
    for (file, src) in sources {
        out.extend(scan(file, src, &forms)?);
    }
    Ok(out)
}

fn workspace() -> Vec<(String, String)> {
    let files = production_sources();
    let production = Production::of(&files);
    files
        .iter()
        .filter(|f| !production.is_test_only(f))
        .map(|f| (relative(f), std::fs::read_to_string(f).unwrap()))
        .collect()
}

fn key(file: &str, function: &str, form: &str, ordinal: usize) -> String {
    format!("{file} {function} {form}#{ordinal}")
}

/// What differs between the inventory and `sites`: each found site with
/// no entry, and each entry with no site.
fn differences(found: &[Found], sites: &[Site]) -> Vec<String> {
    let listed: BTreeSet<String> = sites
        .iter()
        .map(|s| key(s.file, s.function, s.form, s.ordinal))
        .collect();
    let seen: BTreeSet<String> = found
        .iter()
        .map(|f| key(&f.file, &f.function, &f.form, f.ordinal))
        .collect();
    let mut out: Vec<String> = found
        .iter()
        .filter(|f| !listed.contains(&key(&f.file, &f.function, &f.form, f.ordinal)))
        .map(|f| {
            format!(
                "unclassified: {}:{} — site(\"{}\", \"{}\", \"{}\", {}, …)",
                f.file, f.line, f.file, f.function, f.form, f.ordinal
            )
        })
        .collect();
    out.extend(
        sites
            .iter()
            .filter(|s| !seen.contains(&key(s.file, s.function, s.form, s.ordinal)))
            .map(|s| {
                format!(
                    "listed but not found: {}",
                    key(s.file, s.function, s.form, s.ordinal)
                )
            }),
    );
    out
}

#[test]
fn every_timed_wait_is_classified() {
    let sources = workspace();
    for krate in CRATES {
        assert!(
            sources
                .iter()
                .any(|(f, _)| f.starts_with(&format!("{krate}/src/"))),
            "no sources found for {krate}"
        );
    }
    let found = inventory(&sources, SITES).unwrap();
    let bad = differences(&found, SITES);
    assert!(
        bad.is_empty(),
        "the timed waits changed; classify each by what ends it and what re-arms it \
         (docs/invariants.md I10), then list it:\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_site_says_why() {
    let mut seen = BTreeSet::new();
    for s in SITES {
        let k = key(s.file, s.function, s.form, s.ordinal);
        assert!(!s.why.trim().is_empty(), "{k}: no reason");
        assert!(seen.insert(k.clone()), "{k}: listed twice");
    }
}

/// I10 holds exactly when no poll but GitHub's is left, and while one
/// is, the entry names each residual's plan.
#[test]
fn invariant_status_matches_the_inventory() {
    let registry =
        std::fs::read_to_string(crates_dir().parent().unwrap().join("docs/invariants.md")).unwrap();
    let entry = registry
        .split("\n## ")
        .find(|e| e.starts_with("I10."))
        .expect("an I10 entry");
    let owed: BTreeSet<&str> = SITES
        .iter()
        .filter_map(|s| match s.class {
            Poll(Residual::Plan(plan)) => Some(plan),
            _ => None,
        })
        .collect();
    let status = if owed.is_empty() { "holds" } else { "target" };
    assert!(
        entry.contains(&format!("- Status: {status}\n")),
        "I10 must be `{status}` while the residual polls are {owed:?}:\n{entry}"
    );
    for plan in &owed {
        assert!(
            entry.contains(plan),
            "I10 does not name {plan}, which a residual poll owes:\n{entry}"
        );
    }
    assert!(
        SITES.iter().any(|s| s.class == Poll(Residual::Github)),
        "GitHub's poll is the one the rule allows, and it is gone from the list"
    );
}

#[test]
fn the_scan_refuses_each_change_to_the_sites() {
    const LISTED: &[Site] = &[
        site("x.rs", "S::listed", "sleep", 0, OneShot, "w"),
        site("x.rs", "pause", "sleep", 0, Helper, "w"),
        site("x.rs", "caller", "pause", 0, OneShot, "w"),
    ];
    const BASE: &str = "
        impl S { fn listed() { std::thread::sleep(D); } }
        fn pause(d: D) { std::thread::sleep(d) }
        fn caller() { pause(D); }
        fn getter(k: K) -> D { k.timeout() }
        #[cfg(test)]
        fn t() { std::thread::sleep(D); }
        #[cfg(feature = \"test-clock\")]
        fn clock() { std::thread::sleep(D); }
    ";
    let differ = |src: &str| {
        let sources = [("x.rs".to_string(), src.to_string())];
        differences(&inventory(&sources, LISTED).unwrap(), LISTED)
    };
    assert_eq!(differ(BASE), Vec::<String>::new());
    let unlisted = differ(&format!(
        "{BASE} fn new() {{ tokio::time::sleep(D).await; }}"
    ));
    assert!(
        unlisted.iter().any(|d| d.contains("\"new\", \"sleep\", 0")),
        "{unlisted:?}"
    );
    let second = differ(&BASE.replace(
        "std::thread::sleep(D); } }",
        "std::thread::sleep(D); rx.recv_timeout(D); } }",
    ));
    assert!(
        second
            .iter()
            .any(|d| d.contains("\"S::listed\", \"recv_timeout\", 1")),
        "{second:?}"
    );
    let reordered = differ(&BASE.replace(
        "std::thread::sleep(D); } }",
        "rx.recv_timeout(D); std::thread::sleep(D); } }",
    ));
    assert!(
        reordered
            .iter()
            .any(|d| d.contains("listed but not found: x.rs S::listed sleep#0")),
        "{reordered:?}"
    );
    let wrapped = differ(&format!(
        "{BASE} fn nap() {{ std::thread::sleep(D) }} fn spin() {{ loop {{ nap(); }} }}"
    ));
    assert!(
        wrapped.iter().any(|d| d.contains("\"nap\", \"sleep\", 0")),
        "{wrapped:?}"
    );
    let helper = differ(&format!("{BASE} fn spin() {{ loop {{ pause(D); }} }}"));
    assert!(
        helper.iter().any(|d| d.contains("\"spin\", \"pause\", 0")),
        "{helper:?}"
    );
    let in_macro = differ(&format!(
        "{BASE} fn sel() {{ tokio::select! {{ _ = tokio::time::sleep(D) => {{}} }} }}"
    ));
    assert!(
        in_macro.iter().any(|d| d.contains("\"sel\", \"sleep\", 0")),
        "{in_macro:?}"
    );
    let polled = differ(&format!(
        "{BASE} fn p() {{ rustix::event::poll(f, Some(&t)); }}"
    ));
    assert!(
        polled
            .iter()
            .any(|d| d.contains("\"p\", \"event::poll\", 0")),
        "{polled:?}"
    );
}
