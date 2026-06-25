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

/// A live mouse selection bound to the CONCRETE screen it started on
/// (`idx`) — not just "the main pane", so follow-mode swapping the
/// active agent mid-drag can't move the highlight or copy to a
/// different screen. `anchor`/`end` are pane-relative, 0-based, and
/// not yet ordered.
struct Sel {
    idx: usize,
    anchor: (u16, u16),
    end: (u16, u16),
}

impl Sel {
    /// The render-side selection for the CURRENT view, given which
    /// screen is the status pane and which agent is active. `None` when
    /// the anchored screen isn't on display (follow-mode switched the
    /// main pane to a different agent) — there's nothing to highlight.
    fn to_render(&self, active: usize, status_idx: usize) -> Option<render::Selection> {
        let status = self.idx == status_idx;
        if !status && self.idx != active {
            return None;
        }
        let (start, end) = mux::order_cells(self.anchor, self.end);
        Some(render::Selection { status, start, end })
    }
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
    // The status pane is the last screen (roster_specs invariant); the
    // others are switchable agents. Require ≥1 agent — a console that's
    // only the status pane has nothing to multiplex (and `active`
    // would alias the status screen, double-locking it on repaint).
    // Check before entering raw mode so the error prints cleanly.
    let status_idx = specs.len().checked_sub(1).filter(|&i| i > 0);
    let Some(status_idx) = status_idx else {
        anyhow::bail!(
            "clank open (console): no agents in this repo's roster — \
             add one with `clank agent add <label> --tool <claude|codex>` \
             and `clank agent promote <label>`"
        );
    };

    let _guard = AltScreen::enter_raw();
    let (rows, cols) = term_size();
    let mut zoomed = false; // active agent full-screen (for clean copy)
    let mut layout = effective_layout(zoomed, rows, cols);

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
        let rect = screen_rect(i, status_idx, layout);
        screens.push(Screen::spawn(
            spec,
            repo,
            rect.rows,
            rect.cols,
            i,
            stx.clone(),
        )?);
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

    let mut frame = render::Frame::new(rows, cols);
    let mut active = 0usize; // active AGENT (0..status_idx)
    let mut status_focused = false; // input + cursor on the status pane
    let mut follow = false; // follow-active mode: main pane tracks the working agent
    let mut selection: Option<Sel> = None; // live mouse selection
    let mut working: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut mode = Mode::Passthrough;
    let hint = "M-z zoom · M-s status · M-0 follow · M-1-9 pin · ^\\ quit";
    let ctx = |active, status_focused, follow, zoomed, selection: &Option<Sel>, layout| RenderCtx {
        status_idx,
        active,
        status_focused,
        follow,
        zoomed,
        selection: selection
            .as_ref()
            .and_then(|s| s.to_render(active, status_idx)),
        layout,
        hint,
    };

