//! Minimal Markdown → terminal renderer for the status TUI's plan-doc
//! overlay (`status-doc-overlays`). Folds pulldown-cmark events into
//! styled, width-wrapped lines so a plan reads with its structure intact
//! — headings bold, lists bulleted, inline `code`/**bold**/*italic*
//! styled — instead of as raw `#`/`*` source. Pure: (markdown, width) →
//! emitted ANSI lines; no terminal IO.
//!
//! NOT a full CommonMark renderer: tables, images, footnotes, and raw
//! HTML degrade to their plain text (never raw markup noise). Inline
//! emphasis can't combine in the single-`Style` span model, so the
//! innermost wins.

use super::text::{Span, Style, char_width, dim, display_width, emit, plain};

/// Inline-code SGR (yellow) — distinct from body text without a background.
const CODE_COLOR: &str = "33";

/// Render `md` to styled lines, each ≤ `cols` display columns.
pub(super) fn render_markdown(md: &str, cols: usize) -> Vec<String> {
    Md::new(cols).run(md)
}

/// Fold state: finished `out` lines, the current block's `inline`
/// styled-char buffer, the inline-emphasis `styles` stack, and the
/// block context (list nesting + counters, blockquote depth, heading
/// level, fenced-code buffer).
struct Md {
    cols: usize,
    out: Vec<String>,
    inline: Vec<(char, Style)>,
    styles: Vec<Style>,
    /// One entry per open list: `Some(n)` ordered (next number), `None`
    /// bullet.
    lists: Vec<Option<u64>>,
    /// The marker for the next item flush (`• ` / `n. `), consumed once.
    pending_bullet: Option<String>,
    quote: usize,
    heading: Option<usize>,
    code: Option<String>,
}

impl Md {
    fn new(cols: usize) -> Self {
        Self {
            cols,
            out: Vec::new(),
            inline: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            pending_bullet: None,
            quote: 0,
            heading: None,
            code: None,
        }
    }

