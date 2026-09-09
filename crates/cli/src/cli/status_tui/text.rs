//! Text/cell primitives for the status TUI: the styled-[`Span`] model,
//! display-width math (emoji = 2 cols), width-aware wrapping +
//! truncation, and the span→ANSI serializers. Pure, no terminal IO —
//! body lines are built as styled spans, truncated by display width at
//! the span level, and only then serialized to ANSI so an escape is
//! never split.

#[derive(Clone, PartialEq)]
pub(super) enum Style {
    Plain,
    Dim,
    /// State-colored (the frame's one hue) — used sparingly for
    /// the line that demands action (a pending block's question).
    Accent,
    /// Fixed ANSI color (the log tier's verdict ticks — green ✓ /
    /// cyan ✓✓ / red ✗; lloyd asked for the marks to pop).
    Color(&'static str),
    /// An OSC 8 terminal hyperlink wrapping the visible text; the
    /// String is the (dynamic) target URL — which is why `Style`
    /// isn't `Copy`. `emit` owns the escape, so the width math
    /// counts only the visible text, and the target stays the full
    /// URL even when that text truncates on a narrow pane.
    Link(String),
    /// Background-highlighted (the commit-log plan umbrellas): a
    /// fixed bold + dark-grey-background SGR so a plan name reads as
    /// a section divider (tui-log-plan-highlight-align).
    Highlight,
    /// Italic — the master "working…" in-progress row
    /// (status-timeline-progress).
    Italic,
    /// Bold — markdown headings + `**strong**` in the plan-doc overlay.
    Bold,
}

#[derive(Clone)]
pub(super) struct Span(pub(super) Style, pub(super) String);

pub(super) fn dim(s: impl Into<String>) -> Span {
    Span(Style::Dim, s.into())
}
pub(super) fn plain(s: impl Into<String>) -> Span {
    Span(Style::Plain, s.into())
}
pub(super) fn accent(s: impl Into<String>) -> Span {
    Span(Style::Accent, s.into())
}
pub(super) fn italic(s: impl Into<String>) -> Span {
    Span(Style::Italic, s.into())
}
pub(super) fn bold(s: impl Into<String>) -> Span {
    Span(Style::Bold, s.into())
}
pub(super) fn highlight(s: impl Into<String>) -> Span {
    Span(Style::Highlight, s.into())
}
/// A fixed-ANSI-color span — `code` is the SGR (e.g. `"32"` green).
/// Named `colored`, not `color`, to avoid clashing with the ubiquitous
/// `color` state-hue local in the renderers.
pub(super) fn colored(code: &'static str, s: impl Into<String>) -> Span {
    Span(Style::Color(code), s.into())
}
/// An OSC 8 hyperlink span: `url` is the (dynamic) target, `text` the
/// visible label.
pub(super) fn link(url: String, text: impl Into<String>) -> Span {
    Span(Style::Link(url), text.into())
}

/// Right-aligned label gutter: 5 columns + 2 spaces, dim. The
/// fixed gutter is what makes the cluster read as one organized
/// instrument instead of stacked key:value dumps.
pub(super) fn label(name: &str) -> Span {
    dim(format!("{name:>5}  "))
}

/// Wrap `text` to `width` display columns, breaking on whitespace and
/// hard-breaking a single token wider than `width` (a long id/URL
/// can't overflow). `width == 0` degrades to one line per
/// `\n`-segment (no panic).
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.split('\n').map(str::to_string).collect();
    }
    let mut out = Vec::new();
    for segment in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in segment.split_whitespace() {
            let ww = display_width(word);
            let sep = usize::from(!line.is_empty());
            if line_w + sep + ww <= width {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                line_w += sep + ww;
                continue;
            }
            // Doesn't fit: flush the current line first.
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if ww <= width {
                line.push_str(word);
                line_w = ww;
            } else {
                // Token wider than the whole line — hard-break it.
                for ch in word.chars() {
                    let cw = char_width(ch);
                    if line_w + cw > width && !line.is_empty() {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(ch);
                    line_w += cw;
                }
            }
        }
        // Trailing line; for an empty/whitespace-only segment this
        // preserves the intentional blank line.
        out.push(line);
    }
    out
}

