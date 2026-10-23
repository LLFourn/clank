//! Enforcement gate: PRODUCTION code must not create a path directly
//! under a repo's `.clank/` unless that path is declared here.
//!
//! `.clank/` holds three different kinds of thing and only one of them
//! belongs to the repo. Plans and finished plans are tracked. Config,
//! agents and queues are local but durable. And a monitor's descriptor
//! and leases were PROCESS state — meaningless once their process is
//! gone, yet sitting in the working tree, where whether git could see
//! them depended on each repo's `.clank/.gitignore`.
//!
//! That file propagates by being TRACKED, so a fork from a repo that
//! never had one starts dirty and nothing in the new worktree fixes
//! it. Two files appeared in `git status` on every new team made this
//! way, and the allow-list that would have covered them could not
//! reach repos that had never adopted it.
//!
//! The fix was to stop runtime state entering the repo at all. This
//! gate is what keeps it out: a new one becomes a build failure rather
//! than somebody's dirty status months later.
//!
//! A REPO's `.clank/`, specifically. `~/.clank/` is spelled the same
//! way and is a different thing: the user's own config directory, not
//! a working tree, so nothing there can ever reach a `git status` and
//! `~/.clank/remote.lock` is where a lock belongs. Those sites are
//! DECLARED in [`HOME_SITES`], one at a time.
//!
//! What a gate like this is actually for is the spelling somebody
//! reaches for next, so it reads the whole file rather than single
//! lines: the first version matched one formatting of one idiom, and
//! `repo.join(".clank/monitor.json")`, a chain rustfmt had split
//! across two lines, and a repo variable that happened to be called
//! `repo_home` all walked past it (codex on 9dfc3e3).

use crate::common::{Production, crates_dir, rs_files};

/// What production code may create directly under a repo's `.clank/`.
///
/// DIRECT children only. A path deeper inside one of these — a
/// per-agent file under `agents/`, say — is that subtree's business.
///
/// Declaring one says it may LIVE IN THE REPO, for one of two reasons.
/// Most are local but durable — they belong to the repo even though
/// they are not committed — and so they do need an ignore rule, which
/// means a repo with the legacy deny-list will still show them. That
/// migration is deliberately a separate job.
///
/// `cache`, `html` and `zellij` are the other reason: arguably the
/// same process-state category as the files this gate was built for,
/// but directories with their own lifecycles, deferred on purpose
/// rather than lumped in. Declaring one is therefore not a claim that
/// it belongs here for ever.
///
/// What this gate exists to stop is the category no ignore rule should
/// ever have been the answer for: state that belongs to a PROCESS and
/// means nothing once that process is gone.
const DECLARED: &[&str] = &[
    // Fork provenance: the main repo the clone was cut from, its
    // branch and pinned base. Written once, read long after — the
    // repo's own history, not a running process's.
    "fork.json",
    // Uploads the web bridge's messages refer to. They outlive every
    // process that touches them.
    "attachments",
    ".gitignore",
    "config.json",
    "agents",
    "plans",
    "finished",
    "queue",
    "drafts",
    "blocks",
    "stash",
    "shelved",
    "cache",
    "html",
    "zellij",
    "worktrees",
    "clones",
];

/// Where production code reaches the USER's `~/.clank` instead of a
/// repo's, as (file, receiver, why).
///
/// Declared site by site. The first version of this gate exempted any
/// receiver whose NAME contained "home", which silently exempted a
/// repo variable called `repo_home` as well — a storage owner inferred
/// from an arbitrary local name is not a boundary.
const HOME_SITES: &[(&str, &str, &str)] = &[
    (
        "web-cloudflare/src/door.rs",
        "home",
        "the user's own config directory, which is not a working tree, so \
         nothing in it can reach a `git status`",
    ),
    (
        "web-cloudflare/src/tunnel.rs",
        "home",
        "`~/.clank/remote/<identity>.lock` — a tunnel lease, per USER and \
         not per repo, which is why it is not in a repo",
    ),
    (
        "cli/src/cli/setup.rs",
        "home",
        "`~/.clank/setup-manifest.json` — what setup last installed where, \
         read to tell its own old output from a user's edit",
    ),
    (
        "cli/src/cli/user_command/store.rs",
        "home",
        "`~/.clank/user-commands/` — the user-command queue and attempt \
         ledger, per USER and outside every repo so agents have no reason to \
         edit it, and durable across reboots so replay state survives",
    ),
    (
        "cli/src/cli/setup.rs",
        "domain",
        "`<domain>/.clank/setup-install.lock` — setup's installation domain is \
         the shared manifest home or a provider's own home, never a repo, and \
         its lock serializes installs into that home",
    ),
];

