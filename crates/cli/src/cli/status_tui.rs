//! `clank status --tui` — full-screen, READ-ONLY, live-updating
//! status view sized for a small zellij pane.
//!
//! A third render target over the same machinery `--watch` uses
//! (`StatusSnapshot::build_async` + `build_watcher`/`attach_watcher`)
//! — NOT a new data path. The responsive layout is a PURE function
//! [`render`]`(snapshot, rows, cols) -> Vec<String>` so every sizing
//! behavior is unit-testable headless; only the ioctl, the escape
//! sequences, and the loop touch a real terminal.
//!
//! No input handling: no raw mode, no event loop, no TUI framework.
//! Exit is closing the pane (or Ctrl-C — a SIGINT handler restores
//! the terminal first).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use super::status::{
    StatusSnapshot, attach_watcher, build_watcher, short_sha, waiting_actor, waiting_reason,
};
use clank_core::plan_view::WaitingOn;
use clank_core::wait::PlanWorkState;

// ── pure layout ─────────────────────────────────────────────

/// Render the snapshot into at most `rows` lines, each at most
/// `cols` chars. Priority-ordered tiers, greedy fit: a tier is
/// included only if the remaining rows hold its minimum height.
/// The tier-1 active-agent headline ALWAYS renders — even at
/// `rows == 1`.
pub(crate) fn render(snap: &StatusSnapshot, rows: u16, cols: u16) -> Vec<String> {
    let rows = rows.max(1) as usize;
    let cols = cols.max(1) as usize;
    let mut out: Vec<String> = Vec::with_capacity(rows);

    // Tier 1: active agent — unconditional.
    out.push(headline(snap));

    // Tier 2: current plan (first in snapshot order).
    if let Some(v) = snap.plans.first() {
        if out.len() < rows {
            let sha = v
                .sha
                .as_ref()
                .map(|s| format!(" @ {}", short_sha(s.as_str())))
                .unwrap_or_default();
            out.push(format!("plan: {}{sha}", v.plan.as_str()));
        }
        // Tier 3: gate state + waiting-on reason.
        if out.len() < rows {
            out.push(format!(
                "gate: {} — {}",
                v.gate,
                waiting_reason(&v.waiting_on)
            ));
        }
    }

    // Tier 4: queue — header + at least one name, else skip.
    if !snap.queue.is_empty() && rows - out.len() >= 2 {
        out.push(format!("queued ({}):", snap.queue.len()));
        for (i, name) in snap.queue.iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            out.push(format!("  {}. {name}", i + 1));
        }
    }

    // Tier 5: extras, line by line as space allows.
    if out.len() < rows {
        let branch = snap.branch.as_deref().unwrap_or("?");
        let head = snap.head_sha.as_deref().map(short_sha).unwrap_or("?");
        let dirty = if snap.worktree_dirty { " (dirty)" } else { "" };
        out.push(format!("branch: {branch} @ {head}{dirty}"));
    }
    if snap.plans.len() > 1 && rows - out.len() >= 2 {
        out.push("plans:".to_string());
        for v in &snap.plans {
            if out.len() >= rows {
                break;
            }
            out.push(format!(
                "  {} @ {} → {}",
                v.plan.as_str(),
                v.gate,
                waiting_actor(&v.waiting_on)
            ));
        }
    }
    if out.len() < rows {
        if let Some(fp) = &snap.last_finished {
            if snap.plans.is_empty() {
                out.push(format!(
                    "last finished: {} ({})",
                    fp.plan.as_str(),
                    short_sha(fp.finalized_at.as_str())
                ));
            }
        }
    }
    let pending_blocks = snap.blocks.iter().filter(|b| b.answer.is_none()).count();
    if pending_blocks > 0 && out.len() < rows {
        out.push(format!("blocks: {pending_blocks} pending"));
    }

    out.truncate(rows);
    for line in &mut out {
        *line = truncate_to(line, cols);
    }
    out
}

