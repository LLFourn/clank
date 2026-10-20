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
    "pr-reviews",
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
}

/// Blank out `//` comments, so prose about a path is not a path.
///
/// Length-preserving, because every offset is reported as a line
/// number afterwards.
fn without_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        match line.find("//") {
            Some(at) => {
                out.push_str(&line[..at]);
                for c in line[at..].chars() {
                    out.push(if c == '\n' { '\n' } else { ' ' });
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

/// The index just past a balanced `(`…`)` whose `(` is at `open`.
fn past_parens(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.bytes().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The identifier ending at `at`, as a receiver is read backwards.
fn ident_before(s: &str, at: usize) -> String {
    let head = s[..at].trim_end().trim_end_matches('.');
    let mut ident: Vec<char> = head
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    ident.reverse();
    ident.into_iter().collect()
}

/// The locals in `src` bound to a repo's `.clank` root.
///
/// `init.rs` writes the scaffold as `let clank_dir = repo.join(".clank")`
/// and joins children off that binding — a creating site a scanner that
/// only understood the inline spelling could not see at all.
fn clank_roots(src: &str, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("let ") else {
            continue;
        };
        let name: String = rest
            .trim_start_matches("mut ")
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let Some(eq) = rest.find('=') else { continue };
        let value = rest[eq + 1..].split(';').next().unwrap_or("").trim();
        let is_root = (value.starts_with("clank_dir(") && value.ends_with(')'))
            || (value.ends_with(INLINE)
                && !is_home_site(file, &ident_before(value, value.len() - INLINE.len())));
        if is_root && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Whether (file, receiver) is a declared `~/.clank` site.
fn is_home_site(file: &str, receiver: &str) -> bool {
    HOME_SITES
        .iter()
        .any(|(f, r, _)| file.ends_with(f) && *r == receiver)
}

const INLINE: &str = r#"join(".clank")"#;
const COMPOUND: &str = r#"join(".clank/"#;

/// Every child of a repo's `.clank/` the text builds, with its 1-based
/// line.
fn children(src: &str, file: &str) -> Vec<(usize, Child)> {
    let src = without_comments(src);
    let roots = clank_roots(&src, file);
    let mut found: Vec<(usize, Child)> = Vec::new();

    // `join(".clank/<child>/…")` in one literal. Routine production
    // style, and invisible to a scanner that only looked for the
    // two-call chain.
    let mut from = 0;
    while let Some(at) = src[from..].find(COMPOUND) {
        let abs = from + at;
        from = abs + COMPOUND.len();
        if is_home_site(file, &ident_before(&src, abs)) {
            continue;
        }
        if let Some(end) = src[from..].find('"') {
            let rest = &src[from..from + end];
            if let Some(first) = rest.split('/').next().filter(|s| !s.is_empty()) {
                found.push((abs, Child::Named(first.to_string())));
            }
        }
    }

    // The two-call chain, in any formatting: a root expression, then
    // `.join(...)` across whatever whitespace rustfmt left.
    let mut ends: Vec<usize> = Vec::new();
    let mut from = 0;
    while let Some(at) = src[from..].find(INLINE) {
        let abs = from + at;
        from = abs + INLINE.len();
        if !is_home_site(file, &ident_before(&src, abs)) {
            ends.push(from);
        }
    }
    let accessor = "clank_dir(";
    let mut from = 0;
    while let Some(at) = src[from..].find(accessor) {
        let abs = from + at;
        match past_parens(&src, abs + accessor.len() - 1) {
            Some(end) => {
                ends.push(end);
                from = end;
            }
            None => break,
        }
    }
    for name in &roots {
        let mut from = 0;
        while let Some(at) = src[from..].find(name.as_str()) {
            let abs = from + at;
            let end = abs + name.len();
            let standalone = abs == 0
                || !src[..abs]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"');
            if standalone && src[end..].trim_start().starts_with('.') {
                ends.push(end);
            }
            from = end;
        }
    }
    for end in ends {
        if let Some(child) = child_after(&src, end) {
            found.push((end, child));
        }
    }

    found.sort_by_key(|(at, _)| *at);
    found.dedup();
    found
        .into_iter()
        .map(|(at, c)| (src[..at].matches('\n').count() + 1, c))
        .collect()
}

/// The child named by the next `.join(...)` after `at`, if that is what
/// comes next.
///
/// Whitespace-tolerant in both gaps, because rustfmt breaks a chained
/// call after the receiver AND before the dot depending on width — and
/// a gate that a reformat can switch off is not a gate.
fn child_after(src: &str, at: usize) -> Option<Child> {
    let rest = src[at..].trim_start();
    let rest = rest.strip_prefix('.')?.trim_start();
    let rest = rest.strip_prefix("join(")?.trim_start();
    if let Some(lit) = rest.strip_prefix('"') {
        let end = lit.find('"')?;
        let whole = &lit[..end];
        return whole
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(|s| Child::Named(s.to_string()));
    }
    let end = rest.find(')')?;
    Some(Child::Computed(rest[..end].trim().to_string()))
}

/// The gate's verdict on one file, as the lines it would report.
///
/// One decision function, used by the repo scan AND by the corpus
/// test, so what the corpus proves is what the repo is checked with.
fn strays_in(src: &str, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (line, child) in children(src, file) {
        match child {
            Child::Named(name) if DECLARED.contains(&name.as_str()) => {}
            Child::Named(name) => out.push(format!("{file}:{line} — .clank/{name}")),
            Child::Computed(expr) => {
                let declared = COMPUTED
                    .iter()
                    .any(|(f, e, _)| file.ends_with(f) && *e == expr);
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
    for f in &files {
        let shown = f.display().to_string();
        let prod = production.text(f);
        strays.extend(strays_in(&prod, &shown));
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
