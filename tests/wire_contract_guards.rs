//! Phase 1 deliverable for `.trinity/plans/purge-stringly-typed.md`.
//!
//! Two guard tests pin the current count of patterns the plan is
//! draining. As phases 2-8 convert sites, the per-file allowlist
//! drops; new unapproved sites fail CI with a clear diff between
//! observed and expected.
//!
//! The allowlist entries below carry phase-target comments — that
//! IS the audit. There is no separate audit document; the guard
//! is self-describing.
//!
//! ## Guard A — dynamic-JSON sites
//!
//! Counts `json!(`, `serde_json::Value` (as a type), and
//! `axum::Json<Value>` occurrences in production source, OUTSIDE
//! comments and string literals. The substring scanner catches
//! literal occurrences in code; doc-comment mentions and rustdoc
//! examples don't inflate the count.
//!
//! ## Guard B — stringly-control-flow sites
//!
//! Counts production DTO fields named `kind / state / phase /
//! reason / role / verdict / lifecycle / posture / worktree_status`
//! typed as `String` (instead of the corresponding enum), plus
//! production `match x.as_str() { "<vocab>" => ... }` and equality
//! comparisons against known closed-vocab strings.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Drop the workspace-root prefix so error messages stay readable
/// regardless of where `cargo test` is invoked from.
fn relative(p: &Path) -> String {
    let cwd = workspace_root();
    p.strip_prefix(&cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string_lossy().to_string())
}

fn workspace_root() -> PathBuf {
    // tests/ runs with CARGO_MANIFEST_DIR = root of the daemon crate,
    // which is also the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Walk `root` (recursively) and return every `.rs` file under it,
/// skipping `target/` and any path component that starts with `.`.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name == "target" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Count occurrences of any of `needles` in `haystack`, NOT scanning
/// inside `//` line comments or `/* ... */` block comments. Simple
/// state machine — good enough for production Rust files where we
/// don't expect adversarial commenting.
fn count_outside_comments(haystack: &str, needles: &[&str]) -> usize {
    let bytes = haystack.as_bytes();
    let mut count = 0usize;
    let mut i = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut in_string = false;
    let mut string_char = b'"';
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == string_char {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                in_line_comment = true;
                i += 2;
                continue;
            }
            if bytes[i + 1] == b'*' {
                in_block_comment = true;
                i += 2;
                continue;
            }
        }
        if b == b'"' || b == b'\'' {
            in_string = true;
            string_char = b;
            i += 1;
            continue;
        }
        for needle in needles {
            let n = needle.as_bytes();
            if bytes[i..].starts_with(n) {
                count += 1;
                i += n.len();
                continue;
            }
        }
        i += 1;
    }
    count
}

/// Returns the production source roots the guards scan.
fn production_roots() -> Vec<PathBuf> {
    let root = workspace_root();
    vec![root.join("src"), root.join("frontend").join("src")]
}

