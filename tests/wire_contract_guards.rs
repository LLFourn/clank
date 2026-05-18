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

/// Count substring occurrences of every needle in production code.
///
/// Truncates the file at the first `#[cfg(test)]\nmod` so test
/// fixtures don't inflate the production allowlist. Doesn't try to
/// distinguish strings / comments from real source — the guard's
/// job is to PIN the current count and notice when it changes, so
/// false positives from doc comments mentioning `json!(` get
/// absorbed into the allowlist once and stay stable.
fn count_pattern(haystack: &str, needles: &[&str]) -> usize {
    let body = haystack
        .split_once("#[cfg(test)]\nmod ")
        .map(|(prod, _)| prod)
        .unwrap_or(haystack);
    needles.iter().map(|n| body.matches(n).count()).sum()
}

/// Returns the production source roots the guards scan. `trinity-wire`
/// is included because the canonical wire contracts live there — a
/// stringly DTO field defined in the shared crate would silently
/// pollute both daemon and frontend boundaries.
fn production_roots() -> Vec<PathBuf> {
    let root = workspace_root();
    vec![
        root.join("src"),
        root.join("frontend").join("src"),
        root.join("crates").join("trinity-wire").join("src"),
    ]
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
            let n = count_pattern(&body, needles);
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
         Update the per-file allowlist in tests/wire_contract_guards.rs\n\
         when you intentionally change these counts. The trailing\n\
         comment on each row names the phase that drains it; delete\n\
         the row once it hits zero.\n\n"
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
    // 1 production body builder (autofill on raw Value because the
    // shim doesn't know per-tool arg shapes). Allowed exception:
    // the shim is a transport, not a domain producer.
    ("src/mcp_shim/mod.rs", 1),
    // MCP protocol envelope (allowed exception) + tool-arg parsing
    // via Value. Phase 4 typed the args; the envelope stays.
    ("src/server/mcp.rs", 10),
    // Tool input-schema JSON. Allowed exception per plan §"Allowed
    // exceptions"; typed schema builder is out of scope.
    ("src/tools.rs", 5),
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
    // `match req.tool.as_str() { "list_plans" => ... }` — tool
    // name dispatch. Protocol identifier, not domain state.
    // Allowed exception per plan §"Allowed exceptions".
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
            total += count_pattern(&body, dto_field_needles);
            total += count_pattern(&body, match_needles);
            total += count_pattern(&body, eq_needles);
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
