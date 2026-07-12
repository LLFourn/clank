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

/// Display columns a char occupies in the terminal. Not a full
/// unicode-width implementation: rendered content is validated
/// ASCII (plan stems, agent labels, gate names) plus the fixed
/// status emoji set — so "emoji plane → 2, else 1" is exact for
/// everything we draw, with zero new deps.
pub(super) fn char_width(c: char) -> usize {
    if ('\u{1F000}'..='\u{1FAFF}').contains(&c) {
        2
    } else {
        1
    }
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
}
