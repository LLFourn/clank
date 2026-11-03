//! Enforcement gate (every-subprocess-goes-through-one-module):
//! PRODUCTION code outside `proc_run.rs` must not name a process
//! `Command` or `Child`, std's or tokio's, nor `CommandExt`'s
//! `pre_exec`/`exec`.
//!
//! Every spawn has to go through the one module that marks the child's
//! descriptors close-on-exec and bounds what it waits for. A spawn
//! made anywhere else is the bug this exists to prevent: a long-lived
//! child that inherited another call's output pipe held it open
//! forever, and an unbounded read waiting for EOF never returned.
//!
//! It parses the source and walks every use tree, path and macro body,
//! so a nested group, an alias or a glob is caught by structure rather
//! than by spelling: an aliased import is the easiest way past a gate
//! that only knows `std::process::Command`.

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use syn::visit::Visit;

/// The one file that may spawn.
const LAYER: &str = "cli/src/proc_run.rs";

fn exempt(path: &std::path::Path) -> bool {
    path.to_string_lossy().replace('\\', "/").ends_with(LAYER)
}

const SPAWNERS: &[&str] = &["Command", "Child", "CommandExt"];

const PROCESS_CRATES: &[&str] = &["std", "tokio"];

#[derive(Default)]
struct Gate {
    found: Vec<String>,
    line: usize,
}

impl Gate {
    fn flag(&mut self, what: &str, at: String) {
        self.found
            .push(format!("line {}: {what}: `{at}`", self.line));
    }

    fn at(&mut self, span: proc_macro2::Span) {
        self.line = span.start().line;
    }

    /// One leaf of a use tree: the full path it imports, and whether it
    /// is renamed.
    fn leaf(&mut self, path: &[String], renamed: bool) {
        let shown = path.join("::");
        let last = path.last().map(String::as_str).unwrap_or("");
        let parent = path.len().checked_sub(2).map(|i| path[i].as_str());
        if SPAWNERS.contains(&last) && (parent == Some("process") || last == "CommandExt") {
            self.flag("a process `Command`/`Child` imported", shown);
        } else if last == "process"
            && renamed
            && path
                .first()
                .is_some_and(|c| PROCESS_CRATES.contains(&c.as_str()))
        {
            self.flag("a process module imported under an alias", shown);
        }
    }

    fn tree(&mut self, prefix: &mut Vec<String>, tree: &syn::UseTree) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.tree(prefix, &p.tree);
                prefix.pop();
            }
            syn::UseTree::Name(n) => self.named(prefix, &n.ident, false),
            syn::UseTree::Rename(r) => self.named(prefix, &r.ident, true),
            syn::UseTree::Glob(_) => {
                if prefix.last().is_some_and(|l| l == "process") {
                    self.flag("a glob import of a process module", prefix.join("::"));
                }
            }
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    self.tree(prefix, item);
                }
            }
        }
    }

    fn named(&mut self, prefix: &[String], ident: &syn::Ident, renamed: bool) {
        let mut path = prefix.to_vec();
        if ident != "self" {
            path.push(ident.to_string());
        }
        self.leaf(&path, renamed);
    }

    fn segments(&mut self, segs: &[String]) {
        for w in segs.windows(2) {
            if w[0] == "process" && SPAWNERS.contains(&w[1].as_str()) {
                self.flag("a process `Command`/`Child` named", segs.join("::"));
                return;
            }
        }
        if segs.iter().any(|s| s == "CommandExt") {
            self.flag("`CommandExt` named", segs.join("::"));
        }
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_cfg_test)
}

/// The attributes an expression statement can carry.
fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    use syn::Expr::*;
    match expr {
        Call(e) => &e.attrs,
        MethodCall(e) => &e.attrs,
        Macro(e) => &e.attrs,
        Block(e) => &e.attrs,
        Unsafe(e) => &e.attrs,
        Path(e) => &e.attrs,
        Assign(e) => &e.attrs,
        If(e) => &e.attrs,
        Match(e) => &e.attrs,
        Let(e) => &e.attrs,
        ForLoop(e) => &e.attrs,
        While(e) => &e.attrs,
        Loop(e) => &e.attrs,
        Await(e) => &e.attrs,
        Try(e) => &e.attrs,
        Return(e) => &e.attrs,
        _ => &[],
    }
}

