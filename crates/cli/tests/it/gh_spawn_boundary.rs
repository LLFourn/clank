//! Enforcement gate (github-is-polled-only): production code runs `gh`
//! in exactly one place, `gh auth token` in `github/session.rs`, for a
//! token when neither `GH_TOKEN` nor `GITHUB_TOKEN` is set.
//!
//! Everything else GitHub answers over the HTTP session. A `gh` spawn
//! elsewhere is how the webhook relay came to hold a repo's one
//! forwarding slot past its own death, and how a PR title came to need
//! the CLI installed and logged in.
//!
//! Every spawn goes through `proc_run::Spec::new` (`process_boundary`
//! enforces it), so a `gh` spawn is a `Spec::new` whose program is the
//! literal `gh`, or a constant holding it.

use crate::common::{Production, crates_dir, is_cfg_test, rs_files};
use syn::visit::Visit;

/// The one file allowed to spawn `gh`, and the arguments it may pass.
const TOKEN_SITE: &str = "cli/src/cli/github/session.rs";
const TOKEN_ARGS: &[&str] = &["auth", "token"];

#[derive(Debug, PartialEq)]
struct Spawn {
    line: usize,
    args: Vec<String>,
}

#[derive(Default)]
struct Gate {
    spawns: Vec<Spawn>,
    /// Constants whose value is the program name, which a spawn could
    /// name instead of the literal.
    named: Vec<usize>,
}

fn is_gh(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => {
            let v = s.value();
            v == "gh" || v.ends_with("/gh")
        }
        syn::Expr::Reference(r) => is_gh(&r.expr),
        _ => false,
    }
}

/// `…Spec::new("gh")`, at the line it is written on.
fn gh_spec(expr: &syn::Expr) -> Option<usize> {
    let syn::Expr::Call(call) = expr else {
        return None;
    };
    let syn::Expr::Path(path) = &*call.func else {
        return None;
    };
    let segs: Vec<String> = path
        .path
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    let named_spec =
        segs.len() >= 2 && segs[segs.len() - 2] == "Spec" && segs[segs.len() - 1] == "new";
    (named_spec && call.args.first().is_some_and(is_gh))
        .then(|| path.path.segments[0].ident.span().start().line)
}

fn literals(expr: &syn::Expr, out: &mut Vec<String>) {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => out.push(s.value()),
        syn::Expr::Array(a) => a.elems.iter().for_each(|e| literals(e, out)),
        syn::Expr::Reference(r) => literals(&r.expr, out),
        _ => out.push("<not a literal>".into()),
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_cfg_test)
}

impl<'ast> Visit<'ast> for Gate {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = match item {
            syn::Item::Fn(i) => &i.attrs,
            syn::Item::Mod(i) => &i.attrs,
            syn::Item::Impl(i) => &i.attrs,
            syn::Item::Const(i) => &i.attrs,
            syn::Item::Static(i) => &i.attrs,
            syn::Item::Struct(i) => &i.attrs,
            syn::Item::Enum(i) => &i.attrs,
            syn::Item::Trait(i) => &i.attrs,
            syn::Item::Macro(i) => &i.attrs,
            syn::Item::Use(i) => &i.attrs,
            _ => &Vec::new(),
        };
        if !test_only(attrs) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Fn(i) => &i.attrs,
            syn::ImplItem::Const(i) => &i.attrs,
            _ => &Vec::new(),
        };
        if !test_only(attrs) {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(l) => &l.attrs[..],
            syn::Stmt::Macro(m) => &m.attrs[..],
            _ => &[],
        };
        if !test_only(attrs) {
            syn::visit::visit_stmt(self, stmt);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let mut chain = vec![call];
        let mut root = &*call.receiver;
        while let syn::Expr::MethodCall(inner) = root {
            chain.push(inner);
            root = &*inner.receiver;
        }
        let Some(line) = gh_spec(root) else {
            return syn::visit::visit_expr_method_call(self, call);
        };
        let mut args = Vec::new();
        for link in chain.iter().rev() {
            if link.method == "arg" || link.method == "args" {
                link.args.iter().for_each(|a| literals(a, &mut args));
            }
        }
        self.spawns.push(Spawn { line, args });
        for link in &chain {
            for a in &link.args {
                self.visit_expr(a);
            }
        }
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        match gh_spec(expr) {
            Some(line) => self.spawns.push(Spawn {
                line,
                args: Vec::new(),
            }),
            None => syn::visit::visit_expr(self, expr),
        }
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if is_gh(&item.expr) {
            self.named.push(item.ident.span().start().line);
        }
        syn::visit::visit_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        if is_gh(&item.expr) {
            self.named.push(item.ident.span().start().line);
        }
        syn::visit::visit_item_static(self, item);
    }
}