/// Map { file -> total pattern count } for the patterns named by
/// `needles`. Files with zero hits are omitted.
fn scan(needles: &[&str]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for root in production_roots() {
        for path in rust_files(&root) {
            let body = match fs::read_to_string(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let n = count_outside_comments(&body, needles);
            if n > 0 {
                out.insert(relative(&path), n);
            }
        }
    }
    out
}

/// Assert observed counts match the expected allowlist. On mismatch,
/// prints the symmetric difference so a developer who adds a new
/// site sees exactly where their over-budget came from.
fn assert_allowlist(observed: BTreeMap<String, usize>, expected: &[(&str, usize)], guard: &str) {
    let expected_map: BTreeMap<String, usize> =
        expected.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    if observed == expected_map {
        return;
    }
    let mut report = String::new();
    report.push_str(&format!(
        "\n=== {guard}: allowlist mismatch ===\n\
         Update the allowlist in tests/wire_contract_guards.rs AND \
         the audit at .trinity/stubs/purge-stringly-typed-audit.md\n\
         when you intentionally change these counts.\n\n"
    ));
    for (file, observed_count) in &observed {
        match expected_map.get(file) {
            Some(expected_count) if expected_count == observed_count => {}
            Some(expected_count) => {
                report.push_str(&format!(
                    "  {file}: expected {expected_count}, observed {observed_count} \
                     ({:+})\n",
                    *observed_count as i64 - *expected_count as i64
                ));
            }
            None => {
                report.push_str(&format!(
                    "  {file}: NEW FILE, observed {observed_count} (was 0)\n"
                ));
            }
        }
    }
    for (file, expected_count) in &expected_map {
        if !observed.contains_key(file) {
            report.push_str(&format!(
                "  {file}: REMOVED FROM SOURCE (was expected {expected_count})\n"
            ));
        }
    }
    panic!("{report}");
}

// ============================================================
// Guard A — dynamic-JSON sites
// ============================================================

/// Per-file allowlist for `json!(`, `serde_json::Value`,
/// `axum::Json<Value>`. Counts the LITERAL occurrence of those
/// substrings in source (excluding comments / string literals).
/// Phases 2-8 drain these entries. The trailing comment on each
/// row names the phase that drains it; if a row hits zero before
/// its phase, delete the entry.
const GUARD_A_ALLOWLIST: &[(&str, usize)] = &[
    // Public response builders for `get_context`, `list_plans`,
    // `plan_summary`. Drains to typed DTOs in Phase 4.
    ("src/mcp_response.rs", 19),
    // 1 production body builder (autofill on raw Value because the
    // shim doesn't know per-tool arg shapes) + 5 test fixtures.
    // Phase 4 decides typed-per-tool vs leave-as-Value.
    ("src/mcp_shim/mod.rs", 6),
    // `payload: Value` field type on RepoEvent. Drains in Phase 7
    // (typed event payloads).
    ("src/repo_state.rs", 1),
    // LiveEvent payload construction call sites. Drain in Phase 7.
    ("src/runtime.rs", 5),
    // Route handlers returning `axum::Json<Value>` + json! body
    // construction. Drain to `axum::Json<T>` in Phase 5.
    ("src/server/http.rs", 36),
    // MCP protocol envelope (allowed exception) + tool-arg parsing
    // via Value. Phase 4 keeps the envelope, types the args.
    ("src/server/mcp.rs", 10),
    // Tool input-schema JSON. Allowed exception per plan §"Allowed
    // exceptions"; typed schema builder is out of scope.
    ("src/tools.rs", 5),
    // Leptos /api/* response builders. Drain in Phase 5.
    ("src/ui_response.rs", 19),
];

#[test]
fn guard_a_dynamic_json_sites_match_allowlist() {
    let observed = scan(&["json!(", "serde_json::Value", "axum::Json<Value>"]);
    assert_allowlist(observed, GUARD_A_ALLOWLIST, "Guard A (dynamic-JSON)");
}

// ============================================================
// Guard B — stringly-control-flow sites
// ============================================================

/// Per-file allowlist for DTO fields typed `String` for closed
/// vocabularies (`pub kind: String`, `pub state: String`, etc.) +
/// `match x.as_str() { "..." }` + equality against known closed-
/// vocab string literals.
///
/// These counts capture the literal patterns the audit lists; once
/// the field's type changes from `String` to the corresponding
/// enum (Phase 6), the count drops.
const GUARD_B_ALLOWLIST: &[(&str, usize)] = &[
    // Frontend DTOs: 9 `pub <vocab>: String` fields the scanner
    // catches across PlanRow / ReviewGate / CommitFeedback /
    // CommitRow / PlanDetail / CommitDiffPage / WaitingOn. Drain
    // in Phase 6 once the wire crate exposes the enums.
    ("frontend/src/api.rs", 9),
    // SSE event kind: String — two repo/plan event payloads.
    // Drain in Phase 7 (typed SSE events).
    ("frontend/src/store.rs", 2),
    // `page.kind == "finalize"` equality on the commit-detail
    // page and inline expansion. Drains in Phase 6 once the wire
    // crate's tagged CommitDetail enum lands.
    ("frontend/src/components/commit_diff.rs", 1),
    ("frontend/src/components/expanded_commit.rs", 1),
    // `match line.kind.as_str() { "addition" => ... }` —
    // diff-line kind. Closed vocab; drain in Phase 6.
    ("frontend/src/components/structured_diff.rs", 1),
    // `WaitArgs.role: String` on the daemon side. Drain in
    // Phase 4 alongside MCP arg typing.
    ("src/server/wait.rs", 1),
    // `match req.tool.as_str() { "list_plans" => ... }` — tool
    // name dispatch. Protocol identifier, not domain state.
    // Phase 4 decides whether to introduce a typed ToolName enum
    // or leave as the dispatch boundary.
    ("src/server/mcp.rs", 1),
];

#[test]
fn guard_b_stringly_control_flow_sites_match_allowlist() {
    // Patterns:
    //   - `pub <vocab>: String` — DTO fields named with a closed-vocab
    //     name typed String. Includes a leading newline-anchor
    //     approximation via the `pub ` prefix.
    //   - `match ` + `.as_str()` — broken into two needles since the
    //     pattern usually spans `match expr.as_str() {` with
    //     intervening text. We count each `as_str()` match-arm
    //     occurrence by the `.as_str() {` pattern, which is the
    //     load-bearing piece.
    //   - `== "finalize"` — explicit equality against a known wire
    //     string.
    let dto_field_needles: &[&str] = &[
        "pub kind: String",
        "pub state: String",
        "pub phase: String",
        "pub reason: String",
        "pub role: String",
        "pub verdict: String",
        "pub lifecycle: String",
        "pub posture: String",
        "pub worktree_status: String",
    ];
    let match_needles: &[&str] = &[".as_str() {"];
    let eq_needles: &[&str] = &["== \"finalize\""];

    let mut observed: BTreeMap<String, usize> = BTreeMap::new();
    for root in production_roots() {
        for path in rust_files(&root) {
            let body = match fs::read_to_string(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let mut total = 0usize;
            total += count_outside_comments(&body, dto_field_needles);
            total += count_outside_comments(&body, match_needles);
            total += count_outside_comments(&body, eq_needles);
            if total > 0 {
                observed.insert(relative(&path), total);
            }
        }
    }
    assert_allowlist(
        observed,
        GUARD_B_ALLOWLIST,
        "Guard B (stringly-control-flow)",
    );
}
