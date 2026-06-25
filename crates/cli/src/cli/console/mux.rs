//! The console's pure input-routing + layout core. No IO, no
//! terminal, no PTYs — every function here is a total mapping from
//! (state, input) to an intent, unit-tested headless. The loop in
//! [`super`] is the only place that performs the resulting actions.
//!
//! This mirrors the `status_tui` discipline: one `Mode` enum owns
//! routing, each byte is resolved in exactly one place per mode, and
//! pure functions return [`Action`]s the loop executes.

/// Which layer owns the next stdin byte. The console is a thin
/// pass-through: in `Passthrough` every byte goes to the active
/// child, EXCEPT Meta (Alt) chords. `Meta-a` is the leader, arming
/// `Leader` so the next byte is read as a console command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Passthrough,
    Leader,
}

/// What routed input means. The loop performs these; [`route`] never
/// touches a terminal or a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Write these bytes to the active child's PTY verbatim.
    Forward(Vec<u8>),
    /// Make screen `n` (0-based) active, if it exists.
    SwitchTo(usize),
    /// Next / previous screen (wraps).
    Next,
    Prev,
    /// Focus the status screen.
    FocusStatus,
    /// Tear down every child and exit the console.
    Quit,
    /// A recognized-but-inert command (swallowed, no effect).
    None,
}

const ESC: u8 = 0x1b;
const CTRL_BACKSLASH: u8 = 0x1c;

/// Route the front of a stdin burst. Returns how many bytes it
/// consumed (always ≥1), the next mode, and the action. The loop
/// calls this repeatedly across a `read()` chunk.
///
/// Meta (Alt) chords arrive ESC-prefixed in a single `read()` —
/// `Meta-a` is `ESC a`, `Meta-1` is `ESC 1`. The crux is
/// disambiguation by READ GRANULARITY (no timer): a real Alt chord
/// has its byte in the same burst as the `ESC`, while a bare Escape
/// arrives as a lone `ESC`. Anything `ESC`-prefixed we don't
/// recognize as a console chord — `ESC [`/`ESC O` (arrows, SS3), a
/// lone `ESC` (the Escape key), an unmapped `Alt-x` — is FORWARDED,
/// never swallowed, so the child still gets its escape sequences.
/// (Known limitation: over SSH/mosh a chord can fragment across two
/// reads; then the lone `ESC` is forwarded as Escape and the chord
/// silently doesn't register — safe, never corrupting.)
pub(crate) fn route(mode: Mode, bytes: &[u8]) -> (usize, Mode, Action) {
    debug_assert!(!bytes.is_empty());
    match mode {
        Mode::Leader => {
            // The byte after `Meta-a`: a console command (or cancel).
            let action = match bytes[0] {
                b'q' => Action::Quit,
                b'n' | b'\t' => Action::Next,
                b'p' => Action::Prev,
                b'1'..=b'9' => Action::SwitchTo((bytes[0] - b'1') as usize),
                b's' => Action::FocusStatus,
                _ => Action::None,
            };
            (1, Mode::Passthrough, action)
        }
        Mode::Passthrough => {
            // Ctrl-\ is a meta-independent emergency quit, so you're
            // never stuck if the terminal doesn't send Alt as Meta
            // (then no Meta chord — including Meta-a q — would fire).
            if bytes[0] == CTRL_BACKSLASH {
                return (1, Mode::Passthrough, Action::Quit);
            }
            if bytes[0] != ESC {
                return (1, Mode::Passthrough, Action::Forward(vec![bytes[0]]));
            }
            // ESC-prefixed. Only a recognized Meta chord in the SAME
            // burst is a console action; everything else forwards.
            match bytes.get(1) {
                Some(b'a') => (2, Mode::Leader, Action::None),
                Some(&b @ b'1'..=b'9') => {
                    (2, Mode::Passthrough, Action::SwitchTo((b - b'1') as usize))
                }
                Some(b's') => (2, Mode::Passthrough, Action::FocusStatus),
                // Lone ESC, or ESC + anything else (CSI/SS3/unmapped
                // Alt): forward just the ESC; the rest routes as plain
                // bytes, reconstructing the full sequence at the child.
                _ => (1, Mode::Passthrough, Action::Forward(vec![ESC])),
            }
        }
    }
}

