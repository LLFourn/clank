//! Text/cell primitives for the status TUI: the styled-[`Span`] model,
//! display-width math (emoji = 2 cols), width-aware wrapping +
//! truncation, and the span→ANSI serializers. Pure, no terminal IO —
//! body lines are built as styled spans, truncated by display width at
//! the span level, and only then serialized to ANSI so an escape is
//! never split.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
                for grapheme in word.graphemes(true) {
                    let cw = display_width(grapheme);
                    if line_w + cw > width && !line.is_empty() {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push_str(grapheme);
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
/// columns with `…`. The cut is decided ONCE over the plain text of
/// the whole row and then handed out across the spans — an escape can
/// never be split, and a dropped span drops its styling with it.
///
/// The marker belongs to the row, so it is drawn outside every span's
/// escapes and in the default colour, where it used to inherit the
/// styling of whichever span it ended.
pub(super) fn emit(spans: &[Span], color: &str, cols: usize) -> String {
    // ONE cut for the whole row, then distributed back over the
    // spans. Truncating span by span let a span that exactly filled
    // the width end the row with no ellipsis while a later span was
    // dropped entirely -- so a row said something different depending
    // on whether the cursor was on it, `emit_selected` taking the
    // whole line and this one not (a-row-truncates-the-same-way-
    // selected-or-not).
    //
    // Distributed, never re-cut: a second per-span truncation could
    // land inside a grapheme the whole-row cut deliberately kept
    // together, such as a variation selector and its base.
    let whole: String = spans.iter().map(|Span(_, t)| t.as_str()).collect();
    let cut = cut_to(&whole, cols);
    let mut rest = cut.kept.as_str();
    let mut out = String::new();
    for Span(style, text) in spans {
        if rest.is_empty() {
            break;
        }
        // By CHARS: `kept` is a prefix of the concatenation, so each
        // span's share is its own length until the prefix runs out.
        let n = text.chars().count().min(rest.chars().count());
        let end = rest.char_indices().nth(n).map_or(rest.len(), |(i, _)| i);
        let (piece, tail) = rest.split_at(end);
        let piece = piece.to_string();
        rest = tail;
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
    }
    // The marker is the ROW's, not any span's: it says the row was
    // cut, and it is owed exactly when the whole-row cut dropped
    // something -- never because a span's own text happens to end in
    // one.
    if cut.marks(cols) {
        out.push('…');
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
pub(crate) fn one_line(s: &str, cols: usize) -> String {
    let first: String = strip_escapes(s)
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    truncate_to(&first, cols)
}

/// `s` without its escape sequences — CSI (`ESC [ … final`), OSC
/// (`ESC ] … BEL` or `ESC \`) and a lone `ESC x` — whole, so a
/// coloured diagnostic loses its colour and not just its ESC, which
/// left `[31m` in the text (codex on 5fd8a69).
pub(super) fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            _ => {}
        }
    }
    out
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

/// Width of an isolated scalar. Complete graphemes are measured by
/// `display_width`, because selectors, combining marks and joined emoji
/// cannot be measured by summing scalar widths.
pub(super) fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Terminal columns, using the same grapheme widths as wrapping and cuts.
pub(crate) fn display_width(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Truncate to `cols` DISPLAY columns with a `…` ellipsis.
/// Width-aware (emoji count as 2) so a kept double-width char
/// can't push the line past the pane edge.
pub(super) fn truncate_to(s: &str, cols: usize) -> String {
    let cut = cut_to(s, cols);
    if cut.marks(cols) {
        format!("{}…", cut.kept)
    } else {
        cut.kept
    }
}

/// What survives a cut to `cols`, and WHETHER anything was dropped.
///
/// `dropped` is carried rather than inferred from a trailing `…`: the
/// text may legitimately end in one, and a row that fits must keep its
/// own ellipsis without being read as truncated (codex 3d2e4f9).
///
/// A column is reserved for the marker whenever a cut happens, so the
/// caller can append it without overflowing.
pub(super) struct Cut {
    pub(super) kept: String,
    pub(super) dropped: bool,
}

impl Cut {
    /// Whether a marker is owed AND has room to be drawn.
    ///
    /// Two different questions: at zero columns content was still
    /// dropped, but there is nowhere to say so, and a `…` there would
    /// overflow a budget of nothing. Asked through one method so the
    /// two callers cannot answer it differently -- which is how the
    /// paths came to disagree in the first place.
    pub(super) fn marks(&self, cols: usize) -> bool {
        self.dropped && cols > 0
    }
}

pub(super) fn cut_to(s: &str, cols: usize) -> Cut {
    let fits = |kept: String, dropped: bool| Cut { kept, dropped };
    if display_width(s) <= cols {
        return fits(s.to_string(), false);
    }
    if cols == 0 {
        return fits(String::new(), true);
    }
    // Keep whole graphemes, reserving one column for the ellipsis.
    let mut t = String::new();
    let mut w = 0;
    for grapheme in s.graphemes(true) {
        let cw = display_width(grapheme);
        if w + cw > cols - 1 {
            break;
        }
        t.push_str(grapheme);
        w += cw;
    }
    fits(t, true)
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
    /// A row says the same thing whether or not the cursor is on it.
    ///
    /// `emit` used to truncate span by span, so a span that exactly
    /// filled the width ended the row with NO ellipsis while a later
    /// span was dropped whole -- and `emit_selected`, which cuts the
    /// concatenation, added one. Moving the cursor onto a row changed
    /// what the row said, and the unselected form was the misleading
    /// one: `ab` looks complete, `a…` admits it is not.
    #[test]
    fn a_row_says_the_same_thing_selected_or_not() {
        use super::Style;
        let rows: Vec<Vec<super::Span>> = vec![
            // The demonstrated case: span 0 exactly fills 2 columns.
            vec![
                super::Span(Style::Plain, "ab".into()),
                super::Span(Style::Dim, "cdef".into()),
            ],
            // A wide glyph on the boundary.
            vec![
                super::Span(Style::Plain, "\u{1F4A4}".into()),
                super::Span(Style::Plain, " idle".into()),
            ],
            // A variation-selector pair, which must not be split by
            // the distribution either.
            vec![
                super::Span(Style::Plain, "\u{2601}\u{FE0F}".into()),
                super::Span(Style::Plain, " remote".into()),
            ],
            // An empty span in the middle, and a single one.
            vec![
                super::Span(Style::Plain, "ab".into()),
                super::Span(Style::Dim, String::new()),
                super::Span(Style::Bold, "cd".into()),
            ],
            vec![super::Span(Style::Plain, "abcdef".into())],
        ];
        for spans in &rows {
            let whole: String = spans.iter().map(|s| s.1.clone()).collect();
            for cols in 0..=super::display_width(&whole) + 2 {
                let dim = strip(&super::emit(spans, "32", cols));
                // The band pads to the full width ON PURPOSE; only the
                // text has to agree.
                let sel = strip(&super::emit_selected(spans, cols))
                    .trim_end()
                    .to_string();
                assert_eq!(
                    dim.trim_end(),
                    sel,
                    "{whole:?} at {cols} cols: unselected {dim:?} vs selected {sel:?}",
                );
                assert!(
                    super::display_width(dim.trim_end()) <= cols,
                    "{whole:?} at {cols} cols overflows: {dim:?}",
                );
            }
        }
    }

    /// Each span's style lands on its OWN characters after the cut.
    ///
    /// The distribution hands out the whole-row prefix, and a mistake
    /// there moves the style boundary without changing a visible
    /// character -- so the parity test, which strips ANSI, cannot see
    /// it. Asserted on the escapes.
    #[test]
    fn the_cut_moves_no_characters_between_styles() {
        use super::Style;
        let spans = vec![
            super::Span(Style::Plain, "\u{1F4A4}".into()),
            super::Span(Style::Dim, " idle".into()),
        ];
        // Width 7, cut to 5: a column goes to the marker, so the kept
        // prefix is `💤 i` -- which falls INSIDE the second span.
        assert_eq!(
            super::emit(&spans, "32", 5),
            "\u{1F4A4}\x1b[2m i\x1b[0m…",
            "the wide glyph is span 0's and the space is span 1's",
        );
        // Counting the glyph's WIDTH rather than its length would give
        // span 0 the space as well, moving the dim escape one right
        // with every visible character unchanged.
        assert_eq!(
            super::display_width(&strip(&super::emit(&spans, "32", 5))),
            5
        );
    }

    /// A row that FITS keeps a literal ellipsis of its own.
    ///
    /// The old check inferred "was truncated" from a trailing `…`,
    /// which a row can legitimately contain (codex 3d2e4f9).
    #[test]
    fn a_fitting_row_keeps_an_ellipsis_it_already_had() {
        use super::Style;
        let spans = vec![
            super::Span(Style::Plain, "wait…".into()),
            super::Span(Style::Dim, " more".into()),
        ];
        let whole = "wait… more";
        assert_eq!(
            strip(&super::emit(&spans, "32", 10)),
            whole,
            "it fits whole"
        );
        assert_eq!(super::truncate_to(whole, 10), whole);
        // And one that IS cut still gets its marker, even though the
        // kept text already ends in one. Two ellipses read oddly, but
        // suppressing the second would make a truncated row look
        // complete -- which is the bug this whole plan is about, and
        // the reason `dropped` is carried rather than sniffed from
        // the last character.
        let cut = strip(&super::emit(&spans, "32", 6));
        assert_eq!(cut, "wait……");
        assert_eq!(
            super::display_width(&cut),
            6,
            "and it fills the row exactly"
        );

        // The case that distinguishes carrying `dropped` from sniffing
        // the text: a row whose WHOLE content ends in an ellipsis, and
        // which is cut. Sniffing says "it already ends in one" and
        // leaves the row looking complete.
        let ends_in_one = vec![
            super::Span(Style::Plain, "wait".into()),
            super::Span(Style::Dim, " more…".into()),
        ];
        assert!(
            "wait more…".ends_with('…'),
            "the fixture must really end in one",
        );
        let cut = strip(&super::emit(&ends_in_one, "32", 6));
        assert!(
            cut.ends_with('…') && super::display_width(&cut) == 6,
            "a cut row must say so even when its text ends in an ellipsis: {cut:?}",
        );
        assert_ne!(cut, "wait m", "which is what sniffing the text produces");
    }

    /// A cut inside a link keeps the link whole around what is left.
    #[test]
    fn a_cut_link_keeps_its_target_and_closes_itself() {
        use super::Style;
        let url = "https://example.invalid/a/very/long/target";
        let spans = vec![
            super::Span(Style::Link(url.to_string()), "a long label".into()),
            super::Span(Style::Plain, " after".into()),
        ];
        let drawn = super::emit(&spans, "32", 6);
        assert!(
            drawn.contains(url),
            "the target is not the thing being clipped"
        );
        assert_eq!(
            drawn.matches("\x1b]8;;").count(),
            2,
            "one open and one close: {drawn:?}",
        );
        assert_eq!(strip(&drawn), "a lon…", "only the visible label is cut");
    }

    /// ANSI/OSC stripped, for comparing what the eye sees.
    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.peek() {
                Some(']') => {
                    while let Some(e) = chars.next() {
                        if e == '\x07' {
                            break;
                        }
                        if e == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    for e in chars.by_ref() {
                        if e == 'm' {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// A cut never separates a variation selector from its base --
    /// and never throws away a pair that fits.
    ///
    /// Asserted as EXACT outputs per width, not as "both present or
    /// both absent": that biconditional is satisfied by deleting both,
    /// which is precisely the regression it let through
    /// (codex 03c64bd).
    #[test]
    fn truncation_keeps_a_variation_selector_with_its_base() {
        let s = "\u{2601}\u{FE0F} remote";
        assert_eq!(super::display_width(s), 9);
        for (cols, want) in [
            (0, ""),
            // Budget 0 after the ellipsis: nothing fits.
            (1, "…"),
            // Budget 1: the cloud alone would fit, but its selector
            // would not -- so the pair goes rather than shipping a
            // bare codepoint.
            (2, "…"),
            // Budget 2: the WHOLE pair fits and must be kept.
            (3, "\u{2601}\u{FE0F}…"),
            (4, "\u{2601}\u{FE0F} …"),
            (5, "\u{2601}\u{FE0F} r…"),
            // Room for everything: no ellipsis at all.
            (9, "\u{2601}\u{FE0F} remote"),
            (20, "\u{2601}\u{FE0F} remote"),
        ] {
            let cut = super::truncate_to(s, cols);
            assert_eq!(cut, want, "at {cols} cols");
            assert!(
                super::display_width(&cut) <= cols,
                "at {cols} cols it overflows: {cut:?}",
            );
        }
    }

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
    /// Unicode width data — which covers every code
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
        ('\u{FE0F}', 0),  // VARIATION SELECTOR-16 — measured with its base
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
            ('\u{FE0F}', 0),  // selector: see `char_width`
            ('a', 1),
            ('\u{0}', 0),      // a control has no printable cell
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

    #[test]
    fn emoji_and_combining_clusters_keep_their_measured_width_when_cut_or_wrapped() {
        for (icon, width) in [
            ("💬", 2),
            ("☁️", 2),
            ("👩‍💻", 2),
            ("🇦🇺", 2),
            ("1️⃣", 2),
            ("e\u{0301}", 1),
        ] {
            assert_eq!(display_width(icon), width, "{icon:?}");
            let text = format!("{icon}abc");
            for cols in 0..=6 {
                let cut = truncate_to(&text, cols);
                assert!(display_width(&cut) <= cols, "{icon:?} at {cols}: {cut:?}");
                assert!(
                    cut == "" || cut == "…" || cut.starts_with(icon),
                    "partial icon: {cut:?}"
                );
            }
            let wrapped = wrap(&format!("{icon}{icon}"), width);
            assert_eq!(wrapped, vec![icon, icon]);
        }
    }
}