impl<'ast> Visit<'ast> for Gate {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !test_only(item_attrs(item)) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Const(i) => &i.attrs,
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Type(i) => &i.attrs,
            syn::ImplItem::Macro(i) => &i.attrs,
            _ => return syn::visit::visit_impl_item(self, item),
        };
        if !test_only(attrs) {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs = match item {
            syn::TraitItem::Const(i) => &i.attrs,
            syn::TraitItem::Fn(i) => &i.attrs,
            syn::TraitItem::Type(i) => &i.attrs,
            syn::TraitItem::Macro(i) => &i.attrs,
            _ => return syn::visit::visit_trait_item(self, item),
        };
        if !test_only(attrs) {
            syn::visit::visit_trait_item(self, item);
        }
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        if !test_only(&field.attrs) {
            syn::visit::visit_field(self, field);
        }
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if !test_only(&field.attrs) {
            syn::visit::visit_field_value(self, field);
        }
    }

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if !test_only(&variant.attrs) {
            syn::visit::visit_variant(self, variant);
        }
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !test_only(&arm.attrs) {
            syn::visit::visit_arm(self, arm);
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(l) => &l.attrs[..],
            syn::Stmt::Macro(m) => &m.attrs[..],
            syn::Stmt::Expr(e, _) => expr_attrs(e),
            syn::Stmt::Item(_) => &[],
        };
        if !test_only(attrs) {
            syn::visit::visit_stmt(self, stmt);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.at(item.use_token.span);
        self.tree(&mut Vec::new(), &item.tree);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(first) = path.segments.first() {
            self.at(first.ident.span());
        }
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        self.segments(&segs);
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.at(call.method.span());
        if call.method == "pre_exec" {
            self.flag("`CommandExt::pre_exec` called", call.method.to_string());
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    /// Macro bodies are tokens, not syntax: read as identifier runs.
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        fn walk(tokens: proc_macro2::TokenStream, run: &mut Vec<String>, gate: &mut Gate) {
            for token in tokens {
                match token {
                    proc_macro2::TokenTree::Ident(i) => {
                        gate.at(i.span());
                        let i = i.to_string();
                        if i == "pre_exec" {
                            gate.flag("`CommandExt::pre_exec` called", i.clone());
                        }
                        run.push(i);
                    }
                    proc_macro2::TokenTree::Punct(p) if p.as_char() == ':' => {}
                    proc_macro2::TokenTree::Group(g) => {
                        gate.segments(run);
                        run.clear();
                        walk(g.stream(), &mut Vec::new(), gate);
                    }
                    _ => {
                        gate.segments(run);
                        run.clear();
                    }
                }
            }
            gate.segments(run);
            run.clear();
        }
        walk(mac.tokens.clone(), &mut Vec::new(), self);
        syn::visit::visit_macro(self, mac);
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    use syn::Item::*;
    match item {
        Const(i) => &i.attrs,
        Enum(i) => &i.attrs,
        ExternCrate(i) => &i.attrs,
        Fn(i) => &i.attrs,
        ForeignMod(i) => &i.attrs,
        Impl(i) => &i.attrs,
        Macro(i) => &i.attrs,
        Mod(i) => &i.attrs,
        Static(i) => &i.attrs,
        Struct(i) => &i.attrs,
        Trait(i) => &i.attrs,
        TraitAlias(i) => &i.attrs,
        Type(i) => &i.attrs,
        Union(i) => &i.attrs,
        Use(i) => &i.attrs,
        _ => &[],
    }
}

/// Every violation in a source file's production code: anything under
/// `#[cfg(test)]` is skipped.
fn violations(src: &str) -> Vec<String> {
    let file = match syn::parse_file(src) {
        Ok(file) => file,
        Err(e) => return vec![format!("does not parse, so it cannot be checked: {e}")],
    };
    let mut gate = Gate::default();
    gate.visit_file(&file);
    gate.found
}

#[test]
fn no_process_is_spawned_outside_proc_run() {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir())
        .expect("read crates/")
        .flatten()
    {
        let src = entry.path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty(), "scanned no source files");
    assert!(
        files.iter().any(|f| f.to_string_lossy().ends_with(LAYER)),
        "the layer itself was not found, so the exemption names nothing"
    );

    let layer = files.iter().find(|f| exempt(f)).expect("found above");
    assert!(
        !violations(&std::fs::read_to_string(layer).unwrap()).is_empty(),
        "the scanner finds nothing even in the layer that spawns"
    );

    let production = Production::of(&files);
    let mut found: Vec<String> = Vec::new();
    for f in files
        .iter()
        .filter(|f| !exempt(f) && !production.is_test_only(f))
    {
        for what in violations(&std::fs::read_to_string(f).unwrap()) {
            found.push(format!("{} — {what}", f.display()));
        }
    }
    assert!(
        found.is_empty(),
        "process spawning outside proc_run ({} site(s)). Build a \
         `proc_run::Spec` and use its `run`/`start`/`interactive`/`exec`, \
         so the child inherits no descriptor it was not given and every \
         wait is bounded:\n  {}",
        found.len(),
        found.join("\n  ")
    );
}

