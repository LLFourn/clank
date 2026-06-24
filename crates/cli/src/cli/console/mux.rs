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
/// child; the prefix byte is the ONE exception, arming `Prefix` so
/// the next byte is read as a console command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Passthrough,
    Prefix,
}

/// What a routed byte means. The loop performs these; [`route`]
/// never touches a terminal or a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Write these bytes to the active child's PTY verbatim.
    Forward(Vec<u8>),
    /// Make screen `n` (0-based) active, if it exists.
    SwitchTo(usize),
    /// Next / previous screen (wraps).
    Next,
    Prev,
    /// Tear down every child and exit the console.
    Quit,
    /// A recognized-but-inert prefix command (swallowed, no effect).
    None,
}

/// `Ctrl-a`. A single rare control byte that no multi-byte key
/// escape sequence contains, so byte-at-a-time prefix detection
/// never splits an arrow key or a paste. Settled empirically against
/// what claude/codex actually bind in their own editors, and
/// overridable via config because both Ctrl-a and Ctrl-b collide
/// with *something*.
pub(crate) const DEFAULT_PREFIX: u8 = 0x01;

/// Route one stdin byte. Returns the next mode and the action it
/// implies. A single-byte prefix is what keeps this byte-oriented
/// and exhaustively testable.
pub(crate) fn route(mode: Mode, byte: u8, prefix: u8) -> (Mode, Action) {
    match mode {
        Mode::Passthrough => {
            if byte == prefix {
                (Mode::Prefix, Action::None)
            } else {
                (Mode::Passthrough, Action::Forward(vec![byte]))
            }
        }
        Mode::Prefix => {
            let action = match byte {
                // Prefix-prefix sends ONE literal prefix to the child
                // (the tmux convention for typing the prefix itself).
                b if b == prefix => Action::Forward(vec![prefix]),
                b'1'..=b'9' => Action::SwitchTo((byte - b'1') as usize),
                b'n' | b'\t' => Action::Next,
                b'p' => Action::Prev,
                b'q' => Action::Quit,
                _ => Action::None,
            };
            (Mode::Passthrough, action)
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

/// (rows, cols) available to the active child: the terminal minus
/// the one-row chrome bar at the bottom. Always ≥1 row so a child
/// never gets a zero-height PTY.
pub(crate) fn content_rect(rows: u16, cols: u16) -> (u16, u16) {
    (rows.saturating_sub(1).max(1), cols.max(1))
}

/// One tab in the chrome bar.
pub(crate) struct Tab {
    pub(crate) label: String,
    pub(crate) alive: bool,
}

/// The bottom chrome bar: a numbered tab strip, active screen in
/// reverse video, the rest dim, a dead child marked `✗`. Padded to
/// exactly `cols` visible columns. Labels are ASCII (agent label +
/// role + "status") and the mark is width-1, so visible width is the
/// char count. Tabs that don't fit are dropped from the right.
pub(crate) fn chrome_line(tabs: &[Tab], active: usize, cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for (i, tab) in tabs.iter().enumerate() {
        let mark = if tab.alive { "" } else { " ✗" };
        let seg = format!(" {}:{}{} ", i + 1, tab.label, mark);
        let w = seg.chars().count();
        if used + w > cols {
            break;
        }
        // Active = reverse video; others dim. The band is the focus
        // cue, consistent with the status panel's `emit_selected`.
        if i == active {
            out.push_str("\x1b[7m");
        } else {
            out.push_str("\x1b[2m");
        }
        out.push_str(&seg);
        out.push_str("\x1b[0m");
        used += w;
    }
    if used < cols {
        out.push_str(&" ".repeat(cols - used));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PFX: u8 = DEFAULT_PREFIX;

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

    #[test]
    fn passthrough_forwards_non_prefix_bytes() {
        assert_eq!(
            route(Mode::Passthrough, b'x', PFX),
            (Mode::Passthrough, Action::Forward(vec![b'x']))
        );
        // An arrow key arrives as three bytes; none is the prefix, so
        // each forwards verbatim and the mode never flips.
        for b in [0x1b, b'[', b'A'] {
            assert_eq!(
                route(Mode::Passthrough, b, PFX),
                (Mode::Passthrough, Action::Forward(vec![b]))
            );
        }
    }

    #[test]
    fn prefix_arms_then_resolves_in_one_step() {
        let (m, a) = route(Mode::Passthrough, PFX, PFX);
        assert_eq!((m, a), (Mode::Prefix, Action::None));
        // Every command resolves back to Passthrough.
        for (byte, want) in [
            (b'1', Action::SwitchTo(0)),
            (b'9', Action::SwitchTo(8)),
            (b'n', Action::Next),
            (b'\t', Action::Next),
            (b'p', Action::Prev),
            (b'q', Action::Quit),
            (b'Z', Action::None),
        ] {
            assert_eq!(route(Mode::Prefix, byte, PFX), (Mode::Passthrough, want));
        }
    }

    #[test]
    fn prefix_prefix_sends_one_literal_prefix() {
        assert_eq!(
            route(Mode::Prefix, PFX, PFX),
            (Mode::Passthrough, Action::Forward(vec![PFX]))
        );
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
    fn content_rect_reserves_the_chrome_row() {
        assert_eq!(content_rect(24, 80), (23, 80));
        // Never zero-height / zero-width.
        assert_eq!(content_rect(1, 0), (1, 1));
        assert_eq!(content_rect(0, 0), (1, 1));
    }

    #[test]
    fn chrome_line_is_exactly_cols_wide_with_an_active_band() {
        let tabs = vec![
            Tab { label: "claude (master)".into(), alive: true },
            Tab { label: "codex".into(), alive: true },
            Tab { label: "status".into(), alive: true },
        ];
        let line = chrome_line(&tabs, 1, 80);
        assert_eq!(strip(&line).chars().count(), 80, "padded to full width");
        // The active tab is wrapped in reverse-video; inactive in dim.
        assert!(line.contains("\x1b[7m 2:codex \x1b[0m"));
        assert!(line.contains("\x1b[2m 1:claude (master) \x1b[0m"));
    }

    #[test]
    fn chrome_line_marks_dead_children() {
        let tabs = vec![Tab { label: "codex".into(), alive: false }];
        let line = chrome_line(&tabs, 0, 40);
        assert!(strip(&line).contains("1:codex ✗"));
    }

    #[test]
    fn chrome_line_drops_tabs_that_dont_fit() {
        let tabs = vec![
            Tab { label: "aaaaaaaa".into(), alive: true },
            Tab { label: "bbbbbbbb".into(), alive: true },
        ];
        // Width 14 fits only the first " 1:aaaaaaaa " (12 cols).
        let line = chrome_line(&tabs, 0, 14);
        let plain = strip(&line);
        assert!(plain.contains("1:aaaaaaaa"));
        assert!(!plain.contains("2:bbbbbbbb"));
        assert_eq!(plain.chars().count(), 14);
    }
}