/// The sites that join a COMPUTED child, with the reason each is not a
/// new direct child in disguise.
///
/// Declared rather than ignored. A computed child is exactly where an
/// undeclared path can hide, so the gate refuses to resolve one
/// silently: it names the site and makes somebody write down why.
const COMPUTED: &[(&str, &str, &str)] = &[
    (
        "cli/src/repo_watch.rs",
        "d",
        "iterates `CLANK_WAKE_DIRS`, all declared, and only watches them",
    ),
    (
        "cli/src/cli/status.rs",
        "dir",
        "iterates `CLANK_WAKE_DIRS`, all declared, and only stats them",
    ),
    (
        "cli/src/cli/rewire.rs",
        "&dst_rel",
        "`canonical_feedback_path` returns `agents/<label>/feedback/<ref>.md`, \
         so the direct child is the declared `agents`",
    ),
];

/// How a site names a child of `.clank/`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Child {
    /// A literal, whether `.join("plans")` or `join(".clank/plans")`.
    Named(String),
    /// Built from an expression, which this gate does not evaluate.
    Computed(String),
    /// Inside macro syntax the gate cannot read as expressions, which
    /// names `.clank` all the same.
    Unreadable,
}

/// Whether (file, receiver) is a declared `~/.clank` site.
fn is_home_site(file: &str, receiver: &str) -> bool {
    HOME_SITES
        .iter()
        .any(|(f, r, _)| file.ends_with(f) && *r == receiver)
}

/// Every child of a repo's `.clank/` the source builds, with its 1-based
/// line, read from the PARSED source.
///
/// A text scan cannot tell a brace in a string from the end of a block,
/// or a shadowing `let dir` from the binding it replaces, and both
/// decided whether a site was reported. This walks the syntax tree
/// instead, with one scope per block: a local is a `.clank` root exactly
/// while the binding that made it one is the binding in scope. Test code
/// is skipped by its `#[cfg(test)]`, and macro arguments are read as the
/// expressions they almost always are, so `format!`/`assert!` hide
/// nothing. A file that does not parse is reported, not passed.
fn children(src: &str, file: &str) -> Result<Vec<(usize, Child)>, String> {
    let parsed = syn::parse_file(src).map_err(|e| format!("does not parse: {e}"))?;
    let mut scan = Scan {
        file,
        scopes: vec![Default::default()],
        found: Vec::new(),
    };
    for item in &parsed.items {
        syn::visit::Visit::visit_item(&mut scan, item);
    }
    let mut found = scan.found;
    found.sort_by_key(|(line, _)| *line);
    found.dedup();
    Ok(found)
}

struct Scan<'a> {
    file: &'a str,
    /// Innermost last: each local name, and whether it holds a `.clank` root.
    scopes: Vec<std::collections::HashMap<String, bool>>,
    found: Vec<(usize, Child)>,
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
    })
}

fn str_lit(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Some(s.value()),
        _ => None,
    }
}

/// The name a receiver goes by, as `HOME_SITES` spells it.
fn receiver_name(e: &syn::Expr) -> String {
    match e {
        syn::Expr::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        syn::Expr::Field(f) => match &f.member {
            syn::Member::Named(i) => i.to_string(),
            syn::Member::Unnamed(_) => String::new(),
        },
        syn::Expr::Reference(r) => receiver_name(&r.expr),
        syn::Expr::Paren(p) => receiver_name(&p.expr),
        _ => String::new(),
    }
}

