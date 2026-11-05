//! Enforcement gate (unsafe-lives-in-named-modules): `unsafe` appears
//! only in the items [`RESIDUAL`] names, and every unsafe block there
//! says what makes it sound.
//!
//! The workspace denies `unsafe_code`, so the compiler already refuses
//! unsafe anywhere an `#[allow(unsafe_code)]` does not reach. What it
//! cannot refuse is the allow itself: one more attribute, on a whole
//! module or file, quietly widens the surface a reviewer has to read.
//! This gate is that review, made structural. It parses every `.rs`
//! file of every crate — tests, examples and build scripts included —
//! so an `unsafe` in a comment or a string is not a finding, and a
//! finding is placed by the item that encloses it, not by its file: a
//! residual item does not excuse its neighbours.

use std::path::{Path, PathBuf};

use syn::visit::Visit;

use crate::common::crates_dir;

/// One item that may hold `unsafe`, and why nothing safe will do.
struct Residual {
    /// Relative to `crates/`.
    file: &'static str,
    /// The item path within the file, `::`-separated; everything nested
    /// in it is covered.
    item: &'static str,
    why: &'static str,
}

const RESIDUAL: &[Residual] = &[
    Residual {
        file: "cli/src/main.rs",
        item: "reset_sigpipe",
        why: "Rust ignores SIGPIPE and has no safe API to restore its default",
    },
    Residual {
        file: "cli/src/build_stamp.rs",
        item: "embedded",
        why: "only a volatile read keeps the marker from being folded into a second copy",
    },
    Residual {
        file: "cli/src/cli/term.rs",
        item: "restore",
        why: "a C signal handler reaches its saved termios through a static, and calls _exit",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "Spec::command",
        why: "pre_exec is the only way to run code in the child between fork and exec",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "Shed::run",
        why: "the forked child clears CLOEXEC on descriptor numbers the spec names",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "cloexec_range",
        why: "close_range has no safe wrapper in std or rustix",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "apple::extern",
        why: "libc does not declare Apple's posix_spawn addinherit/addchdir extensions",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "apple::Actions::drop",
        why: "posix_spawn file actions are a C object destroyed through a pointer",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "apple::Attr::drop",
        why: "posix_spawn attributes are a C object destroyed through a pointer",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "apple::spawn",
        why: "posix_spawn attributes and file actions are built through out-pointers",
    },
    Residual {
        file: "cli/src/proc_run.rs",
        item: "ExitFd::of",
        why: "kevent, which registers the child's NOTE_EXIT, has no safe form",
    },
    Residual {
        file: "cli/src/proc_run_tests.rs",
        item: "concurrent_forks_never_make_a_run_incomplete",
        why: "only a pre_exec hook takes std's spawn off posix_spawn onto fork",
    },
    Residual {
        file: "cli/src/proc_identity/exit_watch.rs",
        item: "platform::Sys::apply",
        why: "kevent registration: each registered descriptor must outlive the queue",
    },
    Residual {
        file: "cli/src/proc_identity/exit_watch.rs",
        item: "platform::Sys::wait",
        why: "kevent has no safe form, even with no changes to register",
    },
    Residual {
        file: "cli/src/proc_identity/inspect.rs",
        item: "platform::bsd_info",
        why: "proc_pidinfo fills a C struct through an out-pointer",
    },
    Residual {
        file: "cli/src/proc_identity/inspect.rs",
        item: "platform::group_members",
        why: "proc_listpgrppids fills a buffer through an out-pointer",
    },
    Residual {
        file: "cli/src/proc_identity/inspect.rs",
        item: "platform::running_threads",
        why: "proc_pidinfo fills a C struct through an out-pointer",
    },
    Residual {
        file: "cli/src/proc_identity/inspect.rs",
        item: "platform::argv",
        why: "sysctl fills its buffers through out-pointers",
    },
    Residual {
        file: "cli/src/signal.rs",
        item: "Kernel::raw",
        why: "rustix cannot send a signal it has no name for, real-time ones included",
    },
    Residual {
        file: "web-cloudflare/src/secret.rs",
        item: "login_home",
        why: "neither std nor rustix has a passwd lookup, and HOME is not the login home",
    },
];

