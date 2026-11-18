//! Gate (harness-adapters-own-each-tools-facts): on the wake path, only a
//! harness's adapter (`clank_core::harness`) decides by which tool it is.
//!
//! A harness changing its hooks used to surface as a silent bug in
//! whichever `match tool` the change missed. Every decision a wake-path
//! module makes on a `Tool` variant (a pattern naming one, a `==`/`!=`
//! against one, a `matches!` over one) is therefore either gone behind an
//! adapter's capability or listed in [`INVENTORY`] as naming or out of
//! scope, with why. A listed function with more decisions than it lists,
//! and a listed one whose decisions are gone, both fail.
//!
//! Protocol knowledge need not name a `Tool`: an event name or a wire key
//! (`"PermissionRequest"`, `"hookSpecificOutput"`) is as much a harness's
//! as a `match tool`. So the wake path's CLI modules also spell none of
//! the adapters' [`clank_core::harness::PROTOCOL_WORDS`], as a string or
//! inside one, except at the sites [`SPOKEN`] lists with why; a listed
//! site that no longer spells its word fails too.

use std::collections::BTreeMap;

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use syn::visit::Visit;

/// The wake path, as paths under `crates/`: the hooks, turn custody and
/// policy, the monitor, wait and recovery, with the files they include.
const WAKE_PATH: &[&str] = &[
    "cli/src/cli/stop_hook.rs",
    "cli/src/cli/grok_hook.rs",
    "cli/src/cli/question_hook.rs",
    "cli/src/cli/recovery_hook.rs",
    "cli/src/cli/turn_custody.rs",
    "cli/src/cli/turn_wire.rs",
    "cli/src/cli/monitor/mod.rs",
    "cli/src/cli/monitor/clients.rs",
    "cli/src/cli/monitor/connect.rs",
    "cli/src/cli/monitor/control.rs",
    "cli/src/cli/monitor/custody.rs",
    "cli/src/cli/monitor/fold.rs",
    "cli/src/cli/monitor/hosted.rs",
    "cli/src/cli/monitor/keeper.rs",
    "cli/src/cli/monitor/lease.rs",
    "cli/src/cli/monitor/hosted/owed.rs",
    "cli/src/cli/monitor/published.rs",
    "cli/src/cli/monitor/releases.rs",
    "cli/src/cli/monitor/retire.rs",
    "cli/src/cli/monitor/runtime.rs",
    "cli/src/cli/monitor/clients/serving.rs",
    "cli/src/cli/monitor/socket.rs",
    "cli/src/cli/monitor/spawn.rs",
    "cli/src/cli/monitor/custody/transport.rs",
    "cli/src/cli/monitor/user_commands.rs",
    "cli/src/cli/monitor/watchdog.rs",
    "cli/src/cli/monitor/wire.rs",
    "cli/src/cli/wait.rs",
    "cli/src/cli/recovery.rs",
    "core/src/turn_policy.rs",
    "core/src/turn_policy/child.rs",
    "core/src/wait.rs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// The tool is shown or named, and nothing is decided by it.
    Naming,
    /// Tool knowledge that is not hook semantics, kept in the CLI.
    OutOfScope,
}

/// Every wake-path function still deciding by `Tool`, as (path under
/// `crates/`, function, decision sites, class, why).
const INVENTORY: &[(&str, &str, usize, Class, &str)] = &[
    (
        "cli/src/cli/recovery.rs",
        "RecordReader::read",
        5,
        Class::OutOfScope,
        "transcript format: each harness's record is read by its own line shape, and Claude \
         closes a failed turn with the same marker as a finished one",
    ),
    (
        "cli/src/cli/recovery.rs",
        "record_source",
        3,
        Class::OutOfScope,
        "transcript location: where each harness keeps the record its turns are read from",
    ),
    (
        "cli/src/cli/recovery.rs",
        "stop_reported",
        3,
        Class::OutOfScope,
        "transcript format: the turn boundary a Stop is placed by is read from Claude's or \
         Codex's record",
    ),
    (
        "cli/src/cli/recovery.rs",
        "refresh",
        1,
        Class::OutOfScope,
        "transcript format: question results are read from Claude's record",
    ),
    (
        "cli/src/cli/recovery.rs",
        "question_evidence",
        1,
        Class::OutOfScope,
        "the question protocol is Claude's, read from its transcript",
    ),
    (
        "cli/src/cli/stop_hook.rs",
        "question_evidence",
        1,
        Class::OutOfScope,
        "the question protocol is Claude's: the prompts behind a Stop's turn are read from \
         its transcript",
    ),
    (
        "cli/src/cli/turn_custody.rs",
        "Gathered::of",
        1,
        Class::OutOfScope,
        "launch environment: a Grok agent's auto mode is read from its launch home",
    ),
];

