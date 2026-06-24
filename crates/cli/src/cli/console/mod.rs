//! `clank console` — a self-managed agent multiplexer (experimental).
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

use super::ConsoleArgs;
use super::term::{AltScreen, term_size};
// `enter_raw` (full cfmakeraw) so Ctrl-C and friends forward to the
// active child rather than acting on the console.
use mux::{Action, Mode};
use screen::{Event as ScreenEvent, Screen, Spec};

/// One merged event stream feeds the loop, exactly like `status_tui`:
/// child output, raw stdin, and resize all arrive as `Ev`.
enum Ev {
    Screen(ScreenEvent),
    Stdin(Vec<u8>),
    Resize,
}

pub async fn run(args: ConsoleArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
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
        anyhow::bail!("clank console: no screens to show");
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

    let mut frame = render::Frame::new(rows, cols);
    let mut active = 0usize;
    let mut mode = Mode::Passthrough;
    let prefix = mux::DEFAULT_PREFIX;
    let hint = format!("{} n·p·q", mux::prefix_label(prefix));

    repaint(&mut frame, &screens, active, &hint);

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
                    for b in bytes {
                        let (m, action) = mux::route(mode, b, prefix);
                        mode = m;
                        match action {
                            Action::Forward(data) => pty::write_all(screens[active].master, &data),
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            nav => {
                                if let Some(i) = mux::next_active(active, screens.len(), &nav) {
                                    active = i;
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
                    if screens.iter().all(|s| !s.is_alive()) {
                        quit = true;
                    } else {
                        need_repaint = true; // mark the dead tab ✗
                    }
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
            }
        }

        if quit {
            break;
        }
        if need_repaint {
            repaint(&mut frame, &screens, active, &hint);
        }
    }

    teardown(&mut screens);
    Ok(())
}

fn repaint(frame: &mut render::Frame, screens: &[Screen], active: usize, hint: &str) {
    let tabs: Vec<mux::Tab> = screens.iter().map(Screen::tab).collect();
    if let Ok(p) = screens[active].parser.lock() {
        frame.draw(p.screen(), &tabs, active, hint);
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
}