/// One `unsafe` in the source, or one opt-out of the lint.
#[derive(Debug)]
struct Finding {
    path: Vec<String>,
    line: usize,
    what: &'static str,
    /// An unsafe block, which must say why it is sound.
    block: bool,
}

#[derive(Default)]
struct Scan {
    path: Vec<String>,
    found: Vec<Finding>,
}

impl Scan {
    fn find(&mut self, span: proc_macro2::Span, what: &'static str, block: bool) {
        self.found.push(Finding {
            path: self.path.clone(),
            line: span.start().line,
            what,
            block,
        });
    }

    fn nested<T>(&mut self, name: Option<String>, inner: impl FnOnce(&mut Self) -> T) -> T {
        let pushed = name.map(|n| self.path.push(n)).is_some();
        let out = inner(self);
        if pushed {
            self.path.pop();
        }
        out
    }
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        other => quote::quote!(#other).to_string(),
    }
}

fn item_name(item: &syn::Item) -> Option<String> {
    use syn::Item::*;
    Some(match item {
        Const(i) => i.ident.to_string(),
        Enum(i) => i.ident.to_string(),
        ExternCrate(i) => i.ident.to_string(),
        Fn(i) => i.sig.ident.to_string(),
        ForeignMod(_) => "extern".to_string(),
        Impl(i) => type_name(&i.self_ty),
        Macro(i) => i.ident.as_ref()?.to_string(),
        Mod(i) => i.ident.to_string(),
        Static(i) => i.ident.to_string(),
        Struct(i) => i.ident.to_string(),
        Trait(i) => i.ident.to_string(),
        TraitAlias(i) => i.ident.to_string(),
        Type(i) => i.ident.to_string(),
        Union(i) => i.ident.to_string(),
        _ => return None,
    })
}

fn idents(tokens: proc_macro2::TokenStream, out: &mut Vec<proc_macro2::Ident>) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Ident(i) => out.push(i),
            proc_macro2::TokenTree::Group(g) => idents(g.stream(), out),
            _ => {}
        }
    }
}