fn line_of(span: proc_macro2::Span) -> usize {
    span.start().line
}

impl Scan<'_> {
    fn bound_root(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.get(name))
            .copied()
            .unwrap_or(false)
    }

    fn bind(&mut self, pat: &syn::Pat, root: bool) {
        match pat {
            syn::Pat::Ident(p) => {
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(p.ident.to_string(), root);
                if let Some((_, sub)) = &p.subpat {
                    self.bind(sub, false);
                }
            }
            syn::Pat::Type(t) => self.bind(&t.pat, root),
            syn::Pat::Reference(r) => self.bind(&r.pat, root),
            syn::Pat::Paren(p) => self.bind(&p.pat, root),
            syn::Pat::Tuple(t) => t.elems.iter().for_each(|p| self.bind(p, false)),
            syn::Pat::TupleStruct(t) => t.elems.iter().for_each(|p| self.bind(p, false)),
            syn::Pat::Slice(s) => s.elems.iter().for_each(|p| self.bind(p, false)),
            syn::Pat::Or(o) => o.cases.iter().for_each(|p| self.bind(p, false)),
            syn::Pat::Struct(s) => s.fields.iter().for_each(|f| self.bind(&f.pat, false)),
            _ => {}
        }
    }

    /// Whether `e` evaluates to a repo's `.clank` root.
    fn is_root(&self, e: &syn::Expr) -> bool {
        match e {
            syn::Expr::Path(p) => p
                .path
                .get_ident()
                .is_some_and(|i| self.bound_root(&i.to_string())),
            syn::Expr::Call(c) => matches!(&*c.func, syn::Expr::Path(p)
                if p.path.segments.last().is_some_and(|s| s.ident == "clank_dir")),
            syn::Expr::MethodCall(m) if m.method == "join" && m.args.len() == 1 => {
                str_lit(&m.args[0]).as_deref() == Some(".clank")
                    && !is_home_site(self.file, &receiver_name(&m.receiver))
            }
            syn::Expr::Reference(r) => self.is_root(&r.expr),
            syn::Expr::Paren(p) => self.is_root(&p.expr),
            _ => false,
        }
    }

    /// An `if`/`while` condition, binding each `let` in it, a let chain's
    /// included, into the current scope: the one its body runs in.
    fn visit_condition(&mut self, cond: &syn::Expr) {
        use syn::visit::Visit;
        match cond {
            syn::Expr::Let(l) => {
                self.visit_expr(&l.expr);
                let root = self.is_root(&l.expr);
                self.bind(&l.pat, root);
            }
            syn::Expr::Binary(b) if matches!(b.op, syn::BinOp::And(_)) => {
                self.visit_condition(&b.left);
                self.visit_condition(&b.right);
            }
            other => self.visit_expr(other),
        }
    }

    /// The line of the first thing in `tokens`, groups included, that
    /// reaches a repo's `.clank` root: a `.clank` literal, a local bound to
    /// the root here, or the `clank_dir` accessor.
    fn reaches_root(&self, tokens: proc_macro2::TokenStream) -> Option<usize> {
        use proc_macro2::TokenTree;
        tokens.into_iter().find_map(|tt| match tt {
            TokenTree::Group(g) => self.reaches_root(g.stream()),
            TokenTree::Literal(l) if l.to_string().contains(".clank") => Some(line_of(l.span())),
            TokenTree::Ident(i) if i == "clank_dir" || self.bound_root(&i.to_string()) => {
                Some(line_of(i.span()))
            }
            _ => None,
        })
    }

    /// Macro arguments: each piece between `,`, `;` and `=>` that parses
    /// as an expression is visited as one. A piece that does not is
    /// searched inside its groups, and reported if anything in it reaches
    /// the root, whether by a `.clank` literal or through a binding: syntax
    /// this gate cannot read must not pass as safe.
    fn visit_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        use proc_macro2::{Spacing, TokenTree};
        use syn::visit::Visit;
        let mut pieces: Vec<Vec<TokenTree>> = vec![Vec::new()];
        for tt in tokens {
            let piece = pieces.last_mut().unwrap();
            match &tt {
                TokenTree::Punct(p) if matches!(p.as_char(), ',' | ';') => pieces.push(Vec::new()),
                TokenTree::Punct(p)
                    if p.as_char() == '>'
                        && matches!(piece.last(), Some(TokenTree::Punct(eq))
                            if eq.as_char() == '=' && eq.spacing() == Spacing::Joint) =>
                {
                    piece.pop();
                    pieces.push(Vec::new());
                }
                _ => piece.push(tt),
            }
        }
        for piece in pieces.into_iter().filter(|p| !p.is_empty()) {
            let stream: proc_macro2::TokenStream = piece.iter().cloned().collect();
            if let Ok(e) = syn::parse2::<syn::Expr>(stream) {
                self.visit_expr(&e);
                continue;
            }
            for tt in &piece {
                if let TokenTree::Group(g) = tt {
                    self.visit_tokens(g.stream());
                }
            }
            if let Some(line) = self.reaches_root(piece.iter().cloned().collect()) {
                self.found.push((line, Child::Unreadable));
            }
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.scopes.push(Default::default());
        f(self);
        self.scopes.pop();
    }
}

