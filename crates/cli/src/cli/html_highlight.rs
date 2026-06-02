//! Syntax-highlighted source spans for `clank html` diffs.
//!
//! Wraps syntect's class-based HTML output so the rendered
//! markup keeps theming in our stylesheet. The SyntaxSet is
//! built once (lazy) and reused across files.

use std::sync::OnceLock;

use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Highlight a single line of source for `lang_hint` (a file
/// extension like "rs", "toml", "md") and return an inner
/// HTML fragment (no wrapping element). Falls back to a
/// plain-escaped span when the extension isn't recognized or
/// syntect chokes.
pub fn highlight_line(lang_hint: Option<&str>, line: &str) -> String {
    let ss = syntax_set();
    let syntax = lang_hint
        .and_then(|ext| ss.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        ss,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    let mut out = String::new();
    let line_with_endings: String = if line.ends_with('\n') {
        line.to_string()
    } else {
        format!("{line}\n")
    };
    for chunk in LinesWithEndings::from(&line_with_endings) {
        if generator
            .parse_html_for_line_which_includes_newline(chunk)
            .is_err()
        {
            return escape_html(line);
        }
    }
    out.push_str(&generator.finalize());
    // syntect's per-line output wraps each line in its own
    // `<span class="hl-...">...</span>` chain plus a trailing
    // newline. The caller already provides the row container
    // and trailing whitespace handling, so strip the final
    // newline if present.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_keyword() {
        let h = highlight_line(Some("rs"), "fn main() {}");
        // syntect's Rust grammar tags `fn` with a class
        // containing `keyword` (specifically `hl-source` then
        // `hl-keyword.source`). Just verify SOME hl- class
        // appeared so we know the parser engaged.
        assert!(h.contains("hl-"), "expected syntect hl- classes in `{h}`");
    }

    #[test]
    fn unknown_extension_falls_back_to_plain_text() {
        let h = highlight_line(Some("weird-ext"), "anything goes <here>");
        // Plain-text syntax emits no semantic classes but
        // still goes through syntect (which escapes safely).
        assert!(h.contains("&lt;here&gt;"));
    }

    #[test]
    fn empty_line_does_not_panic() {
        let _ = highlight_line(Some("rs"), "");
    }
}