/// Whether an attribute opts out of the lint, or is itself unsafe.
fn opt_out(attr: &syn::Attribute) -> Option<&'static str> {
    let name = attr.path().get_ident()?.to_string();
    let mut words = Vec::new();
    if let syn::Meta::List(list) = &attr.meta {
        idents(list.tokens.clone(), &mut words);
    }
    let names = |w: &str| words.iter().any(|i| i == w);
    let lowers = || ["allow", "expect", "warn"].iter().any(|w| names(w));
    match name.as_str() {
        "allow" | "expect" | "warn" if names("unsafe_code") => Some("an `allow(unsafe_code)`"),
        "cfg_attr" if names("unsafe_code") && lowers() => {
            Some("a `cfg_attr`-wrapped `allow(unsafe_code)`")
        }
        "unsafe" | "no_mangle" | "export_name" | "link_section" => Some("an unsafe attribute"),
        _ => None,
    }
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let unsafety = match item {
            syn::Item::Fn(i) => i.sig.unsafety.map(|u| (u.span, "an `unsafe fn`")),
            syn::Item::Impl(i) => i.unsafety.map(|u| (u.span, "an `unsafe impl`")),
            syn::Item::Trait(i) => i.unsafety.map(|u| (u.span, "an `unsafe trait`")),
            syn::Item::ForeignMod(i) => Some((i.abi.extern_token.span, "an `unsafe extern` block")),
            _ => None,
        };
        self.nested(item_name(item), |scan| {
            if let Some((span, what)) = unsafety {
                scan.find(span, what, false);
            }
            syn::visit::visit_item(scan, item);
        });
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let name = match item {
            syn::ImplItem::Fn(f) => {
                if let Some(u) = f.sig.unsafety {
                    self.nested(Some(f.sig.ident.to_string()), |scan| {
                        scan.find(u.span, "an `unsafe fn`", false)
                    });
                }
                Some(f.sig.ident.to_string())
            }
            syn::ImplItem::Const(c) => Some(c.ident.to_string()),
            syn::ImplItem::Type(t) => Some(t.ident.to_string()),
            _ => None,
        };
        self.nested(name, |scan| syn::visit::visit_impl_item(scan, item));
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let name = match item {
            syn::TraitItem::Fn(f) => {
                if let Some(u) = f.sig.unsafety {
                    self.nested(Some(f.sig.ident.to_string()), |scan| {
                        scan.find(u.span, "an `unsafe fn`", false)
                    });
                }
                Some(f.sig.ident.to_string())
            }
            syn::TraitItem::Const(c) => Some(c.ident.to_string()),
            syn::TraitItem::Type(t) => Some(t.ident.to_string()),
            _ => None,
        };
        self.nested(name, |scan| syn::visit::visit_trait_item(scan, item));
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if let Some(what) = opt_out(attr) {
            self.find(attr.pound_token.span, what, false);
        }
        syn::visit::visit_attribute(self, attr);
    }

    fn visit_expr_unsafe(&mut self, block: &'ast syn::ExprUnsafe) {
        self.find(block.unsafe_token.span, "an unsafe block", true);
        syn::visit::visit_expr_unsafe(self, block);
    }

    /// Macro bodies are tokens, not syntax, so an `unsafe` there is an
    /// identifier; a string mentioning it is a literal and is not.
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let mut words = Vec::new();
        idents(mac.tokens.clone(), &mut words);
        for word in words.into_iter().filter(|w| w == "unsafe") {
            self.find(word.span(), "an `unsafe` in a macro body", true);
        }
        syn::visit::visit_macro(self, mac);
    }
}

/// Whether the comment run directly above `line` (1-based) states a
/// non-empty `SAFETY:` reason. Attributes between it and the line are
/// passed over.
fn justified(lines: &[&str], line: usize) -> bool {
    let mut run = Vec::new();
    for above in lines[..line.saturating_sub(1)].iter().rev() {
        let t = above.trim();
        if let Some(comment) = t.strip_prefix("//") {
            run.push(comment.trim_start_matches('/').trim());
        } else if !t.starts_with("#[") {
            break;
        }
    }
    run.reverse();
    run.iter().enumerate().any(|(i, comment)| {
        comment.strip_prefix("SAFETY:").is_some_and(|rest| {
            !rest.trim().is_empty() || run.get(i + 1).is_some_and(|next| !next.is_empty())
        })
    })
}

fn covers(residual: &Residual, path: &[String]) -> bool {
    let item: Vec<&str> = residual.item.split("::").collect();
    path.len() >= item.len() && path.iter().zip(&item).all(|(p, i)| p == i)
}

/// Every finding in `src` (known as `file`) that `residual` does not
/// permit, and the residual entries for `file` that matched something.
fn check(file: &str, src: &str, residual: &[Residual]) -> (Vec<String>, Vec<&'static str>) {
    let parsed = match syn::parse_file(src) {
        Ok(parsed) => parsed,
        Err(e) => return (vec![format!("{file}: does not parse: {e}")], Vec::new()),
    };
    let mut scan = Scan::default();
    scan.visit_file(&parsed);
    let lines: Vec<&str> = src.lines().collect();
    let mine: Vec<&Residual> = residual.iter().filter(|r| r.file == file).collect();
    let mut used = Vec::new();
    let mut bad = Vec::new();
    for f in &scan.found {
        let at = format!("{file}:{} ({})", f.line, f.path.join("::"));
        match mine.iter().find(|r| covers(r, &f.path)) {
            None => bad.push(format!("{at}: {} outside any residual item", f.what)),
            Some(r) => {
                used.push(r.item);
                if f.block && !justified(&lines, f.line) {
                    bad.push(format!(
                        "{at}: {} with no `// SAFETY:` reason above it",
                        f.what
                    ));
                }
            }
        }
    }
    (bad, used)
}