/// Serialize spans to one ANSI line, truncated to `cols` display
/// columns with `…`. Truncation happens on the PLAIN text span by
/// span — an escape can never be split, and a dropped span drops
/// its styling with it.
pub(super) fn emit(spans: &[Span], color: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for Span(style, text) in spans {
        let remaining = cols.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let piece = truncate_to(text, remaining);
        used += display_width(&piece);
        let truncated_here = piece.ends_with('…') && !text.ends_with('…');
        match style {
            Style::Plain => out.push_str(&piece),
            Style::Dim => out.push_str(&format!("\x1b[2m{piece}\x1b[0m")),
            Style::Accent => out.push_str(&format!("\x1b[{color}m{piece}\x1b[0m")),
            Style::Color(c) => out.push_str(&format!("\x1b[{c}m{piece}\x1b[0m")),
            // OSC 8 hyperlink: ESC ] 8 ; ; <url> ST <text> ESC ] 8 ; ; ST
            Style::Link(url) => out.push_str(&format!("\x1b]8;;{url}\x1b\\{piece}\x1b]8;;\x1b\\")),
            Style::Highlight => out.push_str(&format!("\x1b[1;48;5;238m{piece}\x1b[0m")),
            Style::Italic => out.push_str(&format!("\x1b[3m{piece}\x1b[0m")),
            Style::Bold => out.push_str(&format!("\x1b[1m{piece}\x1b[0m")),
        }
        if truncated_here {
            break;
        }
    }
    out
}

/// The unified "selected" indicator: render a row as one solid
/// full-width reverse-video band, padding to `cols` so the WHOLE line
/// reads as selected. Per-span styling is dropped — the band is the
/// emphasis — and the width is display-aware (no ragged edge on a wide
/// glyph or trailing pad). One selection style, drawn one way, for
/// agent / "+ add" / picker rows alike.
pub(super) fn emit_selected(spans: &[Span], cols: usize) -> String {
    let text: String = spans.iter().map(|Span(_, t)| t.as_str()).collect();
    let text = truncate_to(&text, cols);
    let pad = cols.saturating_sub(display_width(&text));
    format!("\x1b[7m{text}{}\x1b[0m", " ".repeat(pad))
}

/// Emit a panel row: the unified selection band when it's the cursor
/// row, else normal per-span styling.
pub(super) fn row_line(spans: &[Span], selected: bool, color: &str, cols: usize) -> String {
    if selected {
        emit_selected(spans, cols)
    } else {
        emit(spans, color, cols)
    }
}

/// Collapse a possibly-multiline / control-laden string to ONE display
/// line: first line only, control chars dropped, truncated to `cols`.
/// Every line a renderer emits must be a single terminal row — a raw
/// `initial_prompt` with newlines would otherwise inject extra rows and
/// break the clamp.
pub(super) fn one_line(s: &str, cols: usize) -> String {
    let first: String = s
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    truncate_to(&first, cols)
}

/// A section-divider rule (`── TITLE ─────`), dimmed. When `focused`
/// the rule also carries the active-key hint (a quiet aid, not a
/// highlight).
pub(super) fn region_rule(title: &str, hint: &str, focused: bool, cols: usize) -> String {
    region_rule_with_note(title, "", hint, focused, cols)
}

/// [`region_rule`] with an always-shown, case-preserved note after the
/// title (`── LOG · master ────`) — the title is uppercased branding,
/// the note is DATA (a branch name) and must keep its case
/// (tui-gauges-declutter).
pub(super) fn region_rule_with_note(
    title: &str,
    note: &str,
    hint: &str,
    focused: bool,
    cols: usize,
) -> String {
    let head = rule_head(title, note, hint, focused, cols);
    let head = truncate_to(&head, cols);
    let fill = cols.saturating_sub(display_width(&head));
    format!("\x1b[2m{head}{}\x1b[0m", "─".repeat(fill))
}

/// Compose `── TITLE · note · hint ` with the FOCUSED HINT reserved
/// first: the note is DATA of arbitrary length (a branch name) and is
/// fitted into whatever width remains, so it can never truncate the
/// hint away — the focused and unfocused rules must stay visually
/// distinct at any width (codex bef2d5c).
fn rule_head(title: &str, note: &str, hint: &str, focused: bool, cols: usize) -> String {
    let title_part = format!("── {} ", title.to_uppercase());
    let hint_part = if focused && !hint.is_empty() {
        format!("· {hint} ")
    } else {
        String::new()
    };
    let mut head = title_part;
    if !note.is_empty() {
        let reserved = display_width(&head) + display_width(&hint_part);
        let room = cols.saturating_sub(reserved);
        // "· x " needs 5 columns to say anything; below that the note
        // is dropped whole rather than shown as pure ellipsis.
        if room >= 5 {
            let fitted = truncate_to(note, room - 4);
            head.push_str(&format!("· {fitted} "));
        }
    }
    head.push_str(&hint_part);
    head
}