    fn run(mut self, md: &str) -> Vec<String> {
        use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        for ev in Parser::new_ext(md, opts) {
            match ev {
                Event::Start(Tag::Heading { level, .. }) => {
                    self.flush_para();
                    self.heading = Some(level as usize);
                }
                Event::End(TagEnd::Heading(_)) => self.flush_heading(),
                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => {
                    self.flush_para();
                    self.block_gap();
                }
                Event::Start(Tag::List(start)) => {
                    self.flush_para();
                    self.lists.push(start);
                }
                Event::End(TagEnd::List(_)) => {
                    self.lists.pop();
                    self.block_gap();
                }
                Event::Start(Tag::Item) => {
                    self.flush_para();
                    let bullet = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let b = format!("{n}. ");
                            *n += 1;
                            b
                        }
                        _ => "• ".to_string(),
                    };
                    self.pending_bullet = Some(bullet);
                }
                Event::End(TagEnd::Item) => self.flush_para(),
                Event::Start(Tag::BlockQuote(_)) => {
                    self.flush_para();
                    self.quote += 1;
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    self.flush_para();
                    self.quote = self.quote.saturating_sub(1);
                    self.block_gap();
                }
                Event::Start(Tag::CodeBlock(_)) => {
                    self.flush_para();
                    self.code = Some(String::new());
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.flush_code();
                    self.block_gap();
                }
                Event::Start(Tag::Emphasis) => self.styles.push(Style::Italic),
                Event::End(TagEnd::Emphasis) => {
                    self.styles.pop();
                }
                Event::Start(Tag::Strong) => self.styles.push(Style::Bold),
                Event::End(TagEnd::Strong) => {
                    self.styles.pop();
                }
                Event::Text(t) => match &mut self.code {
                    Some(buf) => buf.push_str(&t),
                    None => self.push_text(&t),
                },
                Event::Code(t) => self.push_styled(&t, Style::Color(CODE_COLOR)),
                // A source newline within a paragraph is a space; an
                // explicit hard break is treated the same (rare in plans).
                Event::SoftBreak | Event::HardBreak if self.code.is_none() => {
                    self.inline.push((' ', self.cur_style()));
                }
                Event::TaskListMarker(done) => {
                    self.push_text(if done { "[x] " } else { "[ ] " });
                }
                Event::Rule => {
                    self.flush_para();
                    self.out
                        .push(emit(&[dim("─".repeat(self.cols))], "", self.cols));
                    self.block_gap();
                }
                // Links/images render their inner text (already emitted by
                // the Text events between Start/End); raw HTML and any
                // unhandled block degrade to that plain text.
                _ => {}
            }
        }
        self.flush_para();
        self.finish()
    }

    /// The style for incoming text: the innermost emphasis, else bold in a
    /// heading, else plain.
    fn cur_style(&self) -> Style {
        self.styles
            .last()
            .cloned()
            .unwrap_or(if self.heading.is_some() {
                Style::Bold
            } else {
                Style::Plain
            })
    }

    fn push_text(&mut self, t: &str) {
        let style = self.cur_style();
        for c in t.chars() {
            self.inline.push((c, style.clone()));
        }
    }

    fn push_styled(&mut self, t: &str, style: Style) {
        for c in t.chars() {
            self.inline.push((c, style.clone()));
        }
    }

    /// Emit the current paragraph / list-item inline buffer: wrap to the
    /// content width (after the blockquote bars + list indent + bullet),
    /// prefixing the first line with the pending bullet and continuations
    /// with matching indent.
    fn flush_para(&mut self) {
        let bullet = self.pending_bullet.take();
        if self.inline.iter().all(|(c, _)| *c == ' ') {
            self.inline.clear();
            return;
        }
        let quote_w = self.quote * 2;
        let list_base = self.lists.len().saturating_sub(1) * 2;
        let bullet_w = bullet.as_deref().map(display_width).unwrap_or(0);
        let avail = self
            .cols
            .saturating_sub(quote_w + list_base + bullet_w)
            .max(1);
        let run = std::mem::take(&mut self.inline);
        for (i, mut spans) in wrap_styled(&run, avail).into_iter().enumerate() {
            let mut line: Vec<Span> = Vec::new();
            for _ in 0..self.quote {
                line.push(dim("│ "));
            }
            if i == 0 && bullet.is_some() {
                line.push(plain(" ".repeat(list_base)));
                line.push(dim(bullet.clone().unwrap()));
            } else {
                line.push(plain(" ".repeat(list_base + bullet_w)));
            }
            line.append(&mut spans);
            self.out.push(emit(&line, "", self.cols));
        }
    }

    /// Emit a heading: a dim `#`×level marker, then the (bold) heading
    /// text wrapped under it.
    fn flush_heading(&mut self) {
        let level = self.heading.take().unwrap_or(1);
        if self.inline.is_empty() {
            return;
        }
        let marker = format!("{} ", "#".repeat(level));
        let marker_w = display_width(&marker);
        let avail = self.cols.saturating_sub(marker_w).max(1);
        let run = std::mem::take(&mut self.inline);
        for (i, mut spans) in wrap_styled(&run, avail).into_iter().enumerate() {
            let mut line = vec![if i == 0 {
                dim(marker.clone())
            } else {
                plain(" ".repeat(marker_w))
            }];
            line.append(&mut spans);
            self.out.push(emit(&line, "", self.cols));
        }
        self.block_gap();
    }

    /// Emit the buffered fenced/indented code block: each line dim, a
    /// 2-space gutter, tabs expanded, hard-cut at `cols` (never wrapped).
    fn flush_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        for line in code.trim_end_matches('\n').split('\n') {
            let line = line.replace('\t', "    ");
            self.out
                .push(emit(&[dim(format!("  {line}"))], "", self.cols));
        }
    }

    /// A blank separator between top-level blocks only (intra-list and
    /// intra-quote content stays tight). Collapsed/trimmed in `finish`.
    fn block_gap(&mut self) {
        if self.lists.is_empty() && self.quote == 0 {
            self.out.push(String::new());
        }
    }

    /// Drop leading/trailing blank lines and collapse interior runs of
    /// blanks to one.
    fn finish(self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.out.len());
        for line in self.out {
            if line.is_empty() && out.last().map(String::is_empty).unwrap_or(true) {
                continue;
            }
            out.push(line);
        }
        while out.last().map(String::is_empty).unwrap_or(false) {
            out.pop();
        }
        out
    }
}