fn workspace_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            if p.is_dir() {
                if name != "target" && name != "node_modules" {
                    walk(&p, out);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(&crates_dir(), &mut files);
    files.sort();
    files
}

fn relative(file: &Path) -> String {
    file.strip_prefix(crates_dir())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn unsafe_lives_only_in_the_residual_items() {
    let files = workspace_sources();
    for krate in ["cli", "core", "user-config", "web", "web-cloudflare"] {
        assert!(
            files
                .iter()
                .any(|f| relative(f).starts_with(&format!("{krate}/"))),
            "no sources found for {krate}"
        );
    }
    for root in [
        "cli/build.rs",
        "web/examples/custom_host.rs",
        "core/tests/round_trip.rs",
    ] {
        assert!(
            files.iter().any(|f| relative(f) == root),
            "{root} was not scanned"
        );
    }

    let mut bad = Vec::new();
    let mut used = Vec::new();
    for f in &files {
        let file = relative(f);
        let (b, u) = check(&file, &std::fs::read_to_string(f).unwrap(), RESIDUAL);
        bad.extend(b);
        used.extend(u.into_iter().map(|item| (file.clone(), item)));
    }
    for r in RESIDUAL {
        assert!(
            !r.why.trim().is_empty(),
            "{} {} gives no reason",
            r.file,
            r.item
        );
        if !used.iter().any(|(f, i)| f == r.file && *i == r.item) {
            bad.push(format!(
                "{} {}: listed as residual, but holds no unsafe; take it off the list",
                r.file, r.item
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "unsafe outside the residual list ({} finding(s)). Use the safe rustix or \
         std call, or, where none exists, add the ITEM to RESIDUAL with its reason \
         and give each unsafe block a `// SAFETY:` comment:\n  {}",
        bad.len(),
        bad.join("\n  ")
    );
}

/// The calls a function's body makes: (functions, methods, macros), by
/// last path segment.
fn calls_of(file: &syn::File, name: &str) -> Option<(Vec<String>, Vec<String>, Vec<String>)> {
    #[derive(Default)]
    struct Calls {
        target: String,
        inside: bool,
        found: bool,
        fns: Vec<String>,
        methods: Vec<String>,
        macros: Vec<String>,
    }
    impl<'ast> Visit<'ast> for Calls {
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            let was = self.inside;
            if f.sig.ident == self.target {
                self.inside = true;
                self.found = true;
            }
            syn::visit::visit_item_fn(self, f);
            self.inside = was;
        }
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if self.inside {
                let name = match &*call.func {
                    syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
                    _ => None,
                };
                self.fns
                    .push(name.unwrap_or_else(|| "<an expression>".into()));
            }
            syn::visit::visit_expr_call(self, call);
        }
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if self.inside {
                self.methods.push(call.method.to_string());
            }
            syn::visit::visit_expr_method_call(self, call);
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            if self.inside {
                self.macros
                    .push(mac.path.segments.last().unwrap().ident.to_string());
            }
        }
    }
    let mut calls = Calls {
        target: name.into(),
        ..Default::default()
    };
    calls.visit_file(file);
    calls
        .found
        .then_some((calls.fns, calls.methods, calls.macros))
}

/// What one function the signal handler reaches may call: functions and
/// methods by last path segment. Any macro is a stray: formatting
/// allocates and takes locks.
struct Audit {
    name: &'static str,
    fns: &'static [&'static str],
    methods: &'static [&'static str],
}

/// The handler's own audit: what it calls directly, and the term.rs
/// functions reachable from it, which may only load the saved pointer,
/// dereference it and return.
const HANDLER: &[Audit] = &[
    Audit {
        name: "restore_and_exit",
        fns: &["restore_terminal", "stdin", "stdout", "saved", "_exit"],
        methods: &[],
    },
    Audit {
        name: "restore_terminal",
        fns: &["tcsetattr", "write"],
        methods: &["as_bytes"],
    },
    Audit {
        name: "saved",
        fns: &[],
        methods: &["load", "as_ref"],
    },
];

