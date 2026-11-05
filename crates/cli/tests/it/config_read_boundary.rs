//! Enforcement gate (durable-formats-survive-upgrade): production code
//! parses a config document only inside the module that owns it.
//!
//! Each config document has one validated boundary, which refuses a
//! malformed file and one a newer clank wrote. A reader that parses the
//! bytes itself skips that check: it reads a newer document as if it were
//! today's, and a writer built on it puts back a document the newer clank
//! has moved on from.
//!
//! The owners:
//! - `user-config/src/{lib,versioned}.rs`: `~/.clank/config.json`, and the
//!   versioned read every document goes through;
//! - `cli/src/agent_store/repo_config.rs`: `<repo>/.clank/config.json`;
//! - `cli/src/agent_store.rs`: `.clank/agents/<label>/config.json`.
//!
//! A function outside them that both names a config document and parses
//! JSON from bytes is refused, as is parsing bytes straight into a config
//! type anywhere.

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use quote::ToTokens;
use syn::visit::Visit;

const OWNERS: &[&str] = &[
    "user-config/src/lib.rs",
    "user-config/src/versioned.rs",
    "cli/src/agent_store/repo_config.rs",
    "cli/src/agent_store.rs",
];

const PARSES_BYTES: &[&str] = &[
    "serde_json::from_str",
    "serde_json::from_slice",
    "serde_json::from_reader",
];

/// How a function names a config document's path.
const NAMES_A_CONFIG: &[&str] = &[
    "config.json\"",
    "clank_user_config::path(",
    "repo_config::path(",
    "agent_config_path(",
    "user_config_path(",
];

const CONFIG_TYPES: &[&str] = &[
    "RepoConfigFile",
    "UserConfigFile",
    "AgentConfig",
    "ConfigFile",
];

/// Why `body`, one function's tokens with the whitespace removed, parses
/// a config document, if it does.
fn violation(body: &str) -> Option<String> {
    let parse = PARSES_BYTES.iter().find(|p| body.contains(*p));
    if let Some(parse) = parse
        && let Some(name) = NAMES_A_CONFIG.iter().find(|n| body.contains(*n))
    {
        return Some(format!("`{parse}` beside `{name}`"));
    }
    for parse in PARSES_BYTES {
        for ty in CONFIG_TYPES {
            let turbofish = format!("{parse}::<{ty}>");
            let tail = format!("{parse}::<clank_core::agent_config::{ty}>");
            if body.contains(&turbofish) || body.contains(&tail) {
                return Some(format!("`{parse}` into `{ty}`"));
            }
        }
    }
    None
}

#[derive(Default)]
struct Fns {
    found: Vec<(usize, String)>,
}

impl Fns {
    fn check(&mut self, line: usize, tokens: &impl ToTokens) {
        let body: String = tokens
            .to_token_stream()
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if let Some(why) = violation(&body) {
            self.found.push((line, why));
        }
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_cfg_test)
}

impl<'ast> Visit<'ast> for Fns {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !test_only(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if !test_only(&i.attrs) {
            syn::visit::visit_item_impl(self, i);
        }
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if !test_only(&f.attrs) {
            self.check(f.sig.ident.span().start().line, f);
        }
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if !test_only(&f.attrs) {
            self.check(f.sig.ident.span().start().line, f);
        }
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        if !test_only(&f.attrs) {
            self.check(f.sig.ident.span().start().line, f);
        }
    }
}

fn under_crates(f: &std::path::Path) -> String {
    f.strip_prefix(crates_dir())
        .unwrap_or(f)
        .to_string_lossy()
        .replace('\\', "/")
}

fn violations_in(src: &str) -> Result<Vec<(usize, String)>, String> {
    let parsed = syn::parse_file(src).map_err(|e| format!("does not parse: {e}"))?;
    let mut fns = Fns::default();
    fns.visit_file(&parsed);
    Ok(fns.found)
}

#[test]
fn only_a_documents_owner_parses_it() {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir()).unwrap().flatten() {
        rs_files(&entry.path().join("src"), &mut files);
    }
    assert!(!files.is_empty(), "scanned no source files");
    let production = Production::of(&files);
    let mut violations = Vec::new();
    for f in &files {
        let rel = under_crates(f);
        if OWNERS.contains(&rel.as_str()) || production.is_test_only(f) {
            continue;
        }
        match violations_in(&std::fs::read_to_string(f).unwrap()) {
            Ok(found) => violations.extend(
                found
                    .into_iter()
                    .map(|(line, why)| format!("{rel}:{line}: {why}")),
            ),
            Err(e) => violations.push(format!("{rel}: {e}")),
        }
    }
    assert!(
        violations.is_empty(),
        "production code parses a config document outside its owner. Read it \
         through `clank_user_config::read`, `agent_store::repo_config::read` or \
         `agent_store::load_agent_config`:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn the_parsing_shapes_are_recognized() {
    let caught = |src: &str| !violations_in(src).unwrap().is_empty();
    assert!(caught(
        "fn f(r: &Path) { let s = std::fs::read_to_string(r.join(\".clank/config.json\")).unwrap(); \
         let v: serde_json::Value = serde_json::from_str(&s).unwrap(); }"
    ));
    assert!(caught(
        "fn f(h: &Path) { let b = std::fs::read(clank_user_config::path(h)).unwrap(); \
         let v: Value = serde_json::from_slice(&b).unwrap(); }"
    ));
    assert!(caught(
        "impl X { fn f(&self, s: &str) -> RepoConfigFile { serde_json::from_str::<RepoConfigFile>(s).unwrap() } }"
    ));
    assert!(caught(
        "fn f(s: &str) { let c = serde_json::from_slice::<clank_core::agent_config::AgentConfig>(s.as_bytes()); }"
    ));
    assert!(!caught(
        "fn f(p: &Path) { let s = std::fs::read_to_string(p.join(\"events.jsonl\")).unwrap(); \
         let v: Value = serde_json::from_str(&s).unwrap(); }"
    ));
    assert!(!caught(
        "fn f(r: &Path) -> anyhow::Result<()> { let c = agent_store::repo_config::read(r)?; Ok(()) }"
    ));
    assert!(!caught(
        "#[cfg(test)] mod tests { fn f(s: &str) { serde_json::from_str::<RepoConfigFile>(s); } }"
    ));
}