    repaint(
        &mut frame,
        &screens,
        &working,
        ctx(active, status_focused, follow, zoomed, &selection, layout),
    );

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
                        // Input goes to the focused pane: the status
                        // pane when status-focused, else the active agent.
                        let target = if status_focused { status_idx } else { active };
                        match action {
                            Action::Forward(data) => {
                                if screens[target].is_alive() {
                                    pty::write_all(screens[target].master, &data);
                                } else if data.iter().any(|&b| b == b'\r' || b == b'\n') {
                                    // Dead pane: Enter respawns it in place.
                                    respawn(&mut screens, target, repo, &stx, layout);
                                    need_repaint = true;
                                }
                                // Other input to a dead pane is dropped.
                            }
                            Action::Quit => {
                                quit = true;
                                break;
                            }
                            Action::FollowActive => {
                                // Jump to the working agent + enable follow.
                                // Orthogonal to status focus, so leave the
                                // status pane and show the agent.
                                follow = true;
                                status_focused = false;
                                if let Some(idx) =
                                    primary_working_idx(&screens, status_idx, &working)
                                {
                                    active = idx;
                                }
                                need_repaint = true;
                            }
                            Action::FocusStatus => {
                                if !status_focused {
                                    status_focused = true;
                                    need_repaint = true;
                                }
                            }
                            Action::SwitchTo(_) | Action::Next | Action::Prev => {
                                // Manual nav PINS the pane: follow off,
                                // even when the target is already active.
                                if follow {
                                    follow = false;
                                    need_repaint = true;
                                }
                                if let Some((idx, focus)) =
                                    mux::resolve_nav(active, status_idx, status_focused, &action)
                                {
                                    active = idx;
                                    status_focused = focus;
                                    need_repaint = true;
                                }
                            }
                            Action::ToggleZoom => {
                                // Zoom is a layout change: recompute and
                                // re-propagate winsize to every child, or
                                // the agent draws at the old split width.
                                zoomed = !zoomed;
                                let (r, c) = term_size();
                                layout = effective_layout(zoomed, r, c);
                                relayout(&screens, status_idx, layout, &mut frame, r, c);
                                need_repaint = true;
                            }
                            Action::Mouse(ev) => {
                                let wheel = ev.button & 0b0100_0000 != 0;
                                let motion = ev.button & 0b0010_0000 != 0;
                                let left = ev.button & 0b11 == 0;
                                // Resolve the hit to the CONCRETE screen under
                                // the cursor (which agent / the status screen),
                                // not just "the main pane": follow-mode can swap
                                // the active agent mid-drag, so a selection bound
                                // to a slot would jump panes. `under` is the
                                // screen index + its pane-relative (row, col).
                                let under =
                                    mux::pane_at(layout, ev.col, ev.row).map(|(p, r, c)| {
                                        let idx = match p {
                                            mux::Pane::Status => status_idx,
                                            mux::Pane::Main => active,
                                        };
                                        (idx, r, c)
                                    });
                                // If that child grabbed the mouse, the event is
                                // the AGENT's: forward it re-encoded into the
                                // pane's coordinates.
                                let grab = under.and_then(|(idx, r, c)| {
                                    child_grabs_mouse(&screens, idx).map(|enc| (idx, r, c, enc))
                                });
                                if let Some((idx, r, c, enc)) = grab {
                                    let data = mux::encode_mouse_for_child(ev, c + 1, r + 1, enc);
                                    pty::write_all(screens[idx].master, &data);
                                } else if wheel {
                                    // Wheel over a pane that isn't tracking the
                                    // mouse: nothing to scroll (no scrollback) —
                                    // swallow rather than misread it as a click.
                                } else if ev.pressed && left && !motion {
                                    selection = under.map(|(idx, r, c)| Sel {
                                        idx,
                                        anchor: (r, c),
                                        end: (r, c),
                                    });
                                    need_repaint = true;
                                } else if ev.pressed && left && motion {
                                    // Extend only while the cursor is still over
                                    // the screen the drag began on.
                                    if let (Some(sel), Some((idx, r, c))) = (&mut selection, under)
                                        && idx == sel.idx
                                    {
                                        sel.end = (r, c);
                                        need_repaint = true;
                                    }
                                } else if !ev.pressed
                                    && let Some(sel) = &selection
                                {
                                    copy_selection(&screens[sel.idx], sel);
                                }
                            }
                            Action::None => {}
                        }
                    }
                }
                Ev::Screen(ScreenEvent::Output(i)) => {
                    // Both visible panes repaint on output: the active
                    // agent and the always-on status pane.
                    if i == active || i == status_idx {
                        need_repaint = true;
                    }
                }
                Ev::Screen(ScreenEvent::Exit) => {
                    // A crashed pane stays put showing "press Enter to
                    // respawn" — never auto-quit, never auto-respawn.
                    // Repaint so the ✗ / prompt appears.
                    need_repaint = true;
                }
                Ev::Resize => {
                    let (rows, cols) = term_size();
                    layout = effective_layout(zoomed, rows, cols);
                    relayout(&screens, status_idx, layout, &mut frame, rows, cols);
                    need_repaint = true;
                }
                Ev::Working(set) => {
                    if set != working {
                        working = set;
                        // Follow-active mode: the main pane tracks
                        // whoever is now the working agent (focus
                        // unchanged — follow is orthogonal to it).
                        if follow
                            && let Some(idx) = primary_working_idx(&screens, status_idx, &working)
                        {
                            active = idx;
                        }
                        need_repaint = true;
                    }
                }
            }
        }

        if quit {
            break;
        }
        if need_repaint {
            repaint(
                &mut frame,
                &screens,
                &working,
                ctx(active, status_focused, follow, zoomed, &selection, layout),
            );
        }
    }

    teardown(&mut screens);
    Ok(())
}