/// Why the functions reachable from `audits[0]` within `file` call
/// anything their audit does not allow, if they do. A call to a function
/// defined in `file` that has no audit of its own is itself a stray.
fn handler_strays(file: &syn::File, audits: &[Audit]) -> Result<(), String> {
    let mut queue = vec![audits[0].name.to_string()];
    let mut seen = Vec::new();
    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        let audit = audits
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("the handler reaches `{name}`, which has no audit"))?;
        let (called, invoked, macros) =
            calls_of(file, &name).ok_or_else(|| format!("no fn `{name}`"))?;
        let strays: Vec<String> = called
            .iter()
            .filter(|c| !audit.fns.contains(&c.as_str()))
            .chain(
                invoked
                    .iter()
                    .filter(|m| !audit.methods.contains(&m.as_str())),
            )
            .chain(&macros)
            .cloned()
            .collect();
        if !strays.is_empty() {
            return Err(format!("`{name}` also calls {strays:?}"));
        }
        queue.extend(called.into_iter().filter(|c| calls_of(file, c).is_some()));
        seen.push(name);
    }
    Ok(())
}

/// The SIGINT/SIGTERM handler runs between any two instructions of the
/// program, so it may only restore the terminal and `_exit`; that restore
/// may only make the two syscalls it needs, and reading the saved termios
/// only loads and dereferences a pointer.
#[test]
fn the_terminal_signal_handler_only_restores_and_exits() {
    let src = std::fs::read_to_string(crates_dir().join("cli/src/cli/term.rs")).unwrap();
    handler_strays(&syn::parse_file(&src).unwrap(), HANDLER).unwrap();

    let sound = "fn saved() -> Option<&'static T> { unsafe { SAVED.load(Acquire).as_ref() } }\n\
                 fn restore_terminal() { let _ = tcsetattr(); let _ = write(SEQ.as_bytes()); }\n";
    let with = |handler: &str, saved: &str| {
        let src = format!(
            "extern \"C\" fn restore_and_exit() {{ {handler} }}\n\
             fn saved() -> Option<&'static T> {{ {saved} }}\n\
             fn restore_terminal() {{ let _ = tcsetattr(); let _ = write(SEQ.as_bytes()); }}\n"
        );
        handler_strays(&syn::parse_file(&src).unwrap(), HANDLER)
    };
    let fine_handler = "restore_terminal(stdin(), saved(), stdout()); libc::_exit(1)";
    let fine_saved = "unsafe { SAVED.load(Acquire).as_ref() }";
    assert_eq!(
        with(fine_handler, fine_saved),
        Ok(()),
        "the sound shape was rejected"
    );

    for (what, handler) in [
        ("prints", "eprintln!(\"bye\"); libc::_exit(1)"),
        (
            "exits through std",
            "restore_terminal(); std::process::exit(1)",
        ),
        ("locks", "restore_terminal(); LOCK.lock(); libc::_exit(1)"),
        ("reaches an unaudited helper", "helper(); libc::_exit(1)"),
    ] {
        let src = format!(
            "{sound}fn helper() {{}}\nextern \"C\" fn restore_and_exit() {{ {handler} }}\n"
        );
        let mut audits: Vec<Audit> = HANDLER.iter().map(|a| Audit { ..*a }).collect();
        if what == "reaches an unaudited helper" {
            audits[0].fns = &["helper", "_exit"];
        }
        assert!(
            handler_strays(&syn::parse_file(&src).unwrap(), &audits).is_err(),
            "a handler that {what} passed"
        );
    }

    for (what, saved) in [
        (
            "locks",
            "let _g = LOCK.lock(); unsafe { SAVED.load(Acquire).as_ref() }",
        ),
        (
            "allocates",
            "let _b = Box::new(1); unsafe { SAVED.load(Acquire).as_ref() }",
        ),
        (
            "formats",
            "let _s = format!(\"x\"); unsafe { SAVED.load(Acquire).as_ref() }",
        ),
        (
            "collects",
            "let _v: Vec<u8> = Vec::new(); unsafe { SAVED.load(Acquire).as_ref() }",
        ),
    ] {
        assert!(
            with(fine_handler, saved).is_err(),
            "a `saved` that {what} passed"
        );
    }
}

