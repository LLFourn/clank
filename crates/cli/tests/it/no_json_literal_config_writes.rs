//! Phase 4 of `typed-config-dogfood`: negative regression test
//! that fails if any `.rs` file under `crates/cli/tests/`, at any
//! depth, contains a raw JSON literal that looks like a
//! `.clank/config.json` write.
//!
//! The check is best-effort, NOT a formal grammar:
//! - Scans each test file for `r#"{` substrings followed
//!   (within a window) by a known config field name like
//!   `"role"`, `"agents"`, `"hooks"`, etc.
//! - Multi-line literals are handled by reading the full file
//!   body, not line-by-line.
//! - Lines (in original source) marked with the comment
//!   `// allow-json-literal: <reason>` are stripped from the
//!   search BEFORE matching. That lets a deliberate test
//!   (e.g. parser-fail-closed input, alias-on-input regression)
//!   opt out with a documented reason.
//!
//! Ruthless review of b6323be flagged that a naive
//! single-line regex would miss multi-line literals AND
//! produce false positives for output-assertion tests. The
//! window-scan + opt-out marker addresses both concerns
//! pragmatically.

use std::fs;
use std::path::Path;

const FIELD_NAMES: &[&str] = &[
    r#""role""#,
    r#""agents""#,
    r#""default_agents""#,
    r#""hooks""#,
    r#""review""#,
    r#""diff""#,
];
const ALLOW_MARKER: &str = "// allow-json-literal:";
/// Hard cap on raw-string literal length we'll consider — a
/// safety net against pathological inputs. Real config writes
/// are well under this.
const MAX_LITERAL: usize = 8192;

/// Strip the marker line AND the next non-blank line. That
/// matches the natural Rust pattern of putting the opt-out
/// comment on a line BEFORE the literal it's documenting:
///
/// ```ignore
/// std::fs::write(
///     path,
///     // allow-json-literal: deliberate alias-on-input test
///     r#"{"role":"reviewers"}"#,
/// );
/// ```
fn body_without_allowed_lines(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut skip_next_nonblank = false;
    for line in lines {
        if line.contains(ALLOW_MARKER) {
            skip_next_nonblank = true;
            continue;
        }
        if skip_next_nonblank && !line.trim().is_empty() {
            skip_next_nonblank = false;
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Find the close of a Rust raw string literal that opened with
/// `r#"` at position `open_idx` (so the `"` after `#` is at
/// `open_idx + 2`). The literal ends at the first `"#` after that.
fn find_raw_string_close(body: &str, open_idx: usize) -> Option<usize> {
    let after_open = open_idx + 3;
    let bytes = body.as_bytes();
    let limit = (after_open + MAX_LITERAL).min(bytes.len().saturating_sub(1));
    let mut j = after_open;
    while j + 1 < limit {
        if bytes[j] == b'"' && bytes[j + 1] == b'#' {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn flag_config_json_literal(body: &str, path: &Path) -> Option<String> {
    // Find every `r#"{` raw-string opener, then scan ONLY within
    // that literal's bounds (up to the matching `"#`) for a
    // config-shape field name. Ruthless's window-fragility
    // concern: a fixed window catches unrelated text past the
    // literal. Bounding by the close delimiter eliminates that.
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if &bytes[i..i + 3] == b"r#\"" && bytes.get(i + 3) == Some(&b'{') {
            if let Some(close) = find_raw_string_close(body, i) {
                let literal = &body[i + 3..close];
                for field in FIELD_NAMES {
                    if literal.contains(field) {
                        let snippet: String = literal.chars().take(120).collect();
                        return Some(format!(
                            "JSON literal config-shaped write in `{}` near offset {}: `r#\"{}…\"#`. \
                             Use the typed RepoConfigFile / UserConfigFile / AgentConfig struct \
                             instead; add `// allow-json-literal: <reason>` on the line above to \
                             opt out (deliberate alias / parser-fail-closed test).",
                            path.display(),
                            i,
                            snippet,
                        ));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Every finding under `dir`, at any depth. Recursive because the
/// test files live under `tests/it/` — a walk of the immediate
/// children of `tests/` would stay green while scanning nothing
/// (the-test-suite-takes-a-million-years).
fn findings_under(dir: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            findings.extend(findings_under(&path));
            continue;
        }
        if !path.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        // Skip ourselves — this file mentions the field names in its
        // own source.
        if path.file_name().and_then(|s| s.to_str()) == Some("no_json_literal_config_writes.rs") {
            continue;
        }
        let raw = fs::read_to_string(&path).unwrap();
        let filtered = body_without_allowed_lines(&raw);
        if let Some(msg) = flag_config_json_literal(&filtered, &path) {
            findings.push(msg);
        }
    }
    findings
}

#[test]
fn no_json_literal_config_writes_in_tests() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let findings = findings_under(&tests_dir);
    assert!(
        findings.is_empty(),
        "typed-config-dogfood acceptance violated — JSON literal config writes found:\n\n{}\n",
        findings.join("\n\n")
    );
}

#[test]
fn a_nested_violation_is_found() {
    // The gate must reach `tests/it/…` and deeper, not just the
    // immediate children of `tests/`.
    let root = tempfile::tempdir().unwrap();
    let deep = root.path().join("it").join("deeper");
    fs::create_dir_all(&deep).unwrap();
    fs::write(root.path().join("top.rs"), "fn a() {}\n").unwrap();
    let planted = "let c = r#\"{\"agents\": []}\"#;\n";
    fs::write(deep.join("planted.rs"), planted).unwrap();
    fs::write(deep.join("notes.md"), planted).unwrap();
    let findings = findings_under(root.path());
    assert_eq!(
        findings.len(),
        1,
        "exactly the planted .rs file:\n{findings:?}"
    );
    assert!(findings[0].contains("planted.rs"), "{}", findings[0]);
}

#[test]
fn allow_marker_strips_next_nonblank_line() {
    // Marker on line 1; literal on line 2; both stripped.
    let input = "// allow-json-literal: alias test\nlet x = r##\"{\"role\":\"reviewers\"}\"##;";
    let filtered = body_without_allowed_lines(input);
    assert!(
        !filtered.contains(r##"r##"{"##),
        "marker should strip the following literal-bearing line; got: {filtered:?}"
    );
}

#[test]
fn allow_marker_strips_across_blank_lines() {
    // Blank lines between marker and literal don't break the
    // opt-out — the next NON-blank line gets stripped.
    let input = "// allow-json-literal: alias test\n\nlet x = r##\"{\"role\":\"reviewers\"}\"##;";
    let filtered = body_without_allowed_lines(input);
    assert!(
        !filtered.contains(r##"r##"{"##),
        "marker should skip blank lines and strip the next non-blank; got: {filtered:?}"
    );
}

#[test]
fn flag_config_json_literal_detects_typed_field_in_window() {
    // The detector triggers on the field name within window.
    let body = "let x = r#\"{\"role\":\"master\"}\"#;";
    let result = flag_config_json_literal(body, Path::new("dummy.rs"));
    assert!(
        result.is_some(),
        "should flag literal that contains a config field"
    );
}

#[test]
fn flag_config_json_literal_skips_unrelated_raw_strings() {
    // A raw string that's NOT a config-shape (no recognized
    // field names) should pass.
    let body = "let x = r#\"{\"some_other_key\":\"value\"}\"#;";
    let result = flag_config_json_literal(body, Path::new("dummy.rs"));
    assert!(
        result.is_none(),
        "should NOT flag unrelated raw strings; got: {result:?}"
    );
}