/// The active index after a navigation action, or `None` if it
/// doesn't move: an out-of-range or same-index `SwitchTo`, no
/// screens, or a non-nav action. Pure so wraparound + clamping are
/// tested headless.
pub(crate) fn next_active(active: usize, count: usize, action: &Action) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match action {
        Action::SwitchTo(i) if *i < count && *i != active => Some(*i),
        Action::Next => Some((active + 1) % count),
        Action::Prev => Some((active + count - 1) % count),
        _ => None,
    }
}

/// Resolve an agent-nav action (`SwitchTo`/`Next`/`Prev`) against the
/// current `(active, status_focused)`, returning the new pair iff
/// anything changes. The key rule (the plan's "any Meta-digit returns
/// focus to an agent"): a nav ALWAYS returns focus to the agent pane,
/// even when it targets the already-active agent — so `Meta-<current>`
/// un-focuses the status pane in a single-agent console too. Non-nav
/// actions return `None`. Pure → unit-tested headless.
pub(crate) fn resolve_nav(
    active: usize,
    count: usize,
    status_focused: bool,
    action: &Action,
) -> Option<(usize, bool)> {
    if !matches!(action, Action::SwitchTo(_) | Action::Next | Action::Prev) {
        return None;
    }
    let new_active = next_active(active, count, action).unwrap_or(active);
    if new_active == active && !status_focused {
        return None; // already there, nothing to change
    }
    Some((new_active, false))
}

/// A sub-rectangle of the terminal, 0-based origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

/// How the content area is split between the active agent and the
/// pinned status pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The active agent fills this.
    pub(crate) main: Rect,
    /// `clank status --tui`, always visible.
    pub(crate) status: Rect,
    /// 0-based row index of the chrome bar (the terminal's last row).
    pub(crate) chrome_row: u16,
}

/// Split the terminal into the agent pane, the always-on status pane,
/// and the chrome row. **Landscape** (wide) puts status on the right;
/// **portrait** (tall) puts it on the bottom. We own the winsize, so
/// this is a pure layout decision — no geometry inference. Sizes are
/// clamped so neither pane collapses on a small terminal.
pub(crate) fn layout(rows: u16, cols: u16) -> Layout {
    let rows = rows.max(2); // ≥1 content row + chrome
    let cols = cols.max(1);
    let content_rows = rows - 1;
    let chrome_row = rows - 1;

    if cols >= rows * 2 {
        // Landscape: status on the right.
        let status_cols = (cols / 3).clamp(20, 48).min(cols.saturating_sub(20).max(1));
        let main_cols = cols - status_cols;
        Layout {
            main: Rect {
                x: 0,
                y: 0,
                rows: content_rows,
                cols: main_cols,
            },
            status: Rect {
                x: main_cols,
                y: 0,
                rows: content_rows,
                cols: status_cols,
            },
            chrome_row,
        }
    } else {
        // Portrait: status on the bottom.
        let status_rows = (content_rows / 3)
            .clamp(6, 16)
            .min(content_rows.saturating_sub(3).max(1));
        let main_rows = content_rows - status_rows;
        Layout {
            main: Rect {
                x: 0,
                y: 0,
                rows: main_rows,
                cols,
            },
            status: Rect {
                x: 0,
                y: main_rows,
                rows: status_rows,
                cols,
            },
            chrome_row,
        }
    }
}