#[test]
fn the_scanner_rejects_each_forbidden_case_and_ignores_prose() {
    const FILE: &str = "x/src/lib.rs";
    const ALLOWED: &[Residual] = &[Residual {
        file: FILE,
        item: "inner::approved",
        why: "a fixture",
    }];
    let verdict = |src: &str| check(FILE, src, ALLOWED).0;

    let approved = "mod inner {\n    #[allow(unsafe_code)]\n    fn approved() {\n        \
                    // SAFETY: a fixture, never compiled.\n        unsafe { f() }\n    }\n}\n";
    assert_eq!(
        verdict(approved),
        Vec::<String>::new(),
        "the approved item is accepted"
    );

    for (what, src) in [
        (
            "unsafe beside the approved item",
            "mod inner {\n    #[allow(unsafe_code)]\n    fn approved() {}\n    fn neighbour() {\n        \
             // SAFETY: plausible.\n        unsafe { f() }\n    }\n}\n",
        ),
        (
            "unsafe in a test module of the same file",
            "#[cfg(test)]\nmod tests {\n    fn t() {\n        // SAFETY: plausible.\n        \
             unsafe { f() }\n    }\n}\n",
        ),
        (
            "an unapproved allow",
            "#[allow(unsafe_code)]\nfn elsewhere() {}\n",
        ),
        (
            "an unapproved crate-wide allow",
            "#![allow(unsafe_code)]\nfn f() {}\n",
        ),
        (
            "an unapproved cfg_attr allow",
            "#[cfg_attr(unix, allow(unsafe_code))]\nfn elsewhere() {}\n",
        ),
        ("an unsafe fn", "unsafe fn elsewhere() {}\n"),
        ("an unsafe impl", "unsafe impl Send for S {}\n"),
        (
            "an unsafe extern block",
            "unsafe extern \"C\" { fn f(); }\n",
        ),
        ("an unsafe attribute", "#[unsafe(no_mangle)]\nfn f() {}\n"),
        (
            "unsafe inside a macro",
            "fn f() { let _ = vec![unsafe { g() }]; }\n",
        ),
        (
            "an approved block with no SAFETY comment",
            "mod inner {\n    fn approved() {\n        unsafe { f() }\n    }\n}\n",
        ),
        (
            "an approved block with an empty SAFETY comment",
            "mod inner {\n    fn approved() {\n        // SAFETY:\n        unsafe { f() }\n    }\n}\n",
        ),
        (
            "an approved block whose SAFETY comment is not directly above",
            "mod inner {\n    fn approved() {\n        // SAFETY: about something else.\n        \
             let x = 1;\n        unsafe { f(x) }\n    }\n}\n",
        ),
    ] {
        assert!(!verdict(src).is_empty(), "{what} was accepted:\n{src}");
    }

    for (what, src) in [
        ("prose", "// an unsafe { block } in a comment\nfn f() {}\n"),
        (
            "a doc comment",
            "/// `unsafe { f() }` is how not to\nfn f() {}\n",
        ),
        ("a string", "fn f() { let _ = \"unsafe { f() }\"; }\n"),
        (
            "a string in a macro",
            "fn f() { assert_eq!(g(\"unsafe { h() }\"), 1); }\n",
        ),
        (
            "the lint's own name",
            "#![forbid(unsafe_code)]\nfn f() {}\n",
        ),
    ] {
        assert_eq!(
            verdict(src),
            Vec::<String>::new(),
            "{what} was counted:\n{src}"
        );
    }
}