/// Greedy word-wrap a styled char run to `width` DISPLAY columns
/// (`char_width`, so a wide glyph counts as 2), breaking on whitespace
/// and hard-breaking a token wider than the line; each output line is
/// coalesced into equal-style `Span`s. Works on chars, not words, so
/// abutting styles with no separating space (`a`code`b`) stay one token.
fn wrap_styled(run: &[(char, Style)], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    // Split into words (maximal non-space styled-char runs).
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut word: Vec<(char, Style)> = Vec::new();
    for (c, s) in run {
        if *c == ' ' {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push((*c, s.clone()));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }

    let word_w = |w: &[(char, Style)]| w.iter().map(|(c, _)| char_width(*c)).sum::<usize>();
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut line_w = 0usize;
    for word in words {
        let ww = word_w(&word);
        let sep = usize::from(!line.is_empty());
        if line_w + sep + ww <= width {
            if sep == 1 {
                line.push((' ', Style::Plain));
            }
            line.extend(word);
            line_w += sep + ww;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_w = 0;
        }
        if ww <= width {
            line = word;
            line_w = ww;
        } else {
            // Token wider than the line — hard-break it by char width.
            for (c, s) in word {
                let cw = char_width(c);
                if line_w + cw > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push((c, s));
                line_w += cw;
            }
        }
    }
    lines.push(line);
    lines.into_iter().map(coalesce).collect()
}

/// Merge consecutive equal-style chars into `Span`s.
fn coalesce(line: Vec<(char, Style)>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (c, s) in line {
        match spans.last_mut() {
            Some(Span(ps, text)) if *ps == s => text.push(c),
            _ => spans.push(Span(s, c.to_string())),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::visible;

    /// Visible text of every rendered line (ANSI stripped, trailing pad
    /// trimmed).
    fn plain_lines(md: &str, cols: usize) -> Vec<String> {
        render_markdown(md, cols)
            .iter()
            .map(|l| visible(l))
            .collect()
    }

    #[test]
    fn heading_is_bold_with_a_dim_hash_marker() {
        let lines = render_markdown("## Title", 40);
        assert_eq!(visible(&lines[0]), "## Title");
        // The heading text carries bold SGR; the marker is dim.
        assert!(lines[0].contains("\x1b[1m"), "heading bold: {:?}", lines[0]);
        assert!(
            lines[0].contains("\x1b[2m## \x1b[0m"),
            "dim marker: {:?}",
            lines[0]
        );
    }

    #[test]
    fn inline_styles_render_bold_italic_and_code() {
        let raw = render_markdown("a **b** *c* `d`", 40).join("");
        assert!(raw.contains("\x1b[1mb\x1b[0m"), "bold: {raw:?}");
        assert!(raw.contains("\x1b[3mc\x1b[0m"), "italic: {raw:?}");
        assert!(
            raw.contains(&format!("\x1b[{CODE_COLOR}md\x1b[0m")),
            "code: {raw:?}"
        );
        // Plain text is preserved between the styled runs.
        assert_eq!(
            visible(&render_markdown("a **b** *c* `d`", 40)[0]),
            "a b c d"
        );
    }

    #[test]
    fn bullet_list_is_indented_under_a_marker() {
        let lines = plain_lines("- one\n- two", 40);
        assert_eq!(lines, vec!["• one", "• two"], "tight bullets, no blanks");
    }

    #[test]
    fn ordered_list_numbers_and_increments() {
        let lines = plain_lines("1. a\n2. b", 40);
        assert_eq!(lines, vec!["1. a", "2. b"]);
    }

    #[test]
    fn nested_list_indents_further() {
        let lines = plain_lines("- a\n  - b", 40);
        assert_eq!(lines, vec!["• a", "  • b"], "nested bullet indented");
    }

    #[test]
    fn blockquote_gets_a_dim_bar() {
        let lines = render_markdown("> quoted", 40);
        assert_eq!(visible(&lines[0]), "│ quoted");
        assert!(
            lines[0].contains("\x1b[2m│ \x1b[0m"),
            "dim bar: {:?}",
            lines[0]
        );
    }

    #[test]
    fn code_block_is_verbatim_and_dim() {
        let lines = plain_lines("```\nlet x = 1;\n```", 40);
        assert_eq!(lines, vec!["  let x = 1;"], "gutter, no fence, not wrapped");
    }

    #[test]
    fn paragraphs_are_separated_by_one_blank_line() {
        let lines = plain_lines("para one\n\npara two", 40);
        assert_eq!(lines, vec!["para one", "", "para two"]);
    }

    #[test]
    fn thematic_break_is_a_rule() {
        let lines = plain_lines("a\n\n---\n\nb", 40);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[2], "─".repeat(40), "rule spans the width");
        assert_eq!(lines[4], "b");
    }

    #[test]
    fn long_paragraph_wraps_within_cols_preserving_inline_style() {
        // Width forces a wrap; the styled run must wrap by display width
        // and keep the bold styling on its word wherever it lands.
        let lines = render_markdown("alpha beta **gamma** delta", 12);
        for l in &lines {
            assert!(
                display_width(&visible(l)) <= 12,
                "line over 12 cols: {:?} ({})",
                visible(l),
                display_width(&visible(l))
            );
        }
        assert!(lines.len() >= 2, "must wrap: {lines:?}");
        assert!(
            lines.join("").contains("\x1b[1mgamma\x1b[0m"),
            "bold survives wrap"
        );
        // Reassembled visible text is the original words in order.
        let joined = lines
            .iter()
            .map(|l| visible(l))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "alpha beta gamma delta");
    }

    #[test]
    fn wrap_breaks_on_display_width_for_wide_chars() {
        // Each 🔨 is 2 cols, so width 4 fits two per line — the break is
        // driven by char_width, not char count (5 emoji = 10 cols).
        let lines = plain_lines("🔨🔨 🔨🔨 🔨", 4);
        for l in &lines {
            assert!(display_width(l) <= 4, "wide-char line over 4: {l:?}");
        }
        assert_eq!(
            lines
                .iter()
                .map(|l| l.chars().filter(|c| *c == '🔨').count())
                .sum::<usize>(),
            5
        );
    }

    #[test]
    fn abutting_styles_with_no_space_stay_one_word() {
        // "a`b`c" — text, inline-code, text with NO separating spaces must
        // render contiguously (a naive word-joiner would inject spaces).
        assert_eq!(visible(&render_markdown("a`b`c", 40)[0]), "abc");
    }

    #[test]
    fn empty_markdown_renders_nothing() {
        assert!(render_markdown("", 40).is_empty());
        assert!(render_markdown("\n\n", 40).is_empty());
    }
}
