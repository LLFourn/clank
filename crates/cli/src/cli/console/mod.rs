//! The console — a self-managed agent multiplexer (experimental).
//! It has no command of its own: `clank open` launches it as the
//! default workspace when you're not already in a zellij session.
//! See the plan: clank-console.
//!
//! The console is a thin VT multiplexer over the UNCHANGED
//! agent-start machinery. Each screen is a child process in a PTY
//! the console owns; an agent screen just runs `clank agent start
//! <label> --repo <repo>` — exactly what the zellij layout runs in a
//! pane — so bootstrap / fork / resume / identity / auto-mode all
//! happen untouched inside the child. The console owns only PTY
//! allocation, input routing, and drawing.
//!
//! Architecture (kept honest by the module split):
//! - [`mux`]   — pure routing + layout (no IO, fully unit-tested)
//! - [`pty`]   — PTY allocation on libc
//! - [`screen`]— a child + its vt100 grid + an always-draining reader
//! - [`render`]— flicker-free frame compositing
//! - this file — the event loop that ties them together (the only
//!   place IO happens)

mod mux;
mod pty;
mod render;
mod screen;

use std::path::Path;
use std::sync::mpsc;

use super::term::{AltScreen, term_size};
// `enter_raw` (full cfmakeraw) so Ctrl-C and friends forward to the
// active child rather than acting on the console.
use mux::{Action, Mode};
use screen::{Event as ScreenEvent, Screen, Spec};

/// One merged event stream feeds the loop, exactly like `status_tui`:
/// child output, raw stdin, resize, and the periodic work-state poll
/// all arrive as `Ev`.
enum Ev {
    Screen(ScreenEvent),
    Stdin(Vec<u8>),
    Resize,
    /// The set of agent labels clank currently expects to act (whose
    /// turn it is) — the "working" set, from the work-state poller.
    Working(std::collections::HashSet<String>),
}

/// Launch the console for a repo. Invoked by `clank open` (outside a
/// zellij session) — the console has no command of its own.
pub fn run(repo: Option<&Path>) -> anyhow::Result<()> {
    let repo = super::resolve_repo(repo)?;
    let specs = roster_specs(&repo);
    run_console(&repo, specs)
}

/// The screen list, derived from the one source of truth — the repo
/// roster ([`RegisteredSet`]): the master first, then commit
/// reviewers, then gate reviewers (deduped), each running the SAME
/// `clank agent start <label> --repo <repo>` the zellij layout runs
/// in a pane. A final `clank status --tui` screen is always present,
/// so a bootstrapped repo with no master yet still has something to
/// show.
///
/// [`RegisteredSet`]: crate::cli::teams_config::RegisteredSet
fn roster_specs(repo: &Path) -> Vec<Spec> {
    let exe = clank_exe();
    let repo_str = repo.display().to_string();
    let agent_spec = |label: &str| Spec {
        label: label.to_string(),
        program: exe.clone(),
        args: vec![
            "agent".into(),
            "start".into(),
            label.into(),
            "--repo".into(),
            repo_str.clone(),
        ],
    };

    let mut specs = Vec::new();
    // `home` is unused by the resolver (the roster is self-contained).
    if let Ok(Some(set)) = crate::agent_store::try_resolve_via_team_with(repo, None) {
        specs.extend(ordered_roster_labels(&set).iter().map(|l| agent_spec(l)));
    }

    specs.push(Spec {
        label: "status".into(),
        program: exe,
        args: vec!["status".into(), "--tui".into(), "--repo".into(), repo_str],
    });
    specs
}

/// The roster's labels in display order — master, then commit
/// reviewers, then gate reviewers — deduped so the screen list is
/// 1:1 with agents even if the one-role-per-agent invariant ever
/// slips. Pure so the ordering is unit-tested headless.
fn ordered_roster_labels(set: &crate::cli::teams_config::RegisteredSet) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    std::iter::once(set.master.as_str().to_string())
        .chain(
            set.commit_reviewers
                .iter()
                .map(|a| a.label.as_str().to_string()),
        )
        .chain(
            set.gate_reviewers
                .iter()
                .map(|a| a.label.as_str().to_string()),
        )
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