fn scan(src: &str) -> Gate {
    let file = syn::parse_file(src).expect("source parses");
    let mut gate = Gate::default();
    gate.visit_file(&file);
    gate
}

/// What is wrong with one file's spawns, given whether it is the token
/// site.
fn violations(src: &str, token_site: bool) -> Vec<String> {
    let gate = scan(src);
    let mut out: Vec<String> = gate
        .named
        .iter()
        .map(|line| format!("line {line}: a constant names the gh program"))
        .collect();
    let allowed = |s: &Spawn| token_site && s.args == TOKEN_ARGS;
    for s in gate.spawns.iter().filter(|s| !allowed(s)) {
        out.push(format!("line {}: gh {}", s.line, s.args.join(" ")));
    }
    if token_site && gate.spawns.iter().filter(|s| allowed(s)).count() != 1 {
        out.push("the token site must run `gh auth token` exactly once".into());
    }
    out
}

#[test]
fn gh_runs_only_for_the_token() {
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
    let site = |f: &std::path::Path| f.to_string_lossy().replace('\\', "/").ends_with(TOKEN_SITE);
    assert!(
        files.iter().any(|f| site(f)),
        "the token site was not found, so the allowance names nothing"
    );
    let production = Production::of(&files);
    let mut found = Vec::new();
    for f in files.iter().filter(|f| !production.is_test_only(f)) {
        for v in violations(&std::fs::read_to_string(f).unwrap(), site(f)) {
            found.push(format!("{} — {v}", f.display()));
        }
    }
    assert!(
        found.is_empty(),
        "gh spawned outside `gh auth token` in github/session.rs ({} site(s)). \
         Read GitHub over the HTTP session instead:\n  {}",
        found.len(),
        found.join("\n  ")
    );
}

#[test]
fn every_other_gh_spawn_is_caught() {
    let token = r#"fn t() { crate::proc_run::Spec::new("gh").args(["auth", "token"]).stdout(x); }"#;
    assert!(violations(token, true).is_empty(), "the one allowed spawn");
    assert_eq!(
        violations(token, false).len(),
        1,
        "allowed only at the token site"
    );
    for (what, src) in [
        (
            "pr view",
            r#"fn f() { Spec::new("gh").cwd(d).args(["pr", "view", "7", "--json", "title"]); }"#,
        ),
        (
            "webhook forward",
            r#"fn f() { let spec = Spec::new("gh").args(["webhook", "forward", "--repo", r]); }"#,
        ),
        (
            "an extension",
            r#"fn f() { Spec::new("gh").arg("webhook"); }"#,
        ),
        ("bare", r#"fn f() { let s = proc_run::Spec::new("gh"); }"#),
        (
            "a path",
            r#"fn f() { Spec::new("/opt/homebrew/bin/gh").args(["api", "x"]); }"#,
        ),
        ("a constant", r#"const GH: &str = "gh";"#),
        (
            "in a macro-free closure",
            r#"fn f() { let run = || Spec::new("gh").args(["api", "user"]); }"#,
        ),
    ] {
        assert!(
            !violations(src, true).is_empty(),
            "{what} was not caught: {src}"
        );
    }
    let extra = format!("{token}\nfn g() {{ Spec::new(\"gh\").args([\"auth\", \"token\"]); }}");
    assert!(
        !violations(&extra, true).is_empty(),
        "a second token spawn is a second site"
    );
    for allowed in [
        r#"fn f() { let badge = "gh"; }"#,
        r#"#[cfg(test)] mod tests { fn f() { Spec::new("gh").args(["pr", "view"]); } }"#,
        r#"fn f() { Spec::new("git").args(["fetch"]); }"#,
        r#"fn f() { Spec::new(program).args(["gh"]); }"#,
    ] {
        assert!(violations(allowed, false).is_empty(), "flagged: {allowed}");
    }
}