impl<'ast> syn::visit::Visit<'ast> for Scan<'_> {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        let attrs: &[syn::Attribute] = match i {
            syn::Item::Fn(f) => &f.attrs,
            syn::Item::Mod(m) => &m.attrs,
            syn::Item::Impl(m) => &m.attrs,
            syn::Item::Const(c) => &c.attrs,
            syn::Item::Static(s) => &s.attrs,
            syn::Item::Trait(t) => &t.attrs,
            syn::Item::Use(u) => &u.attrs,
            syn::Item::Macro(m) => &m.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            return;
        }
        // An item starts its own scope: nothing local crosses into it.
        let outer = std::mem::replace(&mut self.scopes, vec![Default::default()]);
        syn::visit::visit_item(self, i);
        self.scopes = outer;
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if is_cfg_test(&f.attrs) {
            return;
        }
        let outer = std::mem::replace(&mut self.scopes, vec![Default::default()]);
        syn::visit::visit_impl_item_fn(self, f);
        self.scopes = outer;
    }

    fn visit_block(&mut self, b: &'ast syn::Block) {
        self.scoped(|s| syn::visit::visit_block(s, b));
    }

    fn visit_stmt(&mut self, st: &'ast syn::Stmt) {
        match st {
            syn::Stmt::Local(l) => {
                if is_cfg_test(&l.attrs) {
                    return;
                }
                let root = match &l.init {
                    Some(init) => {
                        self.visit_expr(&init.expr);
                        if let Some((_, diverge)) = &init.diverge {
                            self.visit_expr(diverge);
                        }
                        self.is_root(&init.expr)
                    }
                    None => false,
                };
                self.bind(&l.pat, root);
            }
            syn::Stmt::Item(i) => self.visit_item(i),
            syn::Stmt::Expr(e, _) => self.visit_expr(e),
            syn::Stmt::Macro(m) => {
                if !is_cfg_test(&m.attrs) {
                    self.visit_macro(&m.mac);
                }
            }
        }
    }

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        self.scoped(|s| {
            for p in &c.inputs {
                s.bind(p, false);
            }
            s.visit_expr(&c.body);
        });
    }

    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        self.scoped(|s| {
            s.bind(&a.pat, false);
            if let Some((_, guard)) = &a.guard {
                s.visit_expr(guard);
            }
            s.visit_expr(&a.body);
        });
    }

    fn visit_expr_for_loop(&mut self, f: &'ast syn::ExprForLoop) {
        self.visit_expr(&f.expr);
        self.scoped(|s| {
            s.bind(&f.pat, false);
            s.visit_block(&f.body);
        });
    }

    fn visit_expr(&mut self, e: &'ast syn::Expr) {
        if let syn::Expr::MethodCall(m) = e
            && m.method == "join"
            && m.args.len() == 1
        {
            let arg = &m.args[0];
            let line = line_of(m.method.span());
            if self.is_root(&m.receiver) {
                let child = match str_lit(arg) {
                    Some(lit) => lit
                        .split('/')
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(|s| Child::Named(s.to_string())),
                    None => Some(Child::Computed(
                        quote::ToTokens::to_token_stream(arg)
                            .to_string()
                            .replace(' ', ""),
                    )),
                };
                if let Some(child) = child {
                    self.found.push((line, child));
                }
            }
            if let Some(lit) = str_lit(arg)
                && let Some(rest) = lit.strip_prefix(".clank/")
                && !is_home_site(self.file, &receiver_name(&m.receiver))
                && let Some(first) = rest.split('/').next().filter(|s| !s.is_empty())
            {
                self.found.push((line, Child::Named(first.to_string())));
            }
        }
        syn::visit::visit_expr(self, e);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.visit_tokens(m.tokens.clone());
    }

    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        self.scoped(|s| {
            s.visit_condition(&i.cond);
            s.visit_block(&i.then_branch);
        });
        if let Some((_, otherwise)) = &i.else_branch {
            self.visit_expr(otherwise);
        }
    }

    fn visit_expr_while(&mut self, w: &'ast syn::ExprWhile) {
        self.scoped(|s| {
            s.visit_condition(&w.cond);
            s.visit_block(&w.body);
        });
    }
}