#[test]
fn every_spelling_of_a_spawn_is_caught_and_tests_are_exempt() {
    let count = |src: &str| violations(src).len();

    for (what, src) in [
        ("use", "use std::process::Command;\n"),
        ("aliased", "use tokio::process::Command as C;\n"),
        (
            "qualified",
            "fn f() { std::process::Command::new(\"x\"); }\n",
        ),
        (
            "glob",
            "use std::process::*;\nfn f() { Command::new(\"x\"); }\n",
        ),
        (
            "group",
            "use std::process::{\n    Stdio,\n    Command,\n};\n",
        ),
        ("child", "struct S(tokio::process::Child);\n"),
        ("ext", "use std::os::unix::process::CommandExt;\n"),
        (
            "pre_exec",
            "fn f(c: &mut C) { unsafe { c.pre_exec(|| Ok(())) }; }\n",
        ),
        (
            "module alias",
            "use std::process as p;\nfn f() { p::Command::new(\"x\"); }\n",
        ),
        (
            "grouped module alias",
            "use std::{process as p};\nfn f() { p::Command::new(\"x\"); }\n",
        ),
        ("nested self alias", "use std::{process::{self as q}};\n"),
        ("tokio grouped alias", "use tokio::{process as p};\n"),
        (
            "tokio nested self alias",
            "use tokio::{process::{self as q}};\n",
        ),
        (
            "nested group",
            "use std::{io, process::{Stdio, Command}};\n",
        ),
        ("tokio nested group", "use tokio::{process::{Child}};\n"),
        ("grouped glob", "use std::{process::*};\n"),
        (
            "in a macro",
            "fn f() { let _ = vec![std::process::Command::new(\"x\")]; }\n",
        ),
    ] {
        assert!(count(src) >= 1, "{what} was not caught: {src}");
    }

    for allowed in [
        "use std::process::ExitStatus;\n",
        "fn f() { std::process::exit(1) }\n",
        "fn f(s: tokio::process::ChildStdin) {}\n",
        "fn f() { crate::proc_run::Spec::new(\"x\"); }\n",
        "// we never call std::process::Command here\n",
        "use std::process;\nfn f() { process::exit(1) }\n",
        "use std::{process::{self, ExitStatus}};\n",
        "fn f() { println!(\"process Command\"); }\n",
    ] {
        assert_eq!(count(allowed), 0, "flagged: {allowed}");
    }

    let test_mod = "#[cfg(test)]\nmod tests {\n    use std::process::Command;\n}\n";
    assert_eq!(count(test_mod), 0, "test code is exempt");
    let mixed = "impl S {\n    #[cfg(test)]\n    fn t() { std::process::Command::new(\"x\"); }\n    \
                 fn p() {\n        #[cfg(test)]\n        let _ = std::process::Command::new(\"y\");\n        \
                 std::process::Command::new(\"z\");\n    }\n}\n";
    assert_eq!(
        count(mixed),
        1,
        "only the production line around test code counts"
    );
}