/// One tab in the chrome bar.
pub(crate) struct Tab {
    pub(crate) label: String,
    pub(crate) alive: bool,
    /// The agent clank currently expects to act (whose turn it is).
    /// Marked with a `●` so you can see who's working at a glance —
    /// independent of which tab you're focused on.
    pub(crate) working: bool,
}

/// The bottom chrome bar: a numbered tab strip on the left and a dim
/// right-aligned keybinding hint so the prefix is discoverable — the
/// console is meant to be a gentle entrypoint, and a new user must be
/// able to find "how do I switch / quit" without a manual.
///
/// Per tab: the focused one is reverse-video (where you're looking),
/// the rest dim; a dead child is marked `✗`; and the agent currently
/// WORKING gets a green `●` just left of its number — rendered
/// outside the dim styling so it stands out even on an unfocused tab
/// (the working agent usually isn't the one you're watching).
///
/// Padded to exactly `cols` visible columns. Labels + hint + marks
/// are width-1, so visible width is the char count. Tabs that don't
/// fit are dropped from the right; the hint is dropped only if it
/// alone wouldn't fit.
pub(crate) fn chrome_line(tabs: &[Tab], active: usize, hint: &str, cols: usize) -> String {
    let hint_seg = if hint.is_empty() {
        String::new()
    } else {
        format!(" {hint} ")
    };
    let hint_w = hint_seg.chars().count();
    // Reserve the hint on the right only if it fits with room to spare
    // for at least part of a tab; otherwise give tabs the full width.
    let tab_budget = if hint_w > 0 && hint_w < cols {
        cols - hint_w
    } else {
        cols
    };

    let mut out = String::new();
    let mut used = 0usize;
    for (i, tab) in tabs.iter().enumerate() {
        let mark = if tab.alive { "" } else { " ✗" };
        let body = format!("{}:{}{} ", i + 1, tab.label, mark);
        // visible width = leading separator + optional ● + body
        let w = 1 + usize::from(tab.working) + body.chars().count();
        if used + w > tab_budget {
            break;
        }
        out.push(' ');
        if tab.working {
            // Green ● OUTSIDE the dim/reverse styling so it pops on
            // any tab, focused or not.
            out.push_str("\x1b[32m●\x1b[0m");
        }
        // Focused = reverse video; others dim. The band is the focus
        // cue, consistent with the status panel's `emit_selected`.
        if i == active {
            out.push_str("\x1b[7m");
        } else {
            out.push_str("\x1b[2m");
        }
        out.push_str(&body);
        out.push_str("\x1b[0m");
        used += w;
    }

    if tab_budget < cols {
        // Pad the gap, then the dim hint, flush right.
        out.push_str(&" ".repeat(tab_budget - used));
        out.push_str("\x1b[2m");
        out.push_str(&hint_seg);
        out.push_str("\x1b[0m");
        used = cols;
    }
    if used < cols {
        out.push_str(&" ".repeat(cols - used));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR escapes (`\x1b[…m`) so a styled line can be measured
    /// and read as plain text.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // skip "[ … m"
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn tab(label: &str, alive: bool, working: bool) -> Tab {
        Tab {
            label: label.into(),
            alive,
            working,
        }
    }

    #[test]
    fn plain_bytes_forward_one_at_a_time() {
        let (n, m, a) = route(Mode::Passthrough, b"x");
        assert_eq!(
            (n, m, a),
            (1, Mode::Passthrough, Action::Forward(vec![b'x']))
        );
    }

    #[test]
    fn ctrl_backslash_always_quits() {
        // Meta-independent emergency exit (works without Option-as-Meta).
        assert_eq!(
            route(Mode::Passthrough, b"\x1c"),
            (1, Mode::Passthrough, Action::Quit)
        );
    }

    #[test]
    fn meta_digit_and_meta_s_switch_directly() {
        for (bytes, want) in [
            (b"\x1b1".as_slice(), Action::SwitchTo(0)),
            (b"\x1b9".as_slice(), Action::SwitchTo(8)),
            (b"\x1bs".as_slice(), Action::FocusStatus),
        ] {
            let (n, m, a) = route(Mode::Passthrough, bytes);
            assert_eq!((n, m), (2, Mode::Passthrough), "{bytes:?}");
            assert_eq!(a, want, "{bytes:?}");
        }
    }

    #[test]
    fn meta_a_arms_the_leader_then_resolves() {
        // Meta-a (ESC a) → Leader, consuming both bytes.
        assert_eq!(
            route(Mode::Passthrough, b"\x1ba"),
            (2, Mode::Leader, Action::None)
        );
        // The following byte is the command.
        for (byte, want) in [
            (b'q', Action::Quit),
            (b'n', Action::Next),
            (b'\t', Action::Next),
            (b'p', Action::Prev),
            (b'3', Action::SwitchTo(2)),
            (b's', Action::FocusStatus),
            (b'Z', Action::None),
        ] {
            assert_eq!(
                route(Mode::Leader, &[byte]),
                (1, Mode::Passthrough, want),
                "leader+{}",
                byte as char
            );
        }
    }

    #[test]
    fn esc_sequences_are_forwarded_never_swallowed() {
        // A lone ESC at the end of a burst = the Escape key.
        assert_eq!(
            route(Mode::Passthrough, b"\x1b"),
            (1, Mode::Passthrough, Action::Forward(vec![0x1b]))
        );
        // ESC [ A (arrow) and ESC O P (SS3) forward the ESC; the rest
        // routes as plain bytes, reconstructing the sequence.
        for seq in [b"\x1b[A".as_slice(), b"\x1bOP".as_slice()] {
            let (n, m, a) = route(Mode::Passthrough, seq);
            assert_eq!(
                (n, m, a),
                (1, Mode::Passthrough, Action::Forward(vec![0x1b])),
                "{seq:?}"
            );
        }
        // An unmapped Alt chord (ESC z) likewise forwards the ESC.
        assert_eq!(
            route(Mode::Passthrough, b"\x1bz"),
            (1, Mode::Passthrough, Action::Forward(vec![0x1b]))
        );
    }

    #[test]
    fn resolve_nav_always_returns_focus_to_the_agent() {
        // Meta-<current agent> while status-focused un-focuses status,
        // even though the active index doesn't change (the codex bug:
        // single-agent consoles were stuck on status).
        assert_eq!(
            resolve_nav(0, 1, true, &Action::SwitchTo(0)),
            Some((0, false))
        );
        // Same key when NOT status-focused is a genuine no-op.
        assert_eq!(resolve_nav(0, 1, false, &Action::SwitchTo(0)), None);
        // Switching to a different agent always lands on it, unfocused.
        assert_eq!(
            resolve_nav(0, 3, true, &Action::SwitchTo(2)),
            Some((2, false))
        );
        assert_eq!(resolve_nav(0, 3, false, &Action::Next), Some((1, false)));
        // Non-nav actions never resolve.
        assert_eq!(resolve_nav(0, 3, true, &Action::FocusStatus), None);
        assert_eq!(resolve_nav(0, 3, true, &Action::None), None);
    }

    #[test]
    fn next_active_wraps_and_clamps() {
        assert_eq!(next_active(0, 3, &Action::Next), Some(1));
        assert_eq!(next_active(2, 3, &Action::Next), Some(0));
        assert_eq!(next_active(0, 3, &Action::Prev), Some(2));
        assert_eq!(next_active(1, 3, &Action::SwitchTo(2)), Some(2));
        // Out-of-range and same-index switches don't move.
        assert_eq!(next_active(1, 3, &Action::SwitchTo(9)), None);
        assert_eq!(next_active(2, 3, &Action::SwitchTo(2)), None);
        // No screens, or a non-nav action, never moves.
        assert_eq!(next_active(0, 0, &Action::Next), None);
        assert_eq!(next_active(0, 3, &Action::Forward(vec![b'x'])), None);
    }

    #[test]
    fn layout_landscape_puts_status_on_the_right() {
        let l = layout(24, 80);
        assert_eq!(l.chrome_row, 23);
        // main + status tile the full width; both full content height.
        assert_eq!(l.main.x, 0);
        assert_eq!(l.main.y, 0);
        assert_eq!(l.main.rows, 23);
        assert_eq!(l.status.y, 0);
        assert_eq!(l.status.rows, 23);
        assert_eq!(l.status.x, l.main.cols);
        assert_eq!(l.main.cols + l.status.cols, 80);
        assert!(l.status.cols >= 20 && l.status.cols <= 48);
    }

    #[test]
    fn layout_portrait_puts_status_on_the_bottom() {
        // Tall + narrow → portrait.
        let l = layout(50, 40);
        // main on top, status below it, stacked, both full width.
        assert_eq!(l.main.x, 0);
        assert_eq!(l.main.y, 0);
        assert_eq!(l.main.cols, 40);
        assert_eq!(l.status.x, 0);
        assert_eq!(l.status.cols, 40);
        assert_eq!(l.status.y, l.main.rows);
        assert_eq!(l.main.rows + l.status.rows, 49); // content rows
        assert_eq!(l.chrome_row, 49);
    }

    #[test]
    fn chrome_line_is_exactly_cols_wide_with_an_active_band() {
        let tabs = vec![
            tab("claude (master)", true, false),
            tab("codex", true, false),
            tab("status", true, false),
        ];
        let line = chrome_line(&tabs, 1, "", 80);
        assert_eq!(strip(&line).chars().count(), 80, "padded to full width");
        // The focused tab's body is wrapped in reverse-video; inactive
        // in dim. (The leading separator space is outside the band.)
        assert!(line.contains("\x1b[7m2:codex \x1b[0m"));
        assert!(line.contains("\x1b[2m1:claude (master) \x1b[0m"));
    }

    #[test]
    fn chrome_line_marks_the_working_agent() {
        let tabs = vec![tab("claude", true, true), tab("codex", true, false)];
        let line = chrome_line(&tabs, 1, "", 80);
        // A green ● sits left of the working (but unfocused) tab.
        assert!(
            line.contains("\x1b[32m●\x1b[0m"),
            "working agent gets a green dot: {line:?}"
        );
        // The dot is rendered OUTSIDE the dim styling so it stays
        // visible on an unfocused tab.
        assert!(strip(&line).contains("●1:claude"));
        assert_eq!(strip(&line).chars().count(), 80);
    }

    #[test]
    fn chrome_line_shows_a_right_aligned_hint() {
        let tabs = vec![tab("claude", true, false)];
        let line = chrome_line(&tabs, 0, "M-s status", 80);
        let plain = strip(&line);
        assert_eq!(plain.chars().count(), 80);
        assert!(plain.contains("1:claude"));
        // The hint is flush right.
        assert!(
            plain.ends_with("M-s status "),
            "hint at the right edge: {plain:?}"
        );
    }

    #[test]
    fn chrome_line_marks_dead_children() {
        let tabs = vec![tab("codex", false, false)];
        let line = chrome_line(&tabs, 0, "", 40);
        assert!(strip(&line).contains("1:codex ✗"));
    }

    #[test]
    fn chrome_line_drops_tabs_that_dont_fit() {
        let tabs = vec![tab("aaaaaaaa", true, false), tab("bbbbbbbb", true, false)];
        // Width 14 fits only the first " 1:aaaaaaaa " (12 cols).
        let line = chrome_line(&tabs, 0, "", 14);
        let plain = strip(&line);
        assert!(plain.contains("1:aaaaaaaa"));
        assert!(!plain.contains("2:bbbbbbbb"));
        assert_eq!(plain.chars().count(), 14);
    }
}