/// Tier-1 headline: whose turn is it to unblock progress.
/// Deterministic multi-plan rule (ruthless 9d01e47 concern 2):
/// with N>1 active plans the headline is a count plus
/// `actor(plan)` pairs in snapshot order (lexicographic by plan
/// key, since `derive_status` folds a BTreeMap); width truncation
/// trims the tail.
///
/// No active plan but a non-empty queue is still somebody's turn
/// — MASTER's, to promote the next item (lloyd, reopen round):
/// the headline names the master label and the head of the queue.
/// `idle` is reserved for truly idle (no plans AND empty queue).
fn headline(snap: &StatusSnapshot) -> String {
    match snap.plans.as_slice() {
        [] if !snap.queue.is_empty() => {
            let master = snap.master.as_deref().unwrap_or("master");
            format!(
                "* {master} — promote — {} ({} queued)",
                snap.queue[0],
                snap.queue.len()
            )
        }
        [] => "idle".to_string(),
        [v] => format!(
            "* {} — {} — {} @ {}",
            actor_of(v),
            verb_of(&v.waiting_on),
            v.plan.as_str(),
            v.gate
        ),
        many => {
            let pairs: Vec<String> = many
                .iter()
                .map(|v| format!("{}({})", actor_of(v), v.plan.as_str()))
                .collect();
            format!("* {} plans: {}", many.len(), pairs.join(", "))
        }
    }
}

fn actor_of(v: &PlanWorkState) -> String {
    match &v.waiting_on {
        // Blocked = awaiting the human, not the block's creator.
        WaitingOn::Blocked { .. } => "human".to_string(),
        w => waiting_actor(w),
    }
}

fn verb_of(w: &WaitingOn) -> &'static str {
    match w {
        WaitingOn::ReviewerApprovalsMissing { .. } => "reviewing",
        WaitingOn::GateReviewersMissing { .. } => "gate-reviewing",
        WaitingOn::MasterToRevise { .. } => "revising",
        WaitingOn::MasterToContinue => "continuing",
        WaitingOn::MasterToCommit => "committing",
        WaitingOn::MasterToFinalize => "finalizing",
        WaitingOn::Blocked { .. } => "blocked",
    }
}

/// Truncate to `cols` display chars with a `…` ellipsis. Char-based
/// (not byte) so multi-byte content can't split.
fn truncate_to(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    let mut t: String = s.chars().take(cols - 1).collect();
    t.push('…');
    t
}

// ── terminal plumbing (untested by design) ──────────────────

/// (rows, cols) of the stdout tty via `TIOCGWINSZ`. libc carries
/// the correct per-platform request constant + struct layout —
/// hardcoding the number is the portability trap. Falls back to
/// 24x80 when stdout isn't a terminal (piped / headless tests).
fn term_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: ws is a valid zeroed winsize; ioctl fills it or
    // returns -1.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        (ws.ws_row, ws.ws_col)
    } else {
        (24, 80)
    }
}

const ENTER_SEQ: &str = "\x1b[?1049h\x1b[?25l"; // alt-screen + hide cursor
const RESTORE_SEQ: &str = "\x1b[?25h\x1b[?1049l"; // show cursor + leave alt-screen

/// RAII alt-screen guard. `Drop` restores on every normal exit
/// path; a panic hook covers `panic=abort` (where Drop won't run);
/// a SIGINT/SIGTERM handler covers Ctrl-C in a direct terminal
/// (ruthless 9d01e47 concern 3 — without it the user's terminal is
/// left wedged on alt-screen with a hidden cursor).
struct AltScreen;

impl AltScreen {
    fn enter() -> Self {
        print!("{ENTER_SEQ}");
        let _ = std::io::stdout().flush();

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = std::io::stdout();
            let _ = out.write_all(RESTORE_SEQ.as_bytes());
            let _ = out.flush();
            prev(info);
        }));

        // SAFETY: installing a handler that only calls
        // async-signal-safe functions (write, _exit).
        let handler =
            restore_and_exit as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
        }
        AltScreen
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        print!("{RESTORE_SEQ}");
        let _ = std::io::stdout().flush();
    }
}