/// Path to the running clank binary, so spawned screens use the
/// EXACT same version as the console (not whatever `clank` resolves
/// to on `$PATH`). Falls back to the bare name.
fn clank_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "clank".to_string())
}

fn run_console(repo: &Path, specs: Vec<Spec>) -> anyhow::Result<()> {
    if specs.is_empty() {
        anyhow::bail!("clank open (console): no screens to show");
    }

    let _guard = AltScreen::enter_raw();
    let (rows, cols) = term_size();
    let (crows, ccols) = mux::content_rect(rows, cols);

    let (tx, rx) = mpsc::channel::<Ev>();

    // Screen events → Ev::Screen (one relay so every screen shares
    // the merged channel).
    let (stx, srx) = mpsc::channel::<ScreenEvent>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for ev in srx {
                if tx.send(Ev::Screen(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let mut screens = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        screens.push(Screen::spawn(spec, repo, crows, ccols, i, stx.clone())?);
    }

    // Raw stdin → Ev::Stdin. Blocking reads (VMIN=1 set by AltScreen)
    // in a dedicated thread; bytes are forwarded verbatim to the
    // active child except the prefix.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: reading our own stdin (fd 0) into a local
                // buffer.
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                if tx.send(Ev::Stdin(buf[..n as usize].to_vec())).is_err() {
                    break;
                }
            }
        });
    }

    // SIGWINCH → Ev::Resize (relayed off the shared forwarder).
    {
        let (wtx, wrx) = mpsc::channel::<()>();
        super::status::spawn_sigwinch_forwarder(wtx)?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            for _ in wrx {
                if tx.send(Ev::Resize).is_err() {
                    break;
                }
            }
        });
    }

    // Work-state poller → Ev::Working. Folds clank's state (only when
    // the `.clank` signature changes) to learn whose turn it is, and
    // pushes the set when it moves. A dedicated thread with its own
    // runtime keeps the async fold off the sync event loop.
    spawn_work_poller(repo, tx.clone());

    // The status screen (always last in roster_specs) — Meta-s jumps
    // to it.
    let status_idx = screens.iter().position(|s| s.label == "status");

    let mut frame = render::Frame::new(rows, cols);
    let mut active = 0usize;
    let mut working: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut mode = Mode::Passthrough;
    let hint = "M-1…9 switch · M-s status · M-a q quit";

    repaint(&mut frame, &screens, active, &working, hint);

    // Batch each wakeup: drain everything currently queued, apply it,
    // and repaint at most once. This coalesces the repaint SIGNAL
    // (never the drain — that's always-on in each reader thread), so
    // a chatty agent can't trigger a repaint per output chunk.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }

        let mut need_repaint = false;
        let mut quit = false;
        for ev in batch {
            match ev {
                Ev::Stdin(bytes) => {
                    // Consume the burst left-to-right; `route` reports
                    // how many bytes each step took (Meta chords are 2).
                    let mut i = 0;
                    while i < bytes.len() {
                        let (consumed, m, action) = mux::route(mode, &bytes[i..]);
                        mode = m;
                        i += consumed.max(1);
                        match action {
                            Action::Forward(data) => {
                                if screens[active].is_alive() {
                                    pty::write_all(screens[active].master, &data);
                                } else if data.iter().any(|&b| b == b'\r' || b == b'\n') {
                                    // Dead screen: Enter respawns it in place.
                                    respawn(&mut screens, active, repo, &stx);
                                    need_repaint = true;
                                }
                                // Other input to a dead screen is dropped.
                            }
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            Action::FocusStatus => {
                                if let Some(idx) = status_idx
                                    && idx != active
                                {
                                    active = idx;
                                    need_repaint = true;
                                }
                            }
                            nav => {
                                if let Some(idx) = mux::next_active(active, screens.len(), &nav) {
                                    active = idx;
                                    need_repaint = true;
                                }
                            }
                        }
                    }
                }
                Ev::Screen(ScreenEvent::Output(i)) => {
                    if i == active {
                        need_repaint = true;
                    }
                }
                Ev::Screen(ScreenEvent::Exit) => {
                    // A crashed agent stays as a ✗ tab showing "press
                    // Enter to respawn" — never auto-quit, never
                    // auto-respawn. The user is in control (quit with
                    // the prefix). Repaint so the ✗ / dead-screen
                    // prompt appears.
                    need_repaint = true;
                }
                Ev::Resize => {
                    let (rows, cols) = term_size();
                    let (crows, ccols) = mux::content_rect(rows, cols);
                    for s in &screens {
                        pty::set_winsize(s.master, crows, ccols);
                        if let Ok(mut p) = s.parser.lock() {
                            p.screen_mut().set_size(crows, ccols);
                        }
                    }
                    frame.resize(rows, cols);
                    need_repaint = true;
                }
                Ev::Working(set) => {
                    if set != working {
                        working = set;
                        need_repaint = true;
                    }
                }
            }
        }

        if quit {
            break;
        }
        if need_repaint {
            repaint(&mut frame, &screens, active, &working, hint);
        }
    }

    teardown(&mut screens);
    Ok(())
}