/// The "lift on scroll" variant of [`region_rule`] (Material app-bar
/// elevation): the same rule rendered on a RAISED surface — dark-grey
/// background across the full width, title at full brightness instead
/// of dim. The caller renders the flat rule while its content is at
/// the top and this one the moment entries scroll UNDER the bar — the
/// bar itself is the scroll-state signal (no text to read, no color
/// that already carries a meaning: dim = secondary, accent = state).
pub(super) fn region_rule_elevated(title: &str, hint: &str, focused: bool, cols: usize) -> String {
    region_rule_elevated_with_note(title, "", hint, focused, cols)
}

/// The elevated variant of [`region_rule_with_note`].
pub(super) fn region_rule_elevated_with_note(
    title: &str,
    note: &str,
    hint: &str,
    focused: bool,
    cols: usize,
) -> String {
    let head = rule_head(title, note, hint, focused, cols);
    let head = truncate_to(&head, cols);
    let fill = cols.saturating_sub(display_width(&head));
    format!("[48;5;238m{head}{}[0m", "─".repeat(fill))
}

/// The East-Asian Wide and Fullwidth code points — the ones a terminal
/// draws in two cells. Generated from the Unicode Character Database
/// (UCD 16.0.0) rather than hand-listed, so it is complete: an earlier
/// hand-built version covered the emoji plane only and measured `⌛`
/// (U+231B) as one column, and a later one still stopped at U+1FAFF and
/// so missed U+20000 (codex on 5f271a6).
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F),
    (0x231A, 0x231B),
    (0x2329, 0x232A),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2630, 0x2637),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x268A, 0x268F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x2E80, 0x2E99),
    (0x2E9B, 0x2EF3),
    (0x2F00, 0x2FD5),
    (0x2FF0, 0x303E),
    (0x3041, 0x3096),
    (0x3099, 0x30FF),
    (0x3105, 0x312F),
    (0x3131, 0x318E),
    (0x3190, 0x31E5),
    (0x31EF, 0x321E),
    (0x3220, 0x3247),
    (0x3250, 0xA48C),
    (0xA490, 0xA4C6),
    (0xA960, 0xA97C),
    (0xAC00, 0xD7A3),
    (0xF900, 0xFAFF),
    (0xFE10, 0xFE19),
    (0xFE30, 0xFE52),
    (0xFE54, 0xFE66),
    (0xFE68, 0xFE6B),
    (0xFF01, 0xFF60),
    (0xFFE0, 0xFFE6),
    (0x16FE0, 0x16FE4),
    (0x16FF0, 0x16FF1),
    (0x17000, 0x187F7),
    (0x18800, 0x18CD5),
    (0x18CFF, 0x18D08),
    (0x1AFF0, 0x1AFF3),
    (0x1AFF5, 0x1AFFB),
    (0x1AFFD, 0x1AFFE),
    (0x1B000, 0x1B122),
    (0x1B132, 0x1B132),
    (0x1B150, 0x1B152),
    (0x1B155, 0x1B155),
    (0x1B164, 0x1B167),
    (0x1B170, 0x1B2FB),
    (0x1D300, 0x1D356),
    (0x1D360, 0x1D376),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F200, 0x1F202),
    (0x1F210, 0x1F23B),
    (0x1F240, 0x1F248),
    (0x1F250, 0x1F251),
    (0x1F260, 0x1F265),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6DF),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA89),
    (0x1FA8F, 0x1FAC6),
    (0x1FACE, 0x1FADC),
    (0x1FADF, 0x1FAE9),
    (0x1FAF0, 0x1FAF8),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
];

