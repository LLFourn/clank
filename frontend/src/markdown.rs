//! Wasm-side markdown → sanitized HTML rendering.
//!
//! Matches the daemon's previous server-side renderer byte-for-byte
//! (same `pulldown-cmark` options, same `ammonia::Builder::default()`
//! plus the one allowed extra attribute). Phase 4 of
//! `wasm-markdown-rendering.md` deletes the daemon's renderer once
//! every consumer is on this path.

// Phase 1 adds the renderer module; Phase 2 wires components to it.
// Until then nothing in the production view tree calls these.
#![allow(dead_code)]

use pulldown_cmark::{Options, Parser, html};
use trinity_core::vocab::Verdict;

/// Render a feedback body with optional verdict-marker stripping.
/// `APPROVE` / `REQUEST_CHANGES` markers at the top of the body are
/// consumed before rendering so the marker doesn't appear in the
/// displayed HTML — matching `disk_format::parse_verdict` leniency.
pub fn render_feedback(body: &str, verdict: Verdict) -> String {
    let stripped = match verdict {
        Verdict::Approve | Verdict::RequestChanges => strip_marker_line(body),
        Verdict::Unmarked => body,
    };
    render(stripped)
}

/// Render arbitrary markdown to sanitized HTML.
pub fn render(input: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(input, opts);
    let mut raw = String::new();
    html::push_html(&mut raw, parser);
    ammonia::Builder::default()
        .add_generic_attributes(["class"])
        .clean(&raw)
        .to_string()
}

/// Strip a leading `APPROVE` or `REQUEST_CHANGES` marker line plus
/// trailing horizontal whitespace and up to one CR/LF.
fn strip_marker_line(body: &str) -> &str {
    let mut chars = body.char_indices();
    while let Some((_, c)) = chars.clone().next() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        break;
    }
    let after_leading = chars.as_str();
    if let Some(rest) = after_leading.strip_prefix("APPROVE") {
        skip_marker_tail(rest)
    } else if let Some(rest) = after_leading.strip_prefix("REQUEST_CHANGES") {
        skip_marker_tail(rest)
    } else {
        body
    }
}

fn skip_marker_tail(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] == b'\r' || bytes[i] == b'\n') {
        i += 1;
    }
    &s[i..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_marker_stripped() {
        let html = render_feedback("APPROVE\n\nlgtm\n", Verdict::Approve);
        assert!(html.contains("<p>lgtm</p>"));
        assert!(!html.contains("APPROVE"));
    }

    #[test]
    fn approve_marker_with_trailing_spaces_stripped() {
        let html = render_feedback("APPROVE   \n\nrest\n", Verdict::Approve);
        assert!(html.contains("<p>rest</p>"));
        assert!(!html.contains("APPROVE"));
    }

    #[test]
    fn request_changes_marker_stripped() {
        let html = render_feedback("REQUEST_CHANGES\n\nbug\n", Verdict::RequestChanges);
        assert!(html.contains("<p>bug</p>"));
        assert!(!html.contains("REQUEST_CHANGES"));
    }

    #[test]
    fn unmarked_keeps_body_verbatim() {
        let html = render_feedback("look at this\n", Verdict::Unmarked);
        assert!(html.contains("<p>look at this</p>"));
    }

    #[test]
    fn code_fence_renders_pre_code() {
        let html = render("```\nfn foo() {}\n```\n");
        assert!(html.contains("<pre>"));
        assert!(html.contains("<code>"));
        assert!(html.contains("fn foo() {}"));
    }

    #[test]
    fn tables_enabled() {
        let html = render("| h |\n| - |\n| c |\n");
        assert!(html.contains("<table>"));
    }

    #[test]
    fn strikethrough_enabled() {
        let html = render("~~old~~");
        assert!(html.contains("<del>"));
    }

    #[test]
    fn link_target_blank_attribute_dropped_by_ammonia() {
        let html = render("[x](javascript:alert(1))");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn class_attribute_preserved() {
        let html = render(r#"<span class="hl">x</span>"#);
        assert!(html.contains(r#"class="hl""#));
    }

    #[test]
    fn script_tag_stripped() {
        let html = render("<script>alert(1)</script>hi");
        assert!(!html.contains("<script>"));
        assert!(html.contains("hi"));
    }
}
