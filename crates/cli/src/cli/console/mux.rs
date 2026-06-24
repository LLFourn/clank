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

/// A printable label for the prefix byte (`0x01` → `^A`), for the
/// chrome hint. Falls back to hex for a non-control prefix.
pub(crate) fn prefix_label(prefix: u8) -> String {
    if (1..=26).contains(&prefix) {
        format!("^{}", (b'A' + prefix - 1) as char)
    } else {
        format!("0x{prefix:02x}")
    }
}

/// Parse a human-written prefix spec into its control byte. Accepts
/// `C-a` / `c-a` / `ctrl-a` / `^a` (case-insensitive letter) for a
/// control char, or a single printable ASCII char taken literally.
/// `None` on anything else, so a bad override falls back to the
/// default rather than wedging input on an unreachable prefix. The
/// override knob for the experimental console is the
/// `CLANK_CONSOLE_PREFIX` env var (a persistent config key is a
/// follow-up once the default is settled).
pub(crate) fn parse_prefix(spec: &str) -> Option<u8> {
    let spec = spec.trim();
    let ctrl_of = |rest: &str| -> Option<u8> {
        let mut chars = rest.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None; // exactly one letter after the modifier
        }
        let lc = c.to_ascii_lowercase();
        lc.is_ascii_lowercase().then(|| (lc as u8) - b'a' + 1)
    };
    for modi in ["ctrl-", "Ctrl-", "C-", "c-", "^"] {
        if let Some(rest) = spec.strip_prefix(modi) {
            return ctrl_of(rest);
        }
    }
    // A single printable ASCII char, taken literally.
    let mut chars = spec.chars();
    let c = chars.next()?;
    if chars.next().is_none() && c.is_ascii() && !c.is_ascii_control() {
        return Some(c as u8);
    }
    None
}

/// The bottom chrome bar: a numbered tab strip on the left (active in
/// reverse video, the rest dim, a dead child marked `✗`) and a dim
/// right-aligned keybinding hint so the prefix is discoverable — the
/// console is meant to be a gentle entrypoint, and a new user must be
/// able to find "how do I switch / quit" without a manual. Padded to
/// exactly `cols` visible columns. Labels + hint are ASCII (the `✗`
/// and `·` are width-1), so visible width is the char count. Tabs
/// that don't fit are dropped from the right; the hint is dropped
/// only if it alone wouldn't fit.
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
        let seg = format!(" {}:{}{} ", i + 1, tab.label, mark);
        let w = seg.chars().count();
        if used + w > tab_budget {
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
            Tab {
                label: "claude (master)".into(),
                alive: true,
            },
            Tab {
                label: "codex".into(),
                alive: true,
            },
            Tab {
                label: "status".into(),
                alive: true,
            },
        ];
        let line = chrome_line(&tabs, 1, "", 80);
        assert_eq!(strip(&line).chars().count(), 80, "padded to full width");
        // The active tab is wrapped in reverse-video; inactive in dim.
        assert!(line.contains("\x1b[7m 2:codex \x1b[0m"));
        assert!(line.contains("\x1b[2m 1:claude (master) \x1b[0m"));
    }

    #[test]
    fn chrome_line_shows_a_right_aligned_hint() {
        let tabs = vec![Tab {
            label: "claude".into(),
            alive: true,
        }];
        let line = chrome_line(&tabs, 0, "^A n·p·q", 80);
        let plain = strip(&line);
        assert_eq!(plain.chars().count(), 80);
        assert!(plain.contains("1:claude"));
        // The hint is flush right.
        assert!(
            plain.ends_with("^A n·p·q "),
            "hint at the right edge: {plain:?}"
        );
    }

    #[test]
    fn prefix_label_renders_control_bytes() {
        assert_eq!(prefix_label(0x01), "^A");
        assert_eq!(prefix_label(0x02), "^B");
    }

    #[test]
    fn parse_prefix_accepts_control_forms_and_literals() {
        for spec in ["C-a", "c-a", "ctrl-a", "Ctrl-A", "^a", "^A", " C-a "] {
            assert_eq!(parse_prefix(spec), Some(0x01), "{spec:?}");
        }
        assert_eq!(parse_prefix("C-b"), Some(0x02));
        assert_eq!(parse_prefix("ctrl-z"), Some(0x1a));
        // A single printable char is taken literally.
        assert_eq!(parse_prefix("`"), Some(b'`'));
        // Garbage falls back (None → caller keeps the default).
        assert_eq!(parse_prefix(""), None);
        assert_eq!(parse_prefix("C-ab"), None);
        assert_eq!(parse_prefix("C-1"), None);
        assert_eq!(parse_prefix("hello"), None);
    }

    #[test]
    fn chrome_line_marks_dead_children() {
        let tabs = vec![Tab {
            label: "codex".into(),
            alive: false,
        }];
        let line = chrome_line(&tabs, 0, "", 40);
        assert!(strip(&line).contains("1:codex ✗"));
    }

    #[test]
    fn chrome_line_drops_tabs_that_dont_fit() {
        let tabs = vec![
            Tab {
                label: "aaaaaaaa".into(),
                alive: true,
            },
            Tab {
                label: "bbbbbbbb".into(),
                alive: true,
            },
        ];
        // Width 14 fits only the first " 1:aaaaaaaa " (12 cols).
        let line = chrome_line(&tabs, 0, "", 14);
        let plain = strip(&line);
        assert!(plain.contains("1:aaaaaaaa"));
        assert!(!plain.contains("2:bbbbbbbb"));
        assert_eq!(plain.chars().count(), 14);
    }
}