/// Display columns a char occupies in the terminal.
///
/// Complete for East-Asian Wide/Fullwidth via [`WIDE`], with no
/// dependency. It does NOT implement zero-width combining marks, which
/// this TUI does not draw — with one deliberate exception: U+FE0F is
/// counted as 1. The selector occupies no cell itself, but it upgrades
/// the preceding character to emoji presentation, which the terminal
/// then draws in two cells instead of one; counting it is how that
/// second cell is accounted for without clustering graphemes.
pub(super) fn char_width(c: char) -> usize {
    let cp = c as u32;
    let hit = WIDE.binary_search_by(|&(lo, hi)| {
        if cp < lo {
            std::cmp::Ordering::Greater
        } else if cp > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    if hit.is_ok() { 2 } else { 1 }
}

pub(super) fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncate to `cols` DISPLAY columns with a `…` ellipsis.
/// Width-aware (emoji count as 2) so a kept double-width char
/// can't push the line past the pane edge.
pub(super) fn truncate_to(s: &str, cols: usize) -> String {
    if display_width(s) <= cols {
        return s.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    // Keep chars while they fit in cols-1 (reserving 1 for `…`).
    let mut t = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > cols - 1 {
            break;
        }
        t.push(c);
        w += cw;
    }
    t.push('…');
    t
}

/// Strip a leading signal-lamp emoji (`"👀 frostsnap"` → `"frostsnap"`)
/// so a prior, un-restored indicator doesn't stack. A lamp glyph is a
/// single emoji-plane grapheme followed by a space. `pub(crate)` because
/// the zellij tab/pane renamer in `open_zellij` strips the same prefix.
pub(crate) fn strip_leading_emoji(name: &str) -> String {
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        // A lamp glyph (emoji-plane, width 2) followed by a space.
        (Some(first), Some(' ')) if char_width(first) == 2 => chars.as_str().to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_leading_emoji_removes_a_stale_glyph_only() {
        // A prior, un-restored indicator must not stack.
        assert_eq!(strip_leading_emoji("👀 frostsnap"), "frostsnap");
        assert_eq!(strip_leading_emoji("💤 clank"), "clank");
        // A plain name is untouched.
        assert_eq!(strip_leading_emoji("clank"), "clank");
        // A name that merely starts with a word (no emoji) is untouched.
        assert_eq!(strip_leading_emoji("pr-497"), "pr-497");
    }

    #[test]
    fn wrap_breaks_on_words_newlines_and_long_tokens() {
        // Word boundaries.
        assert_eq!(wrap("a b c d", 3), vec!["a b", "c d"]);
        // Explicit newlines preserved as hard breaks.
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        // Blank line between segments preserved.
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        // A token wider than the line hard-breaks.
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        // Display-width aware: each 🔨 is 2 cols, so width 2 fits one per line.
        assert_eq!(wrap("🔨🔨", 2), vec!["🔨", "🔨"]);
        // width 0 degrades to one line per newline-segment, no panic.
        assert_eq!(wrap("a b\nc", 0), vec!["a b", "c"]);
    }

    #[test]
    fn region_rule_elevated_is_the_same_bar_on_a_raised_surface() {
        let flat = region_rule("log", "hint", true, 40);
        let lifted = region_rule_elevated("log", "hint", true, 40);
        // Same visible content (title + hint + fill to the same width)…
        let strip = |s: &str| {
            let mut out = String::new();
            let mut ch = s.chars();
            while let Some(c) = ch.next() {
                if c == '\x1b' {
                    for e in ch.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(strip(&flat), strip(&lifted), "same bar, different surface");
        // …but the lifted bar carries the raised-surface background and
        // drops the dim (title at full brightness), while the flat rule
        // is dim with no background.
        assert!(lifted.contains("\x1b[48;5;238m"), "raised surface bg");
        assert!(!lifted.contains("\x1b[2m"), "lifted title not dim");
        assert!(flat.contains("\x1b[2m") && !flat.contains("48;5;238"));
    }

    /// A representative sample of the non-ASCII characters the TUI
    /// draws, each hand-verified against East-Asian width.
    ///
    /// NOT a complete list, and nothing keeps it complete: the scanner
    /// that once did was deleted with the incomplete lookup it existed
    /// to compensate for, so a newly drawn glyph will simply be absent
    /// from here. That costs nothing, because completeness lives in
    /// [`WIDE`] — which is generated from the UCD and covers every code
    /// point — and this is a regression pin for the row that broke
    /// (ruthless on 07c5e04).
    const DRAWN: &[(char, usize)] = &[
        ('\u{00B7}', 1),  // MIDDLE DOT
        ('\u{2014}', 1),  // EM DASH
        ('\u{2022}', 1),  // BULLET
        ('\u{2026}', 1),  // HORIZONTAL ELLIPSIS
        ('\u{2039}', 1),  // SINGLE LEFT-POINTING ANGLE QUOTATION MARK
        ('\u{203A}', 1),  // SINGLE RIGHT-POINTING ANGLE QUOTATION MARK
        ('\u{2190}', 1),  // LEFTWARDS ARROW
        ('\u{2191}', 1),  // UPWARDS ARROW
        ('\u{2192}', 1),  // RIGHTWARDS ARROW
        ('\u{2193}', 1),  // DOWNWARDS ARROW
        ('\u{21C4}', 1),  // RIGHTWARDS ARROW OVER LEFTWARDS ARROW
        ('\u{21E7}', 1),  // UPWARDS WHITE ARROW
        ('\u{2212}', 1),  // MINUS SIGN
        ('\u{231B}', 2),  // HOURGLASS
        ('\u{23CE}', 1),  // RETURN SYMBOL
        ('\u{23F8}', 1),  // DOUBLE VERTICAL BAR
        ('\u{2423}', 1),  // OPEN BOX
        ('\u{2500}', 1),  // BOX DRAWINGS LIGHT HORIZONTAL
        ('\u{2502}', 1),  // BOX DRAWINGS LIGHT VERTICAL
        ('\u{25B6}', 1),  // BLACK RIGHT-POINTING TRIANGLE
        ('\u{25B8}', 1),  // BLACK RIGHT-POINTING SMALL TRIANGLE
        ('\u{26A0}', 1),  // WARNING SIGN
        ('\u{2717}', 1),  // BALLOT X
        ('\u{280B}', 1),  // BRAILLE PATTERN DOTS-124
        ('\u{2819}', 1),  // BRAILLE PATTERN DOTS-145
        ('\u{2826}', 1),  // BRAILLE PATTERN DOTS-236
        ('\u{2827}', 1),  // BRAILLE PATTERN DOTS-1236
        ('\u{2834}', 1),  // BRAILLE PATTERN DOTS-356
        ('\u{2838}', 1),  // BRAILLE PATTERN DOTS-456
        ('\u{2839}', 1),  // BRAILLE PATTERN DOTS-1456
        ('\u{283C}', 1),  // BRAILLE PATTERN DOTS-3456
        ('\u{FE0F}', 1),  // VARIATION SELECTOR-16 — see `char_width`
        ('\u{1F3C1}', 2), // CHEQUERED FLAG
        ('\u{1F440}', 2), // EYES
        ('\u{1F4A4}', 2), // SLEEPING SYMBOL
        ('\u{1F4CB}', 2), // CLIPBOARD
        ('\u{1F500}', 2), // TWISTED RIGHTWARDS ARROWS
        ('\u{1F50D}', 2), // LEFT-POINTING MAGNIFYING GLASS
        ('\u{1F528}', 2), // HAMMER
        ('\u{1F64B}', 2), // HAPPY PERSON RAISING ONE HAND
    ];

    #[test]
    fn every_drawn_glyph_is_measured() {
        let wrong: Vec<String> = DRAWN
            .iter()
            .filter(|(c, w)| char_width(*c) != *w)
            .map(|(c, w)| {
                format!(
                    "U+{:04X} {c} is {w} column(s), measured {}",
                    *c as u32,
                    char_width(*c)
                )
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "char_width mis-measures glyph(s) the TUI draws, so every row \
             containing one is off by that many columns:\n  {}",
            wrong.join("\n  ")
        );
    }

    /// The class, not just the glyph that broke. Each of these is a
    /// character the old lookups got wrong or nearly did: `⌛` sat
    /// outside the emoji plane, and U+20000 sits outside the BMP
    /// ranges that replaced it. Narrow lookalikes are pinned too — a
    /// table that widens `✗` or `→` breaks every row that draws one.
    #[test]
    fn the_wide_class_is_complete_beyond_the_emoji_plane() {
        for (c, want) in [
            ('\u{231B}', 2),  // ⌛ HOURGLASS — the original regression
            ('\u{20000}', 2), // CJK Ext B, plane 2
            ('\u{30000}', 2), // CJK Ext G, plane 3
            ('\u{4E00}', 2),  // 一 CJK unified
            ('\u{AC00}', 2),  // 가 Hangul syllable
            ('\u{FF21}', 2),  // Ａ fullwidth
            ('\u{3000}', 2),  // ideographic space
            ('\u{1F600}', 2), // emoji plane
            ('\u{2717}', 1),  // ✗ — narrow despite the company it keeps
            ('\u{2714}', 1),  // ✔
            ('\u{2192}', 1),  // →
            ('\u{FE0F}', 1),  // selector: see `char_width`
            ('a', 1),
            ('\u{0}', 1),
            ('\u{10FFFF}', 1), // last code point, must not panic
        ] {
            assert_eq!(
                char_width(c),
                want,
                "U+{:04X} should be {want} column(s)",
                c as u32
            );
        }
    }

    /// The ranges must stay sorted and disjoint: `char_width` binary
    /// searches them, and an out-of-order or overlapping entry would
    /// make it silently miss code points.
    #[test]
    fn the_wide_ranges_are_sorted_and_disjoint() {
        for w in WIDE.windows(2) {
            let (a, b) = (w[0], w[1]);
            assert!(a.0 <= a.1, "range U+{:04X}..U+{:04X} is inverted", a.0, a.1);
            assert!(
                a.1 < b.0,
                "U+{:04X}..U+{:04X} overlaps or precedes U+{:04X}..U+{:04X}",
                a.0,
                a.1,
                b.0,
                b.1
            );
        }
    }
}