/// The gate's verdict on one file, as the lines it would report.
///
/// One decision function, used by the repo scan AND by the corpus
/// test, so what the corpus proves is what the repo is checked with.
fn strays_in(src: &str, file: &str) -> Vec<String> {
    let found = match children(src, file) {
        Ok(found) => found,
        Err(why) => return vec![format!("{file} — {why}")],
    };
    let mut out = Vec::new();
    for (line, child) in found {
        match child {
            Child::Named(name) if DECLARED.contains(&name.as_str()) => {}
            Child::Named(name) => out.push(format!("{file}:{line} — .clank/{name}")),
            Child::Unreadable => out.push(format!(
                "{file}:{line} — `.clank` in macro syntax this gate cannot read"
            )),
            Child::Computed(expr) => {
                let declared = COMPUTED
                    .iter()
                    .any(|(f, e, _)| file.ends_with(f) && e.replace(' ', "") == expr);
                if !declared {
                    out.push(format!(
                        "{file}:{line} — .clank/{{{expr}}}, computed and undeclared"
                    ));
                }
            }
        }
    }
    out
}

#[test]
fn production_creates_no_undeclared_path_under_clank() {
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

    let mut strays: Vec<String> = Vec::new();
    let production = Production::of(&files);
    for f in files.iter().filter(|f| !production.is_test_only(f)) {
        let shown = f.display().to_string();
        let src = std::fs::read_to_string(f).unwrap_or_default();
        strays.extend(strays_in(&src, &shown));
    }

    assert!(
        strays.is_empty(),
        "production code puts {} undeclared path(s) under a repo's `.clank/`. If it is \
         RUNTIME state — meaningful only while its process lives — it belongs in \
         `monitor::runtime_path`, outside the repo, where no repo's `.gitignore` has to \
         know about it. If it genuinely belongs to the repo, add it to DECLARED (or, for \
         a computed child, to COMPUTED) and say why:\n  {}",
        strays.len(),
        strays.join("\n  ")
    );
}