/// Index of the primary working agent (first in roster order whose
/// label is in the work-state `working` set), for follow-active mode.
fn primary_working_idx(
    screens: &[Screen],
    status_idx: usize,
    working: &std::collections::HashSet<String>,
) -> Option<usize> {
    let labels: Vec<String> = screens[..status_idx]
        .iter()
        .map(|s| s.label.clone())
        .collect();
    mux::primary_working(&labels, working)
}

/// `Some(encoding)` if screen `idx`'s child has turned ON mouse
/// reporting — then the mouse is the AGENT's (forwarded in its own
/// encoding), not the console's. `None` means the child isn't tracking
/// the mouse, so the console keeps it for selection.
fn child_grabs_mouse(screens: &[Screen], idx: usize) -> Option<mux::ChildMouseEncoding> {
    let guard = screens[idx].parser.lock().ok()?;
    let screen = guard.screen();
    if screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None {
        return None;
    }
    Some(match screen.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Sgr => mux::ChildMouseEncoding::Sgr,
        _ => mux::ChildMouseEncoding::Legacy,
    })
}

/// Copy the selection from a screen's grid to the system clipboard via
/// OSC 52 (best-effort — silently a no-op on terminals that don't
/// support it; `Alt-z` zoom + native copy is the fallback).
fn copy_selection(screen: &Screen, sel: &Sel) {
    let (start, end) = mux::order_cells(sel.anchor, sel.end);
    let text = match screen.parser.lock() {
        Ok(p) => p.screen().contents_between(start.0, start.1, end.0, end.1),
        Err(_) => return,
    };
    if text.is_empty() {
        return;
    }
    let osc = format!("\x1b]52;c;{}\x1b\\", base64_encode(text.as_bytes()));
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = out.write_all(osc.as_bytes());
    let _ = out.flush();
}

/// Standard base64 (RFC 4648) — hand-rolled so OSC 52 needs no
/// dependency (the deps-only-where-it-hurts rule; this is ~20 lines).
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The rect a screen's CHILD is sized to: the status pane for
/// `status_idx`, the main (agent) pane otherwise. Clamped to ≥1×1 so a
/// PTY/parser is never given a zero dimension on a tiny terminal
/// (where the layout may shrink the status pane to 0 — it just isn't
/// drawn; render uses the real layout rects).
fn screen_rect(idx: usize, status_idx: usize, layout: mux::Layout) -> mux::Rect {
    let r = if idx == status_idx {
        layout.status
    } else {
        layout.main
    };
    mux::Rect {
        rows: r.rows.max(1),
        cols: r.cols.max(1),
        ..r
    }
}

/// The scalar render state the loop hands to [`repaint`]: which screen
/// is the status pane, which agent is active, whether status has
/// focus, the layout, and the chrome hint. (The screens + working set
/// are passed separately — they're the data, this is the view.)
#[derive(Clone, Copy)]
struct RenderCtx<'a> {
    status_idx: usize,
    active: usize,
    status_focused: bool,
    follow: bool,
    zoomed: bool,
    selection: Option<render::Selection>,
    layout: mux::Layout,
    hint: &'a str,
}

/// The effective layout for the current mode: zoomed (active agent
/// full-screen, status/divider collapsed) or the normal split.
fn effective_layout(zoomed: bool, rows: u16, cols: u16) -> mux::Layout {
    if zoomed {
        mux::zoom_layout(rows, cols)
    } else {
        mux::layout(rows, cols)
    }
}

/// Re-propagate a layout change to every child (PTY winsize + parser
/// size to its new rect) and clear the frame. The winsize MUST fire on
/// every layout change — resize OR zoom toggle — or a child draws at
/// its old size.
fn relayout(
    screens: &[Screen],
    status_idx: usize,
    layout: mux::Layout,
    frame: &mut render::Frame,
    rows: u16,
    cols: u16,
) {
    for (i, s) in screens.iter().enumerate() {
        let rect = screen_rect(i, status_idx, layout);
        pty::set_winsize(s.master, rect.rows, rect.cols);
        if let Ok(mut p) = s.parser.lock() {
            p.screen_mut().set_size(rect.rows, rect.cols);
        }
    }
    frame.resize(rows, cols);
}

