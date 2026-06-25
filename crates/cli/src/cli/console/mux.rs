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
    /// Jump to the working agent and enable follow-active mode.
    FollowActive,
    /// Focus the status screen.
    FocusStatus,
    /// Toggle zoom: the active agent fills the whole content area (no
    /// status pane / divider) so native terminal selection copies the
    /// agent cleanly.
    ToggleZoom,
    /// An SGR mouse event the loop must route: forward to the focused
    /// child if it grabbed the mouse, else drive console selection.
    Mouse(MouseEvent),
    /// Tear down every child and exit the console.
    Quit,
    /// A recognized-but-inert command (swallowed, no effect).
    None,
}

/// A decoded SGR mouse event (`CSI < Cb ; Cx ; Cy M|m`). `col`/`row`
/// are 1-based terminal coordinates; `button` is the raw Cb (low 2
/// bits = button, +32 motion, +64 wheel); `pressed` is `M` vs `m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseEvent {
    pub(crate) button: u16,
    pub(crate) col: u16,
    pub(crate) row: u16,
    pub(crate) pressed: bool,
}

/// How a child wants mouse reports encoded — the subset of
/// `vt100::MouseProtocolEncoding` we re-emit when forwarding to an
/// agent that grabbed the mouse. `Sgr` (`CSI < … M|m`) is the modern,
/// lossless form essentially every current TUI requests; `Legacy` is
/// the original `CSI M` + three offset bytes (best-effort: coordinates
/// clamp at 223 and a release collapses to the all-buttons-up code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildMouseEncoding {
    Sgr,
    Legacy,
}

/// Parse one SGR mouse sequence at the front of `bytes` (`CSI < Cb ;
/// Cx ; Cy M|m`). Returns `(consumed, event)`, or `None` if `bytes`
/// doesn't start with a COMPLETE such sequence (so an incomplete one
/// split across reads is forwarded as a plain ESC, never mis-decoded).
fn parse_sgr_mouse(bytes: &[u8]) -> Option<(usize, MouseEvent)> {
    let rest = bytes.strip_prefix(b"\x1b[<")?;
    let mut nums = [0u16; 3];
    let mut field = 0usize;
    let mut cur: u32 = 0;
    let mut saw_digit = false;
    let mut i = 0usize;
    let pressed = loop {
        let &b = rest.get(i)?; // None → incomplete → caller forwards ESC
        match b {
            b'0'..=b'9' => {
                cur = cur * 10 + u32::from(b - b'0');
                if cur > u32::from(u16::MAX) {
                    return None;
                }
                saw_digit = true;
            }
            b';' if field < 2 => {
                nums[field] = cur as u16;
                field += 1;
                cur = 0;
                saw_digit = false;
            }
            b'M' | b'm' if field == 2 && saw_digit => {
                nums[2] = cur as u16;
                i += 1; // consume the terminator
                break b == b'M';
            }
            _ => return None,
        }
        i += 1;
    };
    Some((
        3 + i,
        MouseEvent {
            button: nums[0],
            col: nums[1],
            row: nums[2],
            pressed,
        },
    ))
}

