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

/// Highlight a multi-line block of source for `lang_hint`
/// (a token like "rust", a friendly name like "Rust", or a
/// file extension like "rs") and return an inner HTML fragment
/// (no `<pre>`/`<code>` wrapper). Newlines between lines are
/// preserved; the trailing newline (if any) survives.
///
/// Falls back to a plain-escaped string when the lang isn't
/// recognized or syntect chokes. Caller wraps the result in
/// the appropriate container element.
///
/// Used by `render_markdown` for fenced code blocks; the diff
/// path uses `highlight_line` because it owns per-row containers.
pub fn highlight_block(lang_hint: Option<&str>, source: &str) -> String {
    let ss = syntax_set();
    let syntax = lang_hint
        .and_then(|hint| {
            // Try token first (handles "rust"/"bash"/"python"
            // info strings on markdown fences), then friendly
            // name (handles "Rust"), then extension (handles
            // "rs"/"sh"/"py" for callers that pass extensions
            // like the diff path does).
            ss.find_syntax_by_token(hint)
                .or_else(|| ss.find_syntax_by_name(hint))
                .or_else(|| ss.find_syntax_by_extension(hint))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        ss,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    let with_trailing: String = if source.ends_with('\n') {
        source.to_string()
    } else {
        format!("{source}\n")
    };
    for chunk in LinesWithEndings::from(&with_trailing) {
        if generator
            .parse_html_for_line_which_includes_newline(chunk)
            .is_err()
        {
            return escape_html(source);
        }
    }
    generator.finalize()
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

    #[test]
    fn highlight_block_rust_multiline_has_hl_classes() {
        let src = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let h = highlight_block(Some("rust"), src);
        assert!(h.contains("hl-"), "expected hl- classes in `{h}`");
        // All three meaningful lines' content shows up.
        assert!(h.contains("main"));
        assert!(h.contains("let"));
        assert!(h.contains("println"));
    }

    #[test]
    fn highlight_block_resolves_via_token_and_via_extension() {
        // Both `rust` (markdown-style fence info) and `rs`
        // (file-extension caller) map to a Rust grammar.
        let by_token = highlight_block(Some("rust"), "fn x() {}\n");
        let by_ext = highlight_block(Some("rs"), "fn x() {}\n");
        assert!(by_token.contains("hl-"), "token lookup failed: {by_token}");
        assert!(by_ext.contains("hl-"), "extension lookup failed: {by_ext}");
    }

    #[test]
    fn highlight_block_unknown_lang_falls_back_safely() {
        let h = highlight_block(Some("not-a-real-lang"), "anything <here>\n");
        // Plain-text fallback still escapes safely.
        assert!(h.contains("&lt;here&gt;"));
    }

    #[test]
    fn highlight_block_no_lang_safe_escape() {
        // Untyped fence (` ``` ` without info string).
        let h = highlight_block(None, "<div>raw</div>\n& more\n");
        assert!(h.contains("&lt;div&gt;"));
        assert!(h.contains("&amp; more"));
    }
}