extern "C" fn restore_and_exit(sig: libc::c_int) {
    const RESTORE: &[u8] = b"\x1b[?25h\x1b[?1049l";
    // SAFETY: write + _exit are async-signal-safe.
    unsafe {
        libc::write(1, RESTORE.as_ptr().cast(), RESTORE.len());
        libc::_exit(128 + sig);
    }
}

/// One frame: cursor home, each line + clear-to-EOL, then clear
/// any leftover rows from a taller previous frame. No full-screen
/// clear → no flicker.
fn paint(lines: &[String]) {
    let mut buf = String::from("\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            buf.push_str("\r\n");
        }
        buf.push_str(line);
        buf.push_str("\x1b[K");
    }
    buf.push_str("\x1b[J");
    let mut out = std::io::stdout();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

/// The `--tui` loop: rebuild the snapshot on each `.clank/`/`.git`
/// change (same watcher as `--watch`), re-query the terminal size
/// every paint (the ~1s heartbeat doubles as the resize poll — no
/// SIGWINCH handler), repaint in place.
pub(crate) async fn run_tui(
    repo: PathBuf,
    basename: String,
    home: Option<PathBuf>,
    policy: crate::rebuild::CachePolicy,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = build_watcher(tx)?;
    attach_watcher(&mut watcher, &repo)?;

    let _guard = AltScreen::enter();
    loop {
        let snapshot =
            StatusSnapshot::build_async(&repo, &basename, home.as_deref(), policy, None, true)
                .await?;
        let (rows, cols) = term_size();
        paint(&render(&snapshot, rows, cols));

        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => while rx.recv_timeout(Duration::from_millis(200)).is_ok() {},
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
    use clank_core::repo_state::NonEmptyVec;
    use clank_core::vocab::CommitGateState;

    fn plan_state(stem: &str, waiting_on: WaitingOn) -> PlanWorkState {
        PlanWorkState {
            plan: PlanKey::parse(stem).unwrap(),
            sha: Some(CommitSha::parse(&format!("{:0<40}", "abc123")).unwrap()),
            gate: CommitGateState::Unreviewed,
            waiting_on,
            touched_code: false,
        }
    }

    fn reviewer_missing(label: &str) -> WaitingOn {
        WaitingOn::ReviewerApprovalsMissing {
            missing: NonEmptyVec::new(vec![AgentLabel::parse(label).unwrap()]).unwrap(),
        }
    }

    fn snap(plans: Vec<PlanWorkState>, queue: Vec<&str>) -> StatusSnapshot {
        StatusSnapshot {
            repo_path: "/r".into(),
            basename: "r".into(),
            branch: Some("master".into()),
            head_sha: Some(format!("{:0<40}", "deadbeef")),
            head_subject: None,
            worktree_dirty: false,
            plans,
            last_finished: None,
            blocks: Vec::new(),
            queue: queue.into_iter().map(str::to_string).collect(),
            master: Some("claude".into()),
        }
    }

    #[test]
    fn one_row_always_renders_active_agent() {
        // THE invariant: even at 1 row the active-agent headline
        // renders (ruthless 9d01e47 concern 1).
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        let lines = render(&s, 1, 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "* codex — reviewing — foo @ unreviewed");
    }

    #[test]
    fn idle_renders_idle_even_at_one_row() {
        // Truly idle: no plans AND empty queue.
        let s = snap(vec![], vec![]);
        assert_eq!(render(&s, 1, 80), vec!["idle".to_string()]);
    }

    #[test]
    fn no_plan_with_queue_names_master_and_next_promote() {
        // lloyd (reopen round): no active plan + non-empty queue is
        // MASTER's turn — promote the next item. The headline must
        // name the agent we're waiting on, not say `idle`.
        let s = snap(vec![], vec!["zellij-layout", "wfw-hint"]);
        let lines = render(&s, 1, 80);
        assert_eq!(lines[0], "* claude — promote — zellij-layout (2 queued)");
    }

    #[test]
    fn no_plan_with_queue_and_no_team_falls_back_to_master() {
        // Render path degrades on a teamless repo: no resolved
        // master label → the role name.
        let mut s = snap(vec![], vec!["zellij-layout"]);
        s.master = None;
        let lines = render(&s, 1, 80);
        assert_eq!(lines[0], "* master — promote — zellij-layout (1 queued)");
    }

    #[test]
    fn two_plans_get_deterministic_count_headline() {
        // Concern 2: N>1 active plans → count + actor(plan) pairs
        // in snapshot order. Exact string pinned.
        let s = snap(
            vec![
                plan_state("alpha", reviewer_missing("codex")),
                plan_state("beta", WaitingOn::MasterToContinue),
            ],
            vec![],
        );
        let lines = render(&s, 1, 80);
        assert_eq!(lines[0], "* 2 plans: codex(alpha), master(beta)");
    }

    #[test]
    fn width_truncates_every_line_with_ellipsis() {
        let s = snap(
            vec![plan_state(
                "a-very-long-plan-name-that-will-not-fit",
                reviewer_missing("codex"),
            )],
            vec!["another-quite-long-queued-name"],
        );
        let lines = render(&s, 24, 10);
        for line in &lines {
            assert!(
                line.chars().count() <= 10,
                "line wider than 10 cols: `{line}`"
            );
        }
        assert!(lines[0].ends_with('…'), "truncated line ends with ellipsis");
    }

    #[test]
    fn greedy_fit_drops_queue_when_rows_exhausted() {
        // Tiers 1-3 take 3 rows; the queue tier needs 2 more
        // (header + ≥1 item). At 3 and 4 rows it must not render a
        // bare header; at 5 it appears.
        let s = snap(
            vec![plan_state("foo", reviewer_missing("codex"))],
            vec!["q1", "q2"],
        );
        let at3 = render(&s, 3, 80);
        assert!(
            !at3.iter().any(|l| l.starts_with("queued")),
            "no queue tier at 3 rows: {at3:?}"
        );
        let at5 = render(&s, 5, 80);
        assert!(
            at5.iter().any(|l| l.starts_with("queued (2):")),
            "queue header at 5 rows: {at5:?}"
        );
        assert!(at5.iter().any(|l| l.contains("1. q1")));
    }

    #[test]
    fn queue_names_render_in_priority_order_as_space_allows() {
        let s = snap(
            vec![plan_state("foo", reviewer_missing("codex"))],
            vec!["first", "second", "third", "fourth"],
        );
        // 5 rows: 3 tiers + header + exactly ONE queue item.
        let lines = render(&s, 5, 80);
        assert_eq!(lines[3], "queued (4):");
        assert_eq!(lines[4], "  1. first");
        // 7 rows: three items fit.
        let lines = render(&s, 7, 80);
        assert_eq!(lines[6], "  3. third");
    }

    #[test]
    fn blocked_plan_headline_awaits_human() {
        use clank_core::plan_view::PlanBlock;
        let s = snap(
            vec![plan_state(
                "foo",
                WaitingOn::Blocked {
                    block: PlanBlock {
                        creator: AgentLabel::parse("claude").unwrap(),
                        name: "q".into(),
                        message: "is this right?".into(),
                    },
                },
            )],
            vec![],
        );
        let lines = render(&s, 1, 80);
        assert_eq!(lines[0], "* human — blocked — foo @ unreviewed");
    }

    #[test]
    fn extras_render_branch_line_when_space() {
        let s = snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]);
        let lines = render(&s, 24, 80);
        assert!(
            lines.iter().any(|l| l.starts_with("branch: master @ ")),
            "extras tier shows branch/head: {lines:?}"
        );
    }
}