fn repaint(
    frame: &mut render::Frame,
    screens: &[Screen],
    working: &std::collections::HashSet<String>,
    ctx: RenderCtx,
) {
    // Tabs are the agents only (the status pane is always on, not a tab).
    let tabs: Vec<mux::Tab> = screens[..ctx.status_idx]
        .iter()
        .map(|s| mux::Tab {
            label: s.label.clone(),
            alive: s.is_alive(),
            working: working.contains(&s.label),
        })
        .collect();
    let chrome = render::ChromeBar {
        tabs: &tabs,
        active: ctx.active,
        status_focused: ctx.status_focused,
        follow: ctx.follow,
        zoomed: ctx.zoomed,
        hint: ctx.hint,
    };

    // Lock the status parser for the whole frame (consistent order:
    // status before agent), then composite both panes.
    let status_guard = match screens[ctx.status_idx].parser.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let status_screen = status_guard.screen();

    let bytes = if screens[ctx.active].is_alive() {
        let agent_guard = match screens[ctx.active].parser.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        frame.draw(
            render::MainPane::Live(agent_guard.screen()),
            status_screen,
            ctx.layout,
            ctx.status_focused,
            ctx.selection,
            chrome,
        )
    } else {
        frame.draw(
            render::MainPane::Dead(&screens[ctx.active].label),
            status_screen,
            ctx.layout,
            ctx.status_focused,
            ctx.selection,
            chrome,
        )
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

/// Relaunch screen `idx` from its stored spec, replacing the dead one
/// in place (same index, so its tab + pane are preserved) and reaping
/// the old child. Sized to that screen's current pane rect.
fn respawn(
    screens: &mut [Screen],
    idx: usize,
    repo: &Path,
    stx: &mpsc::Sender<ScreenEvent>,
    layout: mux::Layout,
) {
    let status_idx = screens.len() - 1;
    let rect = screen_rect(idx, status_idx, layout);
    let spec = screens[idx].spec.clone();
    if let Ok(new) = Screen::spawn(&spec, repo, rect.rows, rect.cols, idx, stx.clone()) {
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
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn selection_is_bound_to_its_screen_not_the_main_slot() {
        // A drag started on agent screen 1. Follow-mode can switch the
        // active agent mid-drag (the bug the reviewer caught): the
        // highlight must track the SCREEN, not whoever is now in the
        // main pane.
        let status_idx = 3;
        let sel = Sel {
            idx: 1,
            anchor: (0, 0),
            end: (0, 4),
        };
        // Agent 1 still active → highlight in the main pane.
        let r = sel.to_render(1, status_idx).expect("shown while active");
        assert!(!r.status, "an agent selection is a main-pane selection");
        // Active switched to agent 2 → the anchored screen is off-screen,
        // so nothing is highlighted (and copy still reads screen 1).
        assert!(
            sel.to_render(2, status_idx).is_none(),
            "no highlight once the anchored agent is no longer shown"
        );
        // A selection anchored on the status screen always renders in the
        // status pane (it's always on screen), regardless of active.
        let s = Sel {
            idx: status_idx,
            anchor: (0, 0),
            end: (0, 1),
        };
        assert!(
            s.to_render(2, status_idx)
                .expect("status always shown")
                .status
        );
    }

    #[test]
    fn zoom_sizes_agent_full_and_status_to_a_1x1_stub() {
        // The integration the pure layout test can't catch: under the
        // zoom layout, screen_rect sizes the active agent to the full
        // content area and the collapsed status pane to a ≥1×1 stub
        // (so its PTY/parser is never zero-sized).
        let (rows, cols) = (24u16, 80u16);
        let status_idx = 2; // agents 0,1 + status at 2
        let z = effective_layout(true, rows, cols);
        let agent = screen_rect(0, status_idx, z);
        assert_eq!((agent.rows, agent.cols), (23, 80), "agent fills content");
        let status = screen_rect(status_idx, status_idx, z);
        assert!(
            status.rows >= 1 && status.cols >= 1,
            "status PTY stays ≥1×1: {status:?}"
        );
        // Unzoomed, the agent is the narrower split pane.
        let n = effective_layout(false, rows, cols);
        assert!(
            screen_rect(0, status_idx, n).cols < 80,
            "split agent is narrower"
        );
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
        respawn(&mut screens, 0, Path::new("/"), &tx, mux::layout(24, 80));
        assert_eq!(screens.len(), 1, "respawn replaces in place, never adds");
        assert_ne!(
            screens[0].child.id(),
            old_pid,
            "a fresh child replaced the dead one"
        );
    }
}