/// The gate rejects the ordinary ways of writing a forbidden path, not
/// just the one it was first written against.
///
/// Every entry here is a spelling that reached `.clank/monitor.json`
/// and was NOT reported by the first version of this gate, which is
/// how a gate comes to certify its own syntax instead of the rule
/// (codex on 9dfc3e3). Driven through `strays_in`, the same function
/// the repo scan uses.
#[test]
fn the_gate_rejects_every_ordinary_spelling_of_a_forbidden_path() {
    let caught = |src: &str| strays_in(src, "cli/src/cli/monitor.rs");

    for src in [
        // The one the first version did catch.
        r#"fn p(repo: &Path) -> PathBuf { repo.join(".clank").join("monitor.json") }"#,
        // One literal, which is routine production style.
        r#"fn p(repo: &Path) -> PathBuf { repo.join(".clank/monitor.json") }"#,
        // The same chain as rustfmt breaks it when it does not fit.
        "fn p(repo: &Path) -> PathBuf {\n    repo.join(\".clank\")\n        \
         .join(\"monitor.json\")\n}",
        // Through the accessor.
        r#"fn p(repo: &Path) -> PathBuf { clank_dir(repo).join("monitor.json") }"#,
        // Off a local bound to the root — init.rs's own shape.
        "fn p(repo: &Path) -> PathBuf {\n    let clank = repo.join(\".clank\");\n    \
         clank.join(\"monitor.json\")\n}",
        // A receiver whose NAME contains \"home\" but which is a repo.
        r#"fn p(repo_home: &Path) -> PathBuf { repo_home.join(".clank").join("monitor.json") }"#,
    ] {
        let found = caught(src);
        assert!(
            found.iter().any(|s| s.contains("monitor.json")),
            "this spelling walks past the gate:\n{src}\nreported: {found:?}"
        );
    }

    // A computed child is reported rather than skipped: silence there
    // is how an undeclared path would hide.
    assert!(
        caught(r#"fn p(repo: &Path, n: &str) -> PathBuf { repo.join(".clank").join(n) }"#)
            .iter()
            .any(|s| s.contains("computed and undeclared")),
        "a computed child must be named, not resolved away"
    );

    // And the gate stays quiet where it should, or it is noise nobody
    // will keep passing.
    for quiet in [
        r#"fn p(repo: &Path) -> PathBuf { repo.join(".clank").join("plans") }"#,
        r#"fn p(repo: &Path) -> PathBuf { repo.join(".clank/agents/x/config.json") }"#,
        r#"fn p(repo: &Path) -> PathBuf { repo.join(".clank") }"#,
        r#"fn p(repo: &Path) -> bool { repo.join(".clank").is_dir() }"#,
        r#"    // repo.join(".clank").join("monitor.json")"#,
    ] {
        assert!(
            caught(quiet).is_empty(),
            "a false positive here:\n{quiet}\nreported: {:?}",
            caught(quiet)
        );
    }

    // `~/.clank` is the user's config directory, not a working tree,
    // and only at the sites that say so.
    let door = |src: &str| strays_in(src, "crates/web-cloudflare/src/door.rs");
    assert!(
        door(r#"fn p(home: &Path) -> PathBuf { home.join(".clank").join("remote.lock") }"#)
            .is_empty(),
        "a declared home site is exempt"
    );
    assert!(
        door(r#"fn p(home: &Path) -> PathBuf { home.join(".clank/config.json") }"#).is_empty(),
        "including in one literal"
    );
    assert!(
        !caught(r#"fn p(home: &Path) -> PathBuf { home.join(".clank").join("remote.lock") }"#)
            .is_empty(),
        "but the exemption is the declared SITE, not the name `home` anywhere"
    );
}

/// A local bound to `.clank` counts only in the block that binds it.
///
/// The two shapes that were reported for a neighbour's binding:
/// `agent.rs` building `agents/<label>/feedback/<sha>.md` through its
/// own `dir`, and `setup.rs` checking a skill directory's `SKILL.md`.
#[test]
fn a_clank_binding_is_scoped_to_its_block() {
    let caught = |src: &str| strays_in(src, "cli/src/cli/agent.rs");
    let neighbour = "fn lock(repo: &Path) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    dir.join(\"plans\")\n}\n";

    let feedback = format!(
        "fn feedback(repo: &Path, sha: &str) -> PathBuf {{\n    let dir = repo\n        \
         .join(\".clank/agents\")\n        .join(\"x\")\n        .join(\"feedback\");\n    \
         dir.join(format!(\"{{}}.md\", sha))\n}}\n{neighbour}"
    );
    assert_eq!(
        caught(&feedback),
        Vec::<String>::new(),
        "a subtree path under `agents`"
    );
    let skill = format!(
        "{neighbour}fn obsolete(dir: &Path) -> bool {{\n    dir.join(\"SKILL.md\").exists()\n}}\n"
    );
    assert_eq!(
        caught(&skill),
        Vec::<String>::new(),
        "an unrelated `dir` after the binding's block"
    );

    let inside = "fn p(repo: &Path) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    \
                  if true {\n        return dir.join(\"roster.lock\");\n    }\n    dir.join(\"plans\")\n}\n";
    assert!(
        caught(inside).iter().any(|s| s.contains("roster.lock")),
        "inside the binding's block, a nested use still counts: {:?}",
        caught(inside)
    );
    let computed = "fn p(repo: &Path, n: &str) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    dir.join(n)\n}\n";
    assert!(
        caught(computed)
            .iter()
            .any(|s| s.contains("computed and undeclared")),
        "and a computed child through the binding is still named"
    );
    let home_named_repo = "fn p(home: &Path) -> PathBuf {\n    let dir = home.join(\".clank\");\n    dir.join(\"x.lock\")\n}\n";
    assert!(
        !caught(home_named_repo).is_empty(),
        "a repo variable called `home` outside a declared site is a repo"
    );
}

/// Scope is the parsed block, not the text: literal braces close
/// nothing, a shadowing binding has its own provenance, and an inner
/// shadow ends with its block.
#[test]
fn a_clank_bindings_scope_is_rusts() {
    let caught = |src: &str| strays_in(src, "cli/src/cli/agent.rs");
    let brace_in_string = "fn p(repo: &Path) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    let label = \"}\";\n    dir.join(\"rogue.lock\")\n}\n";
    assert!(
        caught(brace_in_string)
            .iter()
            .any(|s| s.contains("rogue.lock")),
        "a `}}` in a string must not end the binding's scope: {:?}",
        caught(brace_in_string)
    );
    let shadowed = "fn p(repo: &Path, name: &str) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    let dir = dir.join(\"agents\");\n    dir.join(name)\n}\n";
    assert_eq!(
        caught(shadowed),
        Vec::<String>::new(),
        "a shadow under `agents` is not the root"
    );
    let restored = "fn p(repo: &Path) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    {\n        let dir = Path::new(\"/elsewhere\");\n        dir.join(\"fine.txt\");\n    }\n    dir.join(\"rogue.lock\")\n}\n";
    assert_eq!(
        caught(restored).len(),
        1,
        "the inner shadow is quiet and the outer root is back after it: {:?}",
        caught(restored)
    );
    let in_macro = "fn p(repo: &Path) {\n    println!(\"{}\", repo.join(\".clank\").join(\"rogue.lock\").display());\n}\n";
    assert!(
        !caught(in_macro).is_empty(),
        "a macro argument is still code"
    );
    let repeat = "fn p(repo: &Path) {\n    let paths = vec![repo.join(\".clank\").join(\"rogue.lock\"); 2];\n}\n";
    assert!(
        caught(repeat).iter().any(|s| s.contains("rogue.lock")),
        "a repeat-form `vec!` is still code: {:?}",
        caught(repeat)
    );
    let unreadable =
        "fn p(repo: &Path) {\n    odd!(=> repo.join(\".clank\").join(\"rogue.lock\") <=);\n}\n";
    assert!(
        caught(unreadable).iter().any(|s| s.contains("cannot read")),
        "macro syntax the gate cannot read is reported, not passed: {:?}",
        caught(unreadable)
    );
    let select = "async fn p(repo: &Path) -> PathBuf {\n    let dir = repo.join(\".clank\");\n    tokio::select! {\n        _ = std::future::pending::<()>() => dir.join(\"rogue.lock\"),\n    }\n}\n";
    assert!(
        caught(select)
            .iter()
            .any(|s| s.contains(".clank/rogue.lock")),
        "a bound root used inside a macro arm is still the root: {:?}",
        caught(select)
    );
    let bound_unreadable = "fn p(repo: &Path) {\n    let dir = repo.join(\".clank\");\n    odd!(dir @ \"rogue.lock\");\n}\n";
    assert!(
        caught(bound_unreadable)
            .iter()
            .any(|s| s.contains("cannot read")),
        "unreadable syntax reaching a bound root is reported: {:?}",
        caught(bound_unreadable)
    );
    let unrelated_unreadable =
        "fn p(repo: &Path) {\n    let other = repo.join(\"x\");\n    odd!(other @ \"fine\");\n}\n";
    assert_eq!(
        caught(unrelated_unreadable),
        Vec::<String>::new(),
        "and quiet when nothing in it is the root"
    );
    let grouped =
        "fn p(repo: &Path) {\n    odd!(=> {repo.join(\".clank\").join(\"rogue.lock\")} <=);\n}\n";
    assert!(
        caught(grouped)
            .iter()
            .any(|s| s.contains(".clank/rogue.lock")),
        "an expression grouped inside unreadable syntax is still read: {:?}",
        caught(grouped)
    );
    let quiet_macro =
        "fn p() {\n    println!(\"see the .clank docs\");\n    let v = vec![1; 2];\n}\n";
    assert_eq!(
        caught(quiet_macro),
        Vec::<String>::new(),
        "a readable macro that builds nothing"
    );
    for conditional in [
        "if let Some(dir) = maybe {\n        let _ = dir.join(\"fine.txt\");\n    }",
        "while let Some(dir) = maybe {\n        let _ = dir.join(\"fine.txt\");\n    }",
        "if let Some(dir) = maybe && let Some(_) = maybe {\n        let _ = dir.join(\"fine.txt\");\n    } else {\n        let _ = dir.join(\"rogue.lock\");\n    }",
    ] {
        let src = format!(
            "fn p(repo: &Path, maybe: Option<&Path>) -> PathBuf {{\n    let dir = repo.join(\".clank\");\n    {conditional}\n    dir.join(\"rogue.lock\")\n}}\n"
        );
        let found = caught(&src);
        assert!(
            !found.iter().any(|s| s.contains("fine.txt")),
            "the conditional binding shadows the root in its body:\n{src}\n{found:?}"
        );
        assert!(
            found.iter().any(|s| s.contains("rogue.lock")),
            "and the root is back after it:\n{src}\n{found:?}"
        );
    }
    let test_only = "#[cfg(test)]\nmod tests {\n    fn t(repo: &Path) -> PathBuf { repo.join(\".clank\").join(\"rogue.lock\") }\n}\n";
    assert_eq!(
        caught(test_only),
        Vec::<String>::new(),
        "test code is not production"
    );
    assert!(
        caught("fn {\n")
            .iter()
            .any(|s| s.contains("does not parse")),
        "a file the gate cannot read is reported, not passed"
    );
}

/// setup's installation domain is a home, not a repo, at that site only.
#[test]
fn setups_installation_domain_is_a_declared_home() {
    let src = "fn installation_lock(domain: &Path) -> File {\n    let dir = domain.join(\".clank\");\n    lock(&dir.join(\"setup-install.lock\"))\n}\n";
    assert_eq!(
        strays_in(src, "crates/cli/src/cli/setup.rs"),
        Vec::<String>::new()
    );
    assert!(
        !strays_in(src, "crates/cli/src/cli/agent.rs").is_empty(),
        "the declaration is the site, not the name `domain`"
    );
}