/// Wake-path CLI functions that still spell a protocol word, as (path
/// under `crates/`, function, word, why).
const SPOKEN: &[(&str, &str, &str, &str)] = &[(
    "cli/src/cli/recovery.rs",
    "codex",
    "turn_id",
    "transcript format: a Codex rollout record names its turn by the same key its Stop does",
)];

const VARIANTS: &[&str] = &["Claude", "Codex", "Grok", "OpenCode"];

/// Whether `tokens` names a `Tool` variant as a path.
fn names_variant(tokens: &str) -> bool {
    VARIANTS.iter().any(|v| {
        let needle = format!("Tool :: {v}");
        tokens.match_indices(&needle).any(|(at, _)| {
            let before = tokens[..at].chars().next_back();
            let after = tokens[at + needle.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

#[derive(Default)]
struct Sites {
    /// The function being walked, innermost last.
    within: Vec<String>,
    impl_of: Vec<String>,
    /// Decision sites per function, in production code.
    found: BTreeMap<String, usize>,
    /// Protocol words each function spells, in production code.
    spoken: std::collections::BTreeSet<(String, String)>,
}

impl Sites {
    fn at(&self) -> String {
        self.within
            .last()
            .cloned()
            .unwrap_or_else(|| "<item>".into())
    }

    fn site(&mut self) {
        *self.found.entry(self.at()).or_default() += 1;
    }

    fn literal(&mut self, value: &str) {
        for word in clank_core::harness::PROTOCOL_WORDS {
            if value == *word || value.contains(&format!("\"{word}\"")) {
                self.spoken.insert((self.at(), word.to_string()));
            }
        }
    }

    fn literals_in(&mut self, tokens: proc_macro2::TokenStream) {
        for tree in tokens {
            match tree {
                proc_macro2::TokenTree::Group(g) => self.literals_in(g.stream()),
                proc_macro2::TokenTree::Literal(l) => {
                    if let Ok(s) = syn::parse_str::<syn::LitStr>(&l.to_string()) {
                        self.literal(&s.value());
                    }
                }
                _ => {}
            }
        }
    }

    fn named(&self, ident: &syn::Ident) -> String {
        match self.impl_of.last() {
            Some(ty) => format!("{ty}::{ident}"),
            None => ident.to_string(),
        }
    }
}

fn tokens(t: &impl quote::ToTokens) -> String {
    t.to_token_stream().to_string()
}

impl<'ast> Visit<'ast> for Sites {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !m.attrs.iter().any(is_cfg_test) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if i.attrs.iter().any(is_cfg_test) {
            return;
        }
        let ty = match &*i.self_ty {
            syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default(),
            other => tokens(other),
        };
        self.impl_of.push(ty);
        syn::visit::visit_item_impl(self, i);
        self.impl_of.pop();
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if f.attrs.iter().any(is_cfg_test) {
            return;
        }
        let impls = std::mem::take(&mut self.impl_of);
        self.within.push(f.sig.ident.to_string());
        syn::visit::visit_item_fn(self, f);
        self.within.pop();
        self.impl_of = impls;
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if f.attrs.iter().any(is_cfg_test) {
            return;
        }
        self.within.push(self.named(&f.sig.ident));
        syn::visit::visit_impl_item_fn(self, f);
        self.within.pop();
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        self.within.push(f.sig.ident.to_string());
        syn::visit::visit_trait_item_fn(self, f);
        self.within.pop();
    }

    fn visit_pat(&mut self, p: &'ast syn::Pat) {
        match names_variant(&tokens(p)) {
            true => self.site(),
            false => syn::visit::visit_pat(self, p),
        }
    }

    fn visit_expr_binary(&mut self, b: &'ast syn::ExprBinary) {
        if matches!(b.op, syn::BinOp::Eq(_) | syn::BinOp::Ne(_))
            && (names_variant(&tokens(&b.left)) || names_variant(&tokens(&b.right)))
        {
            self.site();
        }
        syn::visit::visit_expr_binary(self, b);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if m.path.is_ident("matches") && names_variant(&m.tokens.to_string()) {
            self.site();
        }
        self.literals_in(m.tokens.clone());
        syn::visit::visit_macro(self, m);
    }

    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        self.literal(&s.value());
    }
}

/// The decision sites per function of production source `src`, and the
/// protocol words each function spells.
fn scan(src: &str) -> Sites {
    let file = syn::parse_file(src).unwrap_or_else(|e| panic!("does not parse: {e}"));
    let mut sites = Sites::default();
    sites.visit_file(&file);
    sites
}

fn sites(src: &str) -> BTreeMap<String, usize> {
    scan(src).found
}

type Spoken = std::collections::BTreeSet<(String, String, String)>;

/// Every way the protocol words `spoken` disagree with `listed`.
fn spoken_violations(spoken: &Spoken, listed: &[(&str, &str, &str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, func, word) in spoken {
        if !listed
            .iter()
            .any(|(p, f, w, _)| p == path && f == func && w == word)
        {
            out.push(format!(
                "{path}: fn {func} spells the protocol word `{word}`; let its harness adapter \
                 (clank_core::harness) own it, or list why it is not hook protocol"
            ));
        }
    }
    for (path, func, word, why) in listed {
        assert!(!why.is_empty(), "every listed word says why");
        if !spoken.contains(&(path.to_string(), func.to_string(), word.to_string())) {
            out.push(format!(
                "{path}: fn {func} is listed for `{word}` but no longer spells it; remove it"
            ));
        }
    }
    out
}

/// Every way the scanned `found` disagrees with `inventory`.
fn violations(
    found: &BTreeMap<(String, String), usize>,
    inventory: &[(&str, &str, usize, Class, &str)],
) -> Vec<String> {
    let mut out = Vec::new();
    for ((path, func), n) in found {
        match inventory.iter().find(|(p, f, ..)| p == path && f == func) {
            None => out.push(format!(
                "{path}: fn {func} decides by `Tool` {n} time(s); ask the harness adapter \
                 (clank_core::harness) instead, or list it as naming or out of scope"
            )),
            Some((.., listed, _, _)) if listed != n => out.push(format!(
                "{path}: fn {func} decides by `Tool` {n} time(s), but {listed} are listed"
            )),
            Some(_) => {}
        }
    }
    for (path, func, ..) in inventory {
        if !found.contains_key(&(path.to_string(), func.to_string())) {
            out.push(format!(
                "{path}: fn {func} is listed but no longer decides by `Tool`; remove it"
            ));
        }
    }
    out
}

#[test]
fn the_wake_path_decides_by_tool_only_through_its_adapters() {
    let crates = crates_dir();
    let mut files = Vec::new();
    rs_files(&crates.join("cli/src"), &mut files);
    rs_files(&crates.join("core/src"), &mut files);
    let production = Production::of(&files);
    let mut found = BTreeMap::new();
    let mut spoken = Spoken::new();
    for rel in WAKE_PATH {
        let path = crates.join(rel);
        assert!(path.exists(), "{rel} is gone; update WAKE_PATH");
        if production.is_test_only(&path) {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let scanned = scan(&src);
        for (func, n) in scanned.found {
            found.insert((rel.to_string(), func), n);
        }
        if rel.starts_with("cli/") {
            spoken.extend(
                scanned
                    .spoken
                    .into_iter()
                    .map(|(func, word)| (rel.to_string(), func, word)),
            );
        }
    }
    for (_, _, _, _, why) in INVENTORY {
        assert!(!why.is_empty(), "every listed site says why");
    }
    let errors = violations(&found, INVENTORY);
    assert!(errors.is_empty(), "{errors:#?}");
    let errors = spoken_violations(&spoken, SPOKEN);
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn the_scanner_hears_a_protocol_word_in_a_string_or_a_macro_and_spares_tests() {
    let src = r##"
fn answer(text: &str) -> serde_json::Value {
    serde_json::json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "x": text}})
}

fn parks(event: &str) -> bool {
    event == "PermissionRequest"
}

fn wire() -> &'static str {
    r#"{"decision":"block"}"#
}

fn unrelated() -> &'static str {
    "hook: the PermissionRequest timed out"
}

#[cfg(test)]
mod tests {
    fn f() -> &'static str {
        "PreToolUse"
    }
}
"##;
    let spoken: Vec<(String, String)> = scan(&crate::common::strip_test_code(src))
        .spoken
        .into_iter()
        .collect();
    let want: Vec<(String, String)> = [
        ("answer", "SessionStart"),
        ("answer", "hookEventName"),
        ("answer", "hookSpecificOutput"),
        ("parks", "PermissionRequest"),
        ("wire", "decision"),
    ]
    .into_iter()
    .map(|(f, w)| (f.to_string(), w.to_string()))
    .collect();
    assert_eq!(spoken, want);
}

#[test]
fn an_unlisted_or_vanished_protocol_word_is_a_violation() {
    let spoken: Spoken = [
        ("cli/src/cli/x.rs", "kept", "turn_id"),
        ("cli/src/cli/x.rs", "new", "SessionStart"),
    ]
    .into_iter()
    .map(|(p, f, w)| (p.to_string(), f.to_string(), w.to_string()))
    .collect();
    let listed: &[(&str, &str, &str, &str)] = &[
        ("cli/src/cli/x.rs", "kept", "turn_id", "a transcript"),
        ("cli/src/cli/x.rs", "gone", "decision", "a transcript"),
    ];
    let errors = spoken_violations(&spoken, listed);
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert!(
        errors
            .iter()
            .any(|e| e.contains("fn new spells") && e.contains("SessionStart"))
    );
    assert!(errors.iter().any(|e| e.contains("fn gone is listed")));
}

#[test]
fn the_scanner_sees_each_kind_of_decision_and_spares_the_rest() {
    let src = r#"
fn arms(tool: Tool) -> u8 {
    match tool {
        Tool::Claude | Tool::Grok => 1,
        clank_core::Tool::Codex => 2,
        _ => 3,
    }
}

fn compares(tool: Tool) -> bool {
    tool == Tool::Grok || Tool::OpenCode != tool
}

fn matched(b: Option<(Tool, u8)>) -> bool {
    matches!(b, Some((Tool::Claude, _)))
}

fn let_else(tool: Tool) {
    let (Tool::Claude, Some(_)) = (tool, Some(1)) else {
        return;
    };
}

fn passes_it_along() {
    bind(Tool::Grok);
    let pair = (Tool::Codex, 1);
    let _ = MyTool::Claude == x;
}

struct Reader;
impl Reader {
    fn read(&self) {
        if self.tool == Tool::Claude {}
    }
}

#[cfg(test)]
mod tests {
    fn f(t: Tool) -> bool {
        t == Tool::Claude
    }
}
"#;
    let found = sites(&crate::common::strip_test_code(src));
    let expected: BTreeMap<String, usize> = [
        ("arms", 2),
        ("compares", 2),
        ("matched", 1),
        ("let_else", 1),
        ("Reader::read", 1),
    ]
    .into_iter()
    .map(|(f, n)| (f.to_string(), n))
    .collect();
    assert_eq!(found, expected);
}

#[test]
fn an_unlisted_extra_or_vanished_site_is_a_violation() {
    let key = |f: &str| ("cli/src/cli/x.rs".to_string(), f.to_string());
    let found: BTreeMap<_, _> = [(key("kept"), 1), (key("grown"), 2), (key("new"), 1)].into();
    let inventory: &[(&str, &str, usize, Class, &str)] = &[
        ("cli/src/cli/x.rs", "kept", 1, Class::Naming, "a label"),
        (
            "cli/src/cli/x.rs",
            "grown",
            1,
            Class::OutOfScope,
            "a transcript",
        ),
        (
            "cli/src/cli/x.rs",
            "gone",
            1,
            Class::OutOfScope,
            "a transcript",
        ),
    ];
    let errors = violations(&found, inventory);
    assert_eq!(errors.len(), 3, "{errors:#?}");
    assert!(errors.iter().any(|e| e.contains("fn new decides")));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("fn grown") && e.contains("1 are listed"))
    );
    assert!(errors.iter().any(|e| e.contains("fn gone is listed")));
}