/// Re-encode a mouse event for a child that grabbed the mouse, with
/// `col`/`row` already translated to the child's 1-based pane-relative
/// coordinates. The button byte (including the wheel/motion flags)
/// passes through unchanged — only the coordinates move from terminal
/// space into pane space.
pub(crate) fn encode_mouse_for_child(
    ev: MouseEvent,
    col: u16,
    row: u16,
    enc: ChildMouseEncoding,
) -> Vec<u8> {
    match enc {
        ChildMouseEncoding::Sgr => {
            let term = if ev.pressed { 'M' } else { 'm' };
            format!("\x1b[<{};{};{}{}", ev.button, col, row, term).into_bytes()
        }
        ChildMouseEncoding::Legacy => {
            // `CSI M` then three bytes, each offset by 32. The legacy
            // form can't name the button on a release — it's the
            // all-buttons-up code 3 (the wheel/motion high bits stay).
            // Coordinates clamp at the 223 ceiling the single byte fits.
            let btn = if ev.pressed {
                ev.button
            } else {
                (ev.button & !0b11) | 0b11
            };
            let coord = |v: u16| (v.min(223) as u8).wrapping_add(32);
            vec![
                ESC,
                b'[',
                b'M',
                (btn as u8).wrapping_add(32),
                coord(col),
                coord(row),
            ]
        }
    }
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
                b'0' => Action::FollowActive,
                b'1'..=b'9' => Action::SwitchTo((bytes[0] - b'1') as usize),
                b's' => Action::FocusStatus,
                b'z' => Action::ToggleZoom,
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
            // SGR mouse report (CSI < …) — decode it so the loop can
            // route it (forward to the agent or select); an incomplete
            // one falls through and forwards the ESC.
            if bytes.starts_with(b"\x1b[<") {
                if let Some((consumed, ev)) = parse_sgr_mouse(bytes) {
                    return (consumed, Mode::Passthrough, Action::Mouse(ev));
                }
                return (1, Mode::Passthrough, Action::Forward(vec![ESC]));
            }
            // ESC-prefixed. Only a recognized Meta chord in the SAME
            // burst is a console action; everything else forwards.
            match bytes.get(1) {
                Some(b'a') => (2, Mode::Leader, Action::None),
                Some(b'0') => (2, Mode::Passthrough, Action::FollowActive),
                Some(&b @ b'1'..=b'9') => {
                    (2, Mode::Passthrough, Action::SwitchTo((b - b'1') as usize))
                }
                Some(b's') => (2, Mode::Passthrough, Action::FocusStatus),
                Some(b'z') => (2, Mode::Passthrough, Action::ToggleZoom),
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

/// The index of the primary working agent: the first agent label (in
/// roster order) that's in the `working` set, or `None` if none is.
/// Used by follow-active mode (Alt-0 / a work-state change) to pick
/// which agent the main pane tracks. Pure → unit-tested headless.
pub(crate) fn primary_working(
    agent_labels: &[String],
    working: &std::collections::HashSet<String>,
) -> Option<usize> {
    agent_labels.iter().position(|l| working.contains(l))
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
/// pinned status pane, with a 1-cell divider rule between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The active agent fills this.
    pub(crate) main: Rect,
    /// The rule between the panes: 1 col wide (landscape) or 1 row
    /// tall (portrait), so the status pane's edge is visible.
    pub(crate) divider: Rect,
    /// `clank status --tui`, always visible.
    pub(crate) status: Rect,
    /// True when the split is left/right (status on the right); false
    /// when top/bottom (status on the bottom). Tells render whether
    /// the divider is `│` or `─`.
    pub(crate) landscape: bool,
    /// 0-based row index of the chrome bar (the terminal's last row).
    pub(crate) chrome_row: u16,
}

/// Split the terminal into the agent pane, a divider rule, the
/// always-on status pane, and the chrome row. **Landscape** (wide)
/// puts status on the right; **portrait** (tall) puts it on the
/// bottom. We own the winsize, so this is a pure layout decision — no
/// geometry inference. Sizes are clamped so neither pane collapses on
/// a small terminal.
pub(crate) fn layout(rows: u16, cols: u16) -> Layout {
    let rows = rows.max(2); // ≥1 content row + chrome
    let cols = cols.max(1);
    let content_rows = rows - 1;
    let chrome_row = rows - 1;
    let rect = |x, y, rows, cols| Rect { x, y, rows, cols };

    // Split an axis of length `total` into (main, divider, status) that
    // tile EXACTLY within bounds: a 1-cell divider only when there's
    // room for it plus a status pane, status clamped so main keeps ≥1,
    // and main takes the remainder. On a terminal too small for all
    // three, the divider (then status) drop to 0 — never overflow.
    let split = |total: u16, target: u16| -> (u16, u16, u16) {
        let divider = if total >= 3 { 1 } else { 0 };
        let status = target.min(total.saturating_sub(divider + 1));
        let main = total - divider - status; // ≥1 (status ≤ total-divider-1)
        (main, divider, status)
    };

    if cols >= rows * 2 {
        // Landscape: status on the right, a vertical divider between.
        let (main_cols, divider_cols, status_cols) = split(cols, (cols / 3).clamp(20, 48));
        Layout {
            main: rect(0, 0, content_rows, main_cols),
            divider: rect(main_cols, 0, content_rows, divider_cols),
            status: rect(main_cols + divider_cols, 0, content_rows, status_cols),
            landscape: true,
            chrome_row,
        }
    } else {
        // Portrait: status on the bottom, a horizontal divider between.
        let (main_rows, divider_rows, status_rows) =
            split(content_rows, (content_rows / 3).clamp(6, 16));
        Layout {
            main: rect(0, 0, main_rows, cols),
            divider: rect(0, main_rows, divider_rows, cols),
            status: rect(0, main_rows + divider_rows, status_rows, cols),
            landscape: false,
            chrome_row,
        }
    }
}

/// Zoom layout: the active agent fills the whole content area; the
/// status pane and divider collapse to zero (not drawn) so a native
/// terminal selection of the agent copies cleanly, with nothing
/// spilling in from the side. The chrome bar row is unchanged.
pub(crate) fn zoom_layout(rows: u16, cols: u16) -> Layout {
    let rows = rows.max(2);
    let cols = cols.max(1);
    let content_rows = rows - 1;
    let zero = Rect {
        x: cols,
        y: 0,
        rows: 0,
        cols: 0,
    };
    Layout {
        main: Rect {
            x: 0,
            y: 0,
            rows: content_rows,
            cols,
        },
        divider: zero,
        status: zero,
        landscape: true,
        chrome_row: rows - 1,
    }
}

/// Which pane a coordinate falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Main,
    Status,
}

/// Hit-test a 1-based terminal coordinate against the layout. Returns
/// the pane it lands in plus that pane's 0-based `(row, col)`; `None`
/// for the divider, the chrome row, or anywhere outside a pane. Pure.
pub(crate) fn pane_at(layout: Layout, col1: u16, row1: u16) -> Option<(Pane, u16, u16)> {
    let x = col1.checked_sub(1)?;
    let y = row1.checked_sub(1)?;
    let hit = |r: Rect| {
        r.rows > 0 && r.cols > 0 && x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows
    };
    if hit(layout.main) {
        Some((Pane::Main, y - layout.main.y, x - layout.main.x))
    } else if hit(layout.status) {
        Some((Pane::Status, y - layout.status.y, x - layout.status.x))
    } else {
        None
    }
}

/// Is cell `(row, col)` inside the linewise selection `[start, end]`
/// (reading order, `start ≤ end`)? Pure → unit-tested.
pub(crate) fn in_selection(row: u16, col: u16, start: (u16, u16), end: (u16, u16)) -> bool {
    let ((sr, sc), (er, ec)) = (start, end);
    if row < sr || row > er {
        false
    } else if sr == er {
        col >= sc && col <= ec
    } else if row == sr {
        col >= sc
    } else if row == er {
        col <= ec
    } else {
        true
    }
}

/// Order two cells into `(start, end)` reading order.
pub(crate) fn order_cells(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a <= b { (a, b) } else { (b, a) }
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
/// WORKING is framed in green thin-line edges (`▏…▕`) — a one-row
/// "wire box" rendered outside the band so it stands out even on an
/// unfocused tab (the working agent usually isn't the one you're
/// watching), with breathing room instead of a dot jammed at the
/// number.
///
/// The active agent's tab gets the reverse-video band as the focus
/// cue — but ONLY when an agent is focused. When `status_focused`, no
/// tab bands (the bright divider shows focus is on the status pane).
/// `follow` adds an accent `[follow]` marker on the right so
/// follow-active mode (Alt-0) vs pinned (Alt-digit) is visible.
///
/// Padded to exactly `cols` visible columns. Labels + hint + marks
/// are width-1, so visible width is the char count. Tabs that don't
/// fit are dropped from the right; the right segment is dropped only
/// if it alone wouldn't fit.
pub(crate) fn chrome_line(
    tabs: &[Tab],
    active: usize,
    status_focused: bool,
    follow: bool,
    zoomed: bool,
    hint: &str,
    cols: usize,
) -> String {
    // Right segment: accent mode markers ([follow]/[zoom]) + the hint.
    let mut markers = String::new();
    if follow {
        markers.push_str("[follow] ");
    }
    if zoomed {
        markers.push_str("[zoom] ");
    }
    let right_text_w = markers.chars().count() + hint.chars().count();
    let right_w = if right_text_w > 0 {
        right_text_w + 2
    } else {
        0
    };
    let tab_budget = if right_w > 0 && right_w < cols {
        cols - right_w
    } else {
        cols
    };

    let mut out = String::new();
    let mut used = 0usize;
    for (i, tab) in tabs.iter().enumerate() {
        let mark = if tab.alive { "" } else { " ✗" };
        let body = format!("{}:{}{} ", i + 1, tab.label, mark);
        // visible width = leading separator + body + (green ▏ ▕ edges
        // when working — two extra columns, so the strip reflows for a
        // working tab being wider than an idle one).
        let w = 1 + body.chars().count() + if tab.working { 2 } else { 0 };
        if used + w > tab_budget {
            break;
        }
        out.push(' ');
        // Green left edge of the working "wire box" — outside the band
        // so it shows on any tab.
        if tab.working {
            out.push_str("\x1b[32m▏\x1b[0m");
        }
        // Reverse-video band = focus, but only when an AGENT is
        // focused; when status is focused no tab bands.
        if i == active && !status_focused {
            out.push_str("\x1b[7m");
        } else {
            out.push_str("\x1b[2m");
        }
        out.push_str(&body);
        out.push_str("\x1b[0m");
        // Green right edge.
        if tab.working {
            out.push_str("\x1b[32m▕\x1b[0m");
        }
        used += w;
    }

    if right_w > 0 && tab_budget < cols {
        out.push_str(&" ".repeat(tab_budget - used));
        out.push(' ');
        if !markers.is_empty() {
            out.push_str("\x1b[36m");
            out.push_str(&markers);
            out.push_str("\x1b[0m");
        }
        out.push_str("\x1b[2m");
        out.push_str(hint);
        out.push_str("\x1b[0m ");
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
    fn meta_zero_is_follow_active() {
        // In passthrough and after the Meta-a leader.
        assert_eq!(
            route(Mode::Passthrough, b"\x1b0"),
            (2, Mode::Passthrough, Action::FollowActive)
        );
        assert_eq!(
            route(Mode::Leader, b"0"),
            (1, Mode::Passthrough, Action::FollowActive)
        );
    }

    #[test]
    fn sgr_mouse_parses_press_release_and_big_coords() {
        // Left press at (col 5, row 10).
        assert_eq!(
            route(Mode::Passthrough, b"\x1b[<0;5;10M"),
            (
                10,
                Mode::Passthrough,
                Action::Mouse(MouseEvent {
                    button: 0,
                    col: 5,
                    row: 10,
                    pressed: true
                })
            )
        );
        // Release (m), and coordinates past 223 (the point of SGR mode).
        assert_eq!(
            route(Mode::Passthrough, b"\x1b[<0;500;300m"),
            (
                13,
                Mode::Passthrough,
                Action::Mouse(MouseEvent {
                    button: 0,
                    col: 500,
                    row: 300,
                    pressed: false
                })
            )
        );
        // Drag (motion bit 32) and wheel (bit 64) carry their bits.
        let (_, _, drag) = route(Mode::Passthrough, b"\x1b[<32;5;10M");
        assert!(matches!(drag, Action::Mouse(e) if e.button == 32));
        let (_, _, wheel) = route(Mode::Passthrough, b"\x1b[<64;5;10M");
        assert!(matches!(wheel, Action::Mouse(e) if e.button & 64 != 0));
        // An INCOMPLETE sequence (split across reads) forwards the ESC,
        // never mis-decodes.
        assert_eq!(
            route(Mode::Passthrough, b"\x1b[<0;5"),
            (1, Mode::Passthrough, Action::Forward(vec![0x1b]))
        );
    }

    #[test]
    fn meta_z_toggles_zoom() {
        assert_eq!(
            route(Mode::Passthrough, b"\x1bz"),
            (2, Mode::Passthrough, Action::ToggleZoom)
        );
        assert_eq!(
            route(Mode::Leader, b"z"),
            (1, Mode::Passthrough, Action::ToggleZoom)
        );
    }

    #[test]
    fn pane_at_hit_tests_panes_and_misses_the_divider() {
        let l = layout(24, 80); // landscape: main 0..mc, divider at mc, status after
        // Top-left of main → Main, cell (0,0).
        assert_eq!(pane_at(l, 1, 1), Some((Pane::Main, 0, 0)));
        // First column of the status pane → Status, cell (0,0).
        assert_eq!(pane_at(l, l.status.x + 1, 1), Some((Pane::Status, 0, 0)));
        // The divider column hits nothing.
        assert_eq!(pane_at(l, l.divider.x + 1, 1), None);
        // The chrome row hits nothing.
        assert_eq!(pane_at(l, 1, l.chrome_row + 1), None);
    }

    #[test]
    fn in_selection_is_linewise_reading_order() {
        let start = (1, 3);
        let end = (3, 5);
        assert!(!in_selection(0, 9, start, end)); // above
        assert!(!in_selection(1, 2, start, end)); // first row, before start col
        assert!(in_selection(1, 3, start, end)); // first row, at start
        assert!(in_selection(2, 0, start, end)); // middle row, any col
        assert!(in_selection(3, 5, start, end)); // last row, at end
        assert!(!in_selection(3, 6, start, end)); // last row, past end col
        assert!(!in_selection(4, 0, start, end)); // below
        // Single-line selection.
        assert!(in_selection(1, 4, (1, 3), (1, 6)));
        assert!(!in_selection(1, 7, (1, 3), (1, 6)));
        // order_cells normalizes a backwards drag.
        assert_eq!(order_cells((3, 5), (1, 3)), ((1, 3), (3, 5)));
    }

    #[test]
    fn encode_mouse_for_child_sgr_round_trips_with_pane_coords() {
        // Left press at pane-relative (col 5, row 3): same Cb + M, new
        // coordinates.
        let ev = MouseEvent {
            button: 0,
            col: 40,
            row: 12,
            pressed: true,
        };
        let out = encode_mouse_for_child(ev, 5, 3, ChildMouseEncoding::Sgr);
        assert_eq!(out, b"\x1b[<0;5;3M");
        // Release keeps the button code, flips terminator to `m`.
        let rel = MouseEvent {
            pressed: false,
            ..ev
        };
        assert_eq!(
            encode_mouse_for_child(rel, 5, 3, ChildMouseEncoding::Sgr),
            b"\x1b[<0;5;3m"
        );
        // Wheel-up (Cb 64) passes its high bits through untouched.
        let wheel = MouseEvent { button: 64, ..ev };
        assert_eq!(
            encode_mouse_for_child(wheel, 1, 1, ChildMouseEncoding::Sgr),
            b"\x1b[<64;1;1M"
        );
    }

    #[test]
    fn encode_mouse_for_child_legacy_offsets_by_32_and_collapses_release() {
        // CSI M + three +32 bytes; top-left (1,1) → 33, 33.
        let press = MouseEvent {
            button: 0,
            col: 9,
            row: 9,
            pressed: true,
        };
        assert_eq!(
            encode_mouse_for_child(press, 1, 1, ChildMouseEncoding::Legacy),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release can't name its button in the legacy form — it's the
        // all-buttons-up code 3 (byte 35), coords still offset.
        let rel = MouseEvent {
            pressed: false,
            ..press
        };
        assert_eq!(
            encode_mouse_for_child(rel, 1, 1, ChildMouseEncoding::Legacy),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
    }

    #[test]
    fn zoom_layout_fills_with_agent_and_collapses_status() {
        let l = zoom_layout(24, 80);
        // Active agent fills the whole content area.
        assert_eq!(
            l.main,
            Rect {
                x: 0,
                y: 0,
                rows: 23,
                cols: 80
            }
        );
        // Status + divider collapse to nothing (not drawn).
        assert_eq!((l.status.rows, l.status.cols), (0, 0));
        assert_eq!((l.divider.rows, l.divider.cols), (0, 0));
        // Chrome row unchanged.
        assert_eq!(l.chrome_row, 23);
    }

    #[test]
    fn primary_working_is_first_in_roster_order() {
        let labels = vec![
            "claude".to_string(),
            "codex".to_string(),
            "ruthless".to_string(),
        ];
        let set = |ls: &[&str]| ls.iter().map(|s| s.to_string()).collect();
        // First roster agent that's working wins (codex before ruthless).
        assert_eq!(
            primary_working(&labels, &set(&["ruthless", "codex"])),
            Some(1)
        );
        assert_eq!(primary_working(&labels, &set(&["claude"])), Some(0));
        // Nobody working → None.
        assert_eq!(primary_working(&labels, &set(&[])), None);
        // A working label that isn't an agent (shouldn't happen) → None.
        assert_eq!(primary_working(&labels, &set(&["ghost"])), None);
    }

    #[test]
    fn meta_digit_and_meta_s_switch_directly() {
        for (bytes, want) in [
            (b"\x1b0".as_slice(), Action::FollowActive),
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
        // An unmapped Alt chord (ESC x) likewise forwards the ESC.
        assert_eq!(
            route(Mode::Passthrough, b"\x1bx"),
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
    fn layout_landscape_puts_status_on_the_right_with_a_divider() {
        let l = layout(24, 80);
        assert!(l.landscape);
        assert_eq!(l.chrome_row, 23);
        // main │ status tile the width with a 1-col divider between;
        // all three full content height.
        assert_eq!((l.main.x, l.main.y, l.main.rows), (0, 0, 23));
        assert_eq!(l.divider.cols, 1);
        assert_eq!((l.divider.x, l.divider.rows), (l.main.cols, 23));
        assert_eq!(l.status.x, l.main.cols + 1);
        assert_eq!(l.status.rows, 23);
        assert_eq!(l.main.cols + 1 + l.status.cols, 80, "tile exactly");
        assert!(l.status.cols >= 20 && l.status.cols <= 48);
    }

    #[test]
    fn layout_tiles_within_bounds_for_any_size() {
        // term_size can return any positive size; the regions must tile
        // EXACTLY within the content area (no overflow past the chrome
        // row / terminal edge), main always ≥1, and the divider/status
        // drop to 0 only when there's genuinely no room.
        for (r, c) in [
            (2, 2),
            (3, 3),
            (2, 80),
            (4, 6),
            (3, 100),
            (24, 80),
            (50, 40),
        ] {
            let l = layout(r, c);
            let content_rows = r.max(2) - 1;
            let eff_cols = c.max(1);
            assert!(l.main.rows >= 1 && l.main.cols >= 1, "main {r}x{c}: {l:?}");
            if l.landscape {
                assert_eq!(
                    l.main.cols + l.divider.cols + l.status.cols,
                    eff_cols,
                    "cols tile {r}x{c}: {l:?}"
                );
                assert!(l.status.x + l.status.cols <= eff_cols, "in bounds {r}x{c}");
                assert_eq!(l.main.rows, content_rows);
            } else {
                assert_eq!(
                    l.main.rows + l.divider.rows + l.status.rows,
                    content_rows,
                    "rows tile {r}x{c}: {l:?}"
                );
                assert!(
                    l.status.y + l.status.rows <= content_rows,
                    "in bounds {r}x{c}"
                );
                assert_eq!(l.main.cols, eff_cols);
            }
        }
    }

    #[test]
    fn layout_portrait_puts_status_on_the_bottom_with_a_divider() {
        // Tall + narrow → portrait.
        let l = layout(50, 40);
        assert!(!l.landscape);
        // main / divider / status stacked, all full width.
        assert_eq!((l.main.x, l.main.y, l.main.cols), (0, 0, 40));
        assert_eq!(l.divider.rows, 1);
        assert_eq!((l.divider.y, l.divider.cols), (l.main.rows, 40));
        assert_eq!(l.status.y, l.main.rows + 1);
        assert_eq!(l.status.cols, 40);
        assert_eq!(l.main.rows + 1 + l.status.rows, 49, "content rows tile");
        assert_eq!(l.chrome_row, 49);
    }

    #[test]
    fn chrome_line_is_exactly_cols_wide_with_an_active_band() {
        let tabs = vec![
            tab("claude (master)", true, false),
            tab("codex", true, false),
            tab("status", true, false),
        ];
        let line = chrome_line(&tabs, 1, false, false, false, "", 80);
        assert_eq!(strip(&line).chars().count(), 80, "padded to full width");
        // The focused tab's body is wrapped in reverse-video; inactive
        // in dim. (The leading separator space is outside the band.)
        assert!(line.contains("\x1b[7m2:codex \x1b[0m"));
        assert!(line.contains("\x1b[2m1:claude (master) \x1b[0m"));
    }

    #[test]
    fn chrome_line_frames_the_working_agent_in_green() {
        let tabs = vec![tab("claude", true, true), tab("codex", true, false)];
        let line = chrome_line(&tabs, 1, false, false, false, "", 80);
        // The working (unfocused) tab is framed in green thin-line
        // edges, outside the dim styling so they stay visible.
        assert!(
            line.contains("\x1b[32m▏\x1b[0m"),
            "green left edge: {line:?}"
        );
        assert!(
            line.contains("\x1b[32m▕\x1b[0m"),
            "green right edge: {line:?}"
        );
        // Edges hug the tab body (not jammed at the number).
        assert!(strip(&line).contains("▏1:claude ▕"));
        // The wider working tab still leaves the bar exactly cols wide.
        assert_eq!(strip(&line).chars().count(), 80);
    }

    #[test]
    fn chrome_line_bands_no_agent_when_status_focused() {
        let tabs = vec![tab("claude", true, false), tab("codex", true, false)];
        // Main focused on agent 0 → it bands.
        let main = chrome_line(&tabs, 0, false, false, false, "", 80);
        assert!(main.contains("\x1b[7m1:claude "));
        // Status focused → no reverse-video band on any agent tab.
        let status = chrome_line(&tabs, 0, true, false, false, "", 80);
        assert!(
            !status.contains("\x1b[7m"),
            "no tab bands when status is focused: {status:?}"
        );
    }

    #[test]
    fn chrome_line_shows_follow_marker() {
        let tabs = vec![tab("claude", true, false)];
        let off = chrome_line(&tabs, 0, false, false, false, "h", 80);
        assert!(!strip(&off).contains("[follow]"));
        let on = chrome_line(&tabs, 0, false, true, false, "h", 80);
        assert!(
            strip(&on).contains("[follow]"),
            "follow marker shown: {:?}",
            strip(&on)
        );
        assert_eq!(strip(&on).chars().count(), 80);
        // Zoom marker is independent of follow.
        let zoom = chrome_line(&tabs, 0, false, false, true, "h", 80);
        assert!(strip(&zoom).contains("[zoom]"), "{:?}", strip(&zoom));
        let both = chrome_line(&tabs, 0, false, true, true, "h", 80);
        let p = strip(&both);
        assert!(p.contains("[follow]") && p.contains("[zoom]"));
        assert_eq!(p.chars().count(), 80, "both markers still full width");
    }

    #[test]
    fn chrome_line_shows_a_right_aligned_hint() {
        let tabs = vec![tab("claude", true, false)];
        let line = chrome_line(&tabs, 0, false, false, false, "M-s status", 80);
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
        let line = chrome_line(&tabs, 0, false, false, false, "", 40);
        assert!(strip(&line).contains("1:codex ✗"));
    }

    #[test]
    fn chrome_line_drops_tabs_that_dont_fit() {
        let tabs = vec![tab("aaaaaaaa", true, false), tab("bbbbbbbb", true, false)];
        // Width 14 fits only the first " 1:aaaaaaaa " (12 cols).
        let line = chrome_line(&tabs, 0, false, false, false, "", 14);
        let plain = strip(&line);
        assert!(plain.contains("1:aaaaaaaa"));
        assert!(!plain.contains("2:bbbbbbbb"));
        assert_eq!(plain.chars().count(), 14);
    }
}