fn repaint(
    frame: &mut render::Frame,
    screens: &[Screen],
    active: usize,
    working: &std::collections::HashSet<String>,
    hint: &str,
) {
    let tabs: Vec<mux::Tab> = screens
        .iter()
        .map(|s| mux::Tab {
            label: s.label.clone(),
            alive: s.is_alive(),
            working: working.contains(&s.label),
        })
        .collect();
    let bytes = if screens[active].is_alive() {
        match screens[active].parser.lock() {
            Ok(p) => frame.draw(p.screen(), &tabs, active, hint),
            Err(_) => return,
        }
    } else {
        frame.draw_dead(&screens[active].label, &tabs, active, hint)
    };
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}

/// Poll clank's work-state in a dedicated thread and push the
/// "working" set (whose turn it is) to the loop when it changes.
/// Re-folds only when the `.clank` input signature moves, so an idle
/// repo costs one cheap signature hash per tick.
fn spawn_work_poller(repo: &Path, tx: mpsc::Sender<Ev>) {
    let repo = repo.to_path_buf();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let mut last_sig = None;
        let mut last_working: Option<std::collections::HashSet<String>> = None;
        let mut first = true;
        loop {
            let sig = crate::cli::status::input_signature(&repo).ok();
            if first || sig != last_sig {
                first = false;
                last_sig = sig;
                let working = rt.block_on(working_labels(&repo, home.as_deref()));
                if last_working.as_ref() != Some(&working) {
                    last_working = Some(working.clone());
                    if tx.send(Ev::Working(working)).is_err() {
                        return;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    });
}

/// The set of agent labels clank currently expects to act, folded
/// from the same status snapshot `clank status` uses. Master-action
/// states map to the master's label; the reviewer-missing states map
/// to those reviewers; a blocked plan has nobody working. Empty on
/// any fold/roster error (the indicator just shows no one working).
async fn working_labels(repo: &Path, home: Option<&Path>) -> std::collections::HashSet<String> {
    use clank_core::plan_view::WaitingOn;
    let mut working = std::collections::HashSet::new();
    let Ok(snap) = crate::cli::status::snapshot(repo, home).await else {
        return working;
    };
    let master = crate::agent_store::try_resolve_via_team_with(repo, home)
        .ok()
        .flatten()
        .map(|s| s.master.as_str().to_string());
    for plan in &snap.plans {
        match &plan.waiting_on {
            WaitingOn::ReviewerApprovalsMissing { missing }
            | WaitingOn::GateReviewersMissing { missing } => {
                for label in missing.iter() {
                    working.insert(label.as_str().to_string());
                }
            }
            WaitingOn::MasterToRevise { .. }
            | WaitingOn::MasterToContinue
            | WaitingOn::MasterToFinalize
            | WaitingOn::MasterToCommit
            | WaitingOn::MasterToFixCommitTag => {
                if let Some(m) = &master {
                    working.insert(m.clone());
                }
            }
            WaitingOn::Blocked { .. } => {}
        }
    }
    working
}

/// Relaunch screen `idx` from its stored spec, replacing the dead
/// one in place (same index, so its tab + position are preserved)
/// and reaping the old child. Sized to the current terminal.
fn respawn(screens: &mut [Screen], idx: usize, repo: &Path, stx: &mpsc::Sender<ScreenEvent>) {
    let (rows, cols) = term_size();
    let (crows, ccols) = mux::content_rect(rows, cols);
    let spec = screens[idx].spec.clone();
    if let Ok(new) = Screen::spawn(&spec, repo, crows, ccols, idx, stx.clone()) {
        let mut old = std::mem::replace(&mut screens[idx], new);
        let _ = old.child.wait();
        pty::close(old.master);
    }
}

/// Hang up every child's process group (SIGHUP — `setsid` made each
/// child its own group leader, so `-pid` reaches the agent and any
/// helpers it spawned), then reap and close the masters. Agents
/// persist their session to disk continuously, so a hangup loses no
/// recoverable state — relaunching resumes them.
fn teardown(screens: &mut [Screen]) {
    for s in screens.iter() {
        let pid = s.child.id() as libc::pid_t;
        // SAFETY: signalling a process group we created.
        unsafe { libc::kill(-pid, libc::SIGHUP) };
    }
    for s in screens.iter_mut() {
        let _ = s.child.wait();
        pty::close(s.master);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::teams_config::{AgentDescription, RegisteredSet, ResolvedAgent};
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;

    fn fixture(master: &str, commit: &[&str], gate: &[&str]) -> RegisteredSet {
        let label = |s: &str| AgentLabel::parse(s).unwrap();
        let desc = || AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        };
        let agents = |ls: &[&str]| {
            ls.iter()
                .map(|l| ResolvedAgent {
                    label: label(l),
                    desc: desc(),
                })
                .collect()
        };
        RegisteredSet {
            master: label(master),
            master_desc: desc(),
            commit_reviewers: agents(commit),
            gate_reviewers: agents(gate),
        }
    }

    #[test]
    fn roster_labels_are_master_then_commit_then_gate() {
        let set = fixture("claude", &["codex"], &["ruthless"]);
        assert_eq!(
            ordered_roster_labels(&set),
            vec!["claude", "codex", "ruthless"]
        );
    }

    #[test]
    fn roster_labels_dedupe_keeping_first_occurrence() {
        // Should the one-role-per-agent invariant ever slip, a label
        // in two tiers still yields exactly one screen.
        let set = fixture("claude", &["codex", "claude"], &["codex"]);
        assert_eq!(ordered_roster_labels(&set), vec!["claude", "codex"]);
    }

    #[test]
    fn respawn_replaces_a_dead_screen_in_place() {
        use screen::Spec;
        use std::time::{Duration, Instant};

        let (tx, rx) = mpsc::channel();
        // `true` exits immediately; both the original and the respawn
        // self-exit, so the test leaks no long-lived process.
        let spec = Spec {
            label: "x".into(),
            program: "true".into(),
            args: vec![],
        };
        let mut screens =
            vec![Screen::spawn(&spec, Path::new("/"), 24, 80, 0, tx.clone()).expect("spawn")];

        let deadline = Instant::now() + Duration::from_secs(5);
        while screens[0].is_alive() && Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(100));
        }
        assert!(!screens[0].is_alive(), "child should have exited");

        let old_pid = screens[0].child.id();
        respawn(&mut screens, 0, Path::new("/"), &tx);
        assert_eq!(screens.len(), 1, "respawn replaces in place, never adds");
        assert_ne!(
            screens[0].child.id(),
            old_pid,
            "a fresh child replaced the dead one"
        );
    }
}
