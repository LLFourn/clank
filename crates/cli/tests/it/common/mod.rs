//! Shared integration-test helpers for the roster model
//! (`repo-agents-no-team`).
//!
//! Setup goes through [`TestEnv`] + [`register_team`]: build the
//! repo ROSTER via the REAL library cores (`clank agent add` /
//! `clank agent promote`), in a HOME distinct from the repo.
//! The repo's `agents` IS the operating team — there's no separate
//! `team` field.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A git repo with its OWN HOME, distinct from the repo dir, so a
/// team set up via [`register_team`] persists in user-scope.
/// Tests pass `env.home()` / `env.repo()` to the library cores
/// they call in-process — no test executes the clank binary
/// (clean break, lloyd 2026-06-10; behavior coverage lives in
/// each `cli::*` module's unit tests).
pub struct TestEnv {
    pub home: tempfile::TempDir,
    pub repo: tempfile::TempDir,
}

impl TestEnv {
    /// `git init` a fresh repo (main branch, test identity, no
    /// gpg) alongside a separate empty HOME.
    pub fn init() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let repo = tempfile::tempdir().expect("repo tempdir");
        let p = repo.path();
        git(p, &["init", "--quiet", "--initial-branch=main"]);
        git(p, &["config", "user.email", "test@test"]);
        git(p, &["config", "user.name", "test"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        Self { home, repo }
    }

    pub fn home(&self) -> &Path {
        self.home.path()
    }

    pub fn repo(&self) -> &Path {
        self.repo.path()
    }

    /// Register a `default` team via the real cores (see
    /// [`register_team`]). Call once per env.
    pub fn register_team(&self, master: &str, commit_reviewers: &[&str], gate_reviewers: &[&str]) {
        register_team(
            self.home(),
            self.repo(),
            master,
            commit_reviewers,
            gate_reviewers,
        );
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(repo) = dunce::canonicalize(self.repo()) {
            let tag = blake3::hash(repo.as_os_str().as_bytes()).to_hex()[..32].to_string();
            let path = PathBuf::from(format!("/tmp/clank-run-{}", unsafe { libc::getuid() }))
                .join(format!("{tag}.plan-mutation"));
            let _ = std::fs::remove_file(path);
        }
    }
}

/// `git -C repo` carrying a test identity in its environment, so a
/// commit in a clone or worktree the fixture never configured works on
/// a runner with no global git identity.
pub fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test");
    command
}

fn git(repo: &Path, args: &[&str]) {
    let status = git_command(repo).args(args).status().expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a repo ROSTER THE REAL WAY — through the same library
/// cores the CLI handlers call, so test setup and production share
/// one code path. `home` is accepted for signature stability (some
/// callers build a user-scope library too) but the roster lives in
/// the repo config; the two MUST be separate dirs.
///
/// Sequence mirrors what a user would run on a fresh repo:
/// `clank agent add <name> --tool …` for each agent, then
/// `clank agent make-master <master>`.
pub fn register_team(
    home: &Path,
    repo: &Path,
    master: &str,
    commit_reviewers: &[&str],
    gate_reviewers: &[&str],
) {
    let _ = home;
    use clank::cli::teams_config::{AgentDescription, RosterRole};
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;

    let desc = || AgentDescription {
        tool: Tool::Claude,
        launch: None,
        initial_prompt: None,
    };
    let lbl = |s: &str| AgentLabel::parse(s).unwrap();

    // Add the master + reviewers to the repo roster (inline
    // definitions), then designate the master.
    clank::cli::agent::add_repo_roster_agent(repo, &lbl(master), desc(), RosterRole::Commit)
        .unwrap();
    for r in commit_reviewers {
        clank::cli::agent::add_repo_roster_agent(repo, &lbl(r), desc(), RosterRole::Commit)
            .unwrap();
    }
    for r in gate_reviewers {
        clank::cli::agent::add_repo_roster_agent(repo, &lbl(r), desc(), RosterRole::Gate).unwrap();
    }
    clank::cli::agent::set_repo_master(repo, &lbl(master)).unwrap();
}

// ---------------------------------------------------------------
// Wait-test timing support (remove-wait-timeout).
//
// `clank wait` no longer has a timeout, so "it stays parked" can no
// longer be asserted by letting a clock expire. It is now asserted
// by SAMPLING: spawn the wait, look after a window, abort. The
// window is the delicate part.
//
// A watcher attach costs ~2ms from an ordinary binary but 7.5-11.3s
// under libtest on some machines (measured; the mechanism inside the
// harness was never identified). So a fixed 2s window would sample a
// wait that has not begun to park — and would pass just as happily
// for a wait wedged in setup, which is exactly the bug worth
// catching. Every window here is therefore derived from a real
// measurement taken once per test binary.
// ---------------------------------------------------------------

use std::time::{Duration, Instant};

/// This environment's watcher-attach cost, measured once per test
/// binary against a real repo.
///
/// Note for anyone reading a slow run: attach is SYNCHRONOUS, so a
/// spawned wait occupies a runtime worker for its whole duration.
/// Tests here therefore need more workers than the two a wait plus
/// the test itself would suggest — with too few, every worker sits
/// in a blocking attach and the test's own `timeout` future is never
/// polled, so its deadline cannot fire and the test hangs instead of
/// failing.
pub fn attach_cost(repo: &Path) -> Duration {
    static COST: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *COST.get_or_init(|| {
        let (tx, _rx) = std::sync::mpsc::channel::<()>();
        let t = Instant::now();
        let watcher = clank::repo_watch::RepoStateWatcher::attach(repo, true, tx)
            .expect("attach a watcher to the fixture repo");
        drop(watcher);
        t.elapsed()
    })
}

/// How long to let a wait run before concluding it is parked.
pub fn parked_window(repo: &Path) -> Duration {
    attach_cost(repo) + Duration::from_secs(2)
}

/// Deadline for a race that must RESOLVE — generous enough to cover
/// setup plus the work itself. Linear in the measurement, not a
/// multiple of it: attach has been observed anywhere from 2ms to
/// ~20s here, and multiplying the high end turns a 4-test binary
/// into minutes of waiting for deadlines nothing is expected to
/// reach.
pub fn race_deadline(repo: &Path) -> Duration {
    attach_cost(repo) + Duration::from_secs(30)
}

/// Attaching watchers concurrently is what made these binaries flaky
/// (four at once pushed every deadline past its limit). Hold this for
/// the body of any test that spawns a wait.
pub fn serial() -> Serial {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    Serial {
        _guard: SERIAL.lock().unwrap_or_else(|e| e.into_inner()),
    }
}

/// Held across a whole async test body, awaits included: each
/// `#[tokio::test]` runs on its own runtime thread, so blocking another
/// test's thread on it is the serialization, not a deadlock.
pub struct Serial {
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Assert a wait does NOT resolve: spawn it, sample after
/// [`parked_window`], then abort. Returns nothing — a wait that
/// parked has no result to inspect.
pub async fn assert_stays_parked(repo: &Path, args: clank::cli::WaitArgs, what: &str) {
    let handle = tokio::spawn(clank::cli::wait::run(args));
    tokio::time::sleep(parked_window(repo)).await;
    let finished = handle.is_finished();
    handle.abort();
    assert!(!finished, "{what}: the wait resolved instead of parking");
}

// ---- source-scan helpers, shared by the enforcement gates ----
//
// Five gates read the tree and assert what production code may not
// name. They all walk the same files the same way; the walk is not
// what any of them is about.

/// Blank out `#[cfg(test)]` items so a scan sees only production code.
/// An item ends at its closing brace, or at a `;` before any brace
/// (`#[cfg(test)] #[path = "x.rs"] mod tests;`). Dropped lines stay as
/// empty lines, so a line number in the result is one in the source.
pub fn strip_test_code(src: &str) -> String {
    let mut out = String::new();
    let mut in_test = false;
    let mut depth = 0i32;
    let mut entered = false; // seen the item's opening brace yet?
    let mut ended = false;
    let mut lexer = Lexer::default();
    for line in src.lines() {
        if !in_test {
            if line.trim_start().starts_with("#[cfg(test)]") {
                in_test = true;
                depth = 0;
                entered = false;
                ended = false;
                // fall through: count this attr line's braces (usually none)
            } else {
                lexer.scan(line, &mut |_| {});
                out.push_str(line);
                out.push('\n');
                continue;
            }
        }
        lexer.scan(line, &mut |c| match c {
            '{' if !ended => {
                depth += 1;
                entered = true;
            }
            '}' if !ended => {
                depth -= 1;
                ended = entered && depth <= 0;
            }
            ';' if !entered => ended = true,
            _ => {}
        });
        if ended {
            in_test = false;
        }
        out.push('\n');
    }
    out
}

/// The production text of each scanned file: [`strip_test_code`], and
/// nothing at all for a file that only test code includes.
///
/// A file such as `grok_hook_tests.rs` says nothing about itself; what
/// makes it test code is the `#[cfg(test)] #[path = …] mod tests;` that
/// includes it. The inclusions come from each file's PARSED module tree,
/// with its attributes attached and inline modules nested, so a comment
/// or a string that looks like a declaration includes nothing. A file is
/// test-only when every inclusion is in test code (or in a test-only
/// file), production as soon as one is production or none is found, and
/// if any production declaration cannot be resolved, or a file does not
/// parse, nothing is exempt: an inclusion clank cannot see might be the
/// production one.
pub struct Production {
    test_only: std::collections::HashSet<PathBuf>,
}

/// One `mod name;` that loads `file` from `from`, and whether it sits in
/// test code.
struct Inclusion {
    file: PathBuf,
    from: PathBuf,
    in_test: bool,
}

impl Production {
    pub fn of(files: &[PathBuf]) -> Self {
        let mut found: Vec<Inclusion> = Vec::new();
        let mut uncertain = false;
        for f in files {
            let src = std::fs::read_to_string(f).unwrap_or_default();
            let Ok(parsed) = syn::parse_file(&src) else {
                uncertain = true;
                continue;
            };
            let Some(dir) = f.parent() else {
                uncertain = true;
                continue;
            };
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let plain = if matches!(stem, "mod" | "lib" | "main") {
                dir.to_path_buf()
            } else {
                dir.join(stem)
            };
            let at = ModuleDirs {
                plain,
                path_attr: dir.to_path_buf(),
            };
            collect_inclusions(&parsed.items, &at, false, f, &mut found, &mut uncertain);
        }
        if uncertain {
            return Production {
                test_only: Default::default(),
            };
        }
        let mut test_only = std::collections::HashSet::new();
        loop {
            let before = test_only.len();
            for inc in &found {
                let target = normal(&inc.file);
                if test_only.contains(&target) {
                    continue;
                }
                let all_test = found
                    .iter()
                    .filter(|other| normal(&other.file) == target)
                    .all(|other| other.in_test || test_only.contains(&normal(&other.from)));
                if all_test {
                    test_only.insert(target);
                }
            }
            if test_only.len() == before {
                break;
            }
        }
        Production { test_only }
    }

    /// Whether only test code includes `f`.
    pub fn is_test_only(&self, f: &Path) -> bool {
        self.test_only.contains(&normal(f))
    }

    pub fn text(&self, f: &Path) -> String {
        if self.test_only.contains(&normal(f)) {
            return String::new();
        }
        strip_test_code(&std::fs::read_to_string(f).unwrap_or_default())
    }
}

/// Where a module declared at this point is looked for: `plain` for
/// `mod x;`, `path_attr` for `#[path = "…"] mod x;`. They differ only at
/// a file's top level, where a `#[path]` is relative to the file's own
/// directory (the Rust reference, "The path attribute").
struct ModuleDirs {
    plain: PathBuf,
    path_attr: PathBuf,
}

fn collect_inclusions(
    items: &[syn::Item],
    at: &ModuleDirs,
    in_test: bool,
    from: &Path,
    found: &mut Vec<Inclusion>,
    uncertain: &mut bool,
) {
    use quote::ToTokens;
    for item in items {
        let syn::Item::Mod(m) = item else {
            continue;
        };
        let in_test = in_test || m.attrs.iter().any(is_cfg_test);
        let name = m.ident.to_string();
        let path = m.attrs.iter().find_map(path_attr);
        let conditional_path = m.attrs.iter().any(|a| {
            a.path().is_ident("cfg_attr") && a.meta.to_token_stream().to_string().contains("path")
        });
        if conditional_path && !in_test {
            *uncertain = true;
            continue;
        }
        match &m.content {
            None => {
                let target = match &path {
                    Some(p) => Some(at.path_attr.join(p)).filter(|p| p.exists()),
                    None => [
                        at.plain.join(format!("{name}.rs")),
                        at.plain.join(&name).join("mod.rs"),
                    ]
                    .into_iter()
                    .find(|p| p.exists()),
                };
                match target {
                    Some(file) => found.push(Inclusion {
                        file,
                        from: from.to_path_buf(),
                        in_test,
                    }),
                    None if !in_test => *uncertain = true,
                    None => {}
                }
            }
            Some((_, inner)) => {
                if path.is_some() {
                    if !in_test {
                        *uncertain = true;
                    }
                    continue;
                }
                let dir = at.plain.join(&name);
                let nested = ModuleDirs {
                    plain: dir.clone(),
                    path_attr: dir,
                };
                collect_inclusions(inner, &nested, in_test, from, found, uncertain);
            }
        }
    }
}

fn is_cfg_test(a: &syn::Attribute) -> bool {
    a.path().is_ident("cfg")
        && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
}

fn path_attr(a: &syn::Attribute) -> Option<String> {
    let syn::Meta::NameValue(nv) = &a.meta else {
        return None;
    };
    if !nv.path.is_ident("path") {
        return None;
    }
    match &nv.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Some(s.value()),
        _ => None,
    }
}

fn normal(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Enough Rust lexing to count braces that are actually CODE.
///
/// The stripper used to count every `{` and `}` character, so a test
/// containing a brace inside a literal drove its depth negative, ended
/// the test module early, and handed the rest of that file's TEST code
/// to every gate as production. Measured: `git_boundary` reported five
/// such sites in `status_tui`, then two more in `open_zellij` and
/// `agent` once the single-line cases were handled. A standing red gate
/// is the failure this plan exists to end, so this is fixed in the
/// shared stripper rather than worked around in the files that trip it.
#[derive(Default)]
struct Lexer {
    /// What is still open from a previous line. All three of these span
    /// lines in Rust, and the multi-line RAW string is the one that
    /// actually bit: `open_zellij` and `agent` both embed brace-heavy
    /// JSON fixtures that way, and a fixture's braces are not code.
    inside: Option<Inside>,
}

enum Inside {
    BlockComment,
    Str,
    Raw(usize),
}

impl Lexer {
    /// Feed one line, calling `code` for each character that is outside
    /// strings, chars and comments.
    fn scan(&mut self, line: &str, code: &mut impl FnMut(char)) {
        let b = line.as_bytes();
        let mut i = 0;
        loop {
            match self.inside {
                Some(Inside::BlockComment) => match find(b, i, b"*/") {
                    Some(at) => {
                        self.inside = None;
                        i = at + 2;
                    }
                    None => return,
                },
                Some(Inside::Str) => {
                    while i < b.len() && b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    if i >= b.len() {
                        return;
                    }
                    self.inside = None;
                    i += 1;
                }
                Some(Inside::Raw(hashes)) => {
                    let close: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    match find(b, i, &close) {
                        Some(at) => {
                            self.inside = None;
                            i = at + close.len();
                        }
                        None => return,
                    }
                }
                None => {
                    if i >= b.len() {
                        return;
                    }
                    match b[i] {
                        b'/' if b.get(i + 1) == Some(&b'/') => return,
                        b'/' if b.get(i + 1) == Some(&b'*') => {
                            self.inside = Some(Inside::BlockComment);
                            i += 2;
                        }
                        b'"' => {
                            self.inside = Some(Inside::Str);
                            i += 1;
                        }
                        // A char literal, not a lifetime: `'}'` and
                        // `'\u{1b}'` must not count, and `'a` must not
                        // swallow the line.
                        b'\'' if b.get(i + 1) == Some(&b'\\') => {
                            i += 3;
                            while i < b.len() && b[i] != b'\'' {
                                i += 1;
                            }
                            i += 1;
                        }
                        b'\'' if char_literal_len(line, i).is_some() => {
                            i += char_literal_len(line, i).unwrap();
                        }
                        b'r' if raw_opens(b, i).is_some() => {
                            let (hashes, after) = raw_opens(b, i).unwrap();
                            self.inside = Some(Inside::Raw(hashes));
                            i = after;
                        }
                        c => {
                            code(c as char);
                            i += 1;
                        }
                    }
                }
            }
        }
    }
}

/// The byte length of an unescaped char literal opening at `i`, such as
/// `'}'` or `'é'`.
fn char_literal_len(line: &str, i: usize) -> Option<usize> {
    let c = line.get(i + 1..)?.chars().next()?;
    let close = i + 1 + c.len_utf8();
    (line.as_bytes().get(close) == Some(&b'\'')).then_some(close + 1 - i)
}

/// The hash count and the index just past `r#*"`, if one opens at `i`.
fn raw_opens(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut hashes = 0;
    let mut j = i + 1;
    while b.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    (b.get(j) == Some(&b'"')).then_some((hashes, j + 1))
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    (from..b.len().saturating_sub(needle.len() - 1)).find(|&k| b[k..].starts_with(needle))
}

/// A file is test code exactly when test code is all that includes it,
/// read from the parsed module tree.
///
/// `grok_hook_tests.rs` is the real shape: nothing in the file says it is
/// a test, the `#[cfg(test)] #[path = …] mod tests;` that loads it does.
#[test]
fn a_file_only_test_code_includes_is_test_code() {
    const GIT: &str = "pub fn f() { std::process::Command::new(\"git\"); }\n";
    // Which of `layout`'s files the gates would scan as production.
    let scanned = |layout: &[(&str, &str)]| -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in layout {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let mut files = Vec::new();
        rs_files(dir.path(), &mut files);
        let production = Production::of(&files);
        let mut out: Vec<String> = files
            .iter()
            .filter(|f| production.text(f).contains("Command::new"))
            .map(|f| f.strip_prefix(dir.path()).unwrap().display().to_string())
            .collect();
        out.sort();
        out
    };
    let with = |host: &str| {
        vec![
            ("lib.rs", host.to_string()),
            ("host_tests.rs", GIT.to_string()),
        ]
    };
    let run = |layout: Vec<(&str, String)>| {
        let refs: Vec<(&str, &str)> = layout.iter().map(|(p, b)| (*p, b.as_str())).collect();
        scanned(&refs)
    };
    let none: Vec<String> = Vec::new();
    let host_tests = vec!["host_tests.rs".to_string()];

    let guarded = "#[cfg(test)]\n#[path = \"host_tests.rs\"]\nmod tests;\n";
    assert_eq!(run(with(guarded)), none, "included only by test code");
    let one_line = "#[cfg(test)] #[path = \"host_tests.rs\"] mod tests;\n";
    assert_eq!(run(with(one_line)), none);

    let unguarded = "#[path = \"host_tests.rs\"]\nmod tests;\n";
    assert_eq!(
        run(with(unguarded)),
        host_tests,
        "the guard is what exempts it"
    );

    let commented = format!(
        "{guarded}#[path = \"host_tests.rs\"]\n// a comment between the attribute and its item\nmod live;\n"
    );
    assert_eq!(
        run(with(&commented)),
        host_tests,
        "a production inclusion wins, comment or not"
    );

    let quoted = "#[cfg(test)]\nmod tests {\n    const EXAMPLE: &str = r#\"\n#[path = \"host_tests.rs\"]\nmod imaginary;\n\"#;\n}\n";
    assert_eq!(
        run(with(quoted)),
        host_tests,
        "a declaration in a string includes nothing"
    );

    assert_eq!(
        run(with("pub fn f() {}\n")),
        host_tests,
        "included by nothing found"
    );

    // Inside an inline module a `#[path]` is relative to that module's
    // directory: `lib.rs`'s `mod tests { #[path = "shared.rs"] mod inner; }`
    // loads `tests/shared.rs`, and the root `shared.rs` is someone else.
    let inline = "#[cfg(test)]\nmod tests {\n    #[path = \"shared.rs\"]\n    mod inner;\n}\n";
    assert_eq!(
        run(vec![
            ("lib.rs", inline.to_string()),
            ("tests/shared.rs", GIT.to_string()),
            ("shared.rs", GIT.to_string()),
        ]),
        vec!["shared.rs".to_string()],
    );

    let via = |host: &str| {
        vec![
            ("lib.rs", host.to_string()),
            (
                "host_tests.rs",
                "#[path = \"leaf.rs\"]\nmod leaf;\n".to_string(),
            ),
            ("leaf.rs", GIT.to_string()),
        ]
    };
    assert_eq!(run(via(guarded)), none, "test-only by inheritance");
    assert_eq!(
        run(via(unguarded)),
        vec!["leaf.rs".to_string()],
        "and production through a production file"
    );

    let mut unresolved = with(guarded);
    unresolved.push(("other.rs", "mod nowhere;\n".to_string()));
    assert_eq!(
        run(unresolved),
        host_tests,
        "an inclusion clank cannot resolve might be production"
    );
    let mut unparsed = with(guarded);
    unparsed.push(("broken.rs", "fn {\n".to_string()));
    assert_eq!(
        run(unparsed),
        host_tests,
        "nor can a file that does not parse"
    );
}

/// The stripper keeps production and drops tests, and is not fooled by
/// braces that are not code.
///
/// Every case here is one that made it end a test module early and
/// hand that file's tests to the gates as production. The multi-line
/// RAW string is the one that actually bit, twice.
#[test]
fn the_stripper_counts_only_code_braces() {
    let kept = |src: &str| strip_test_code(src);

    // The baseline shape: production survives, the test module goes.
    let out = kept("fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn after() {}\n");
    assert!(out.contains("fn prod()"), "{out}");
    assert!(
        out.contains("fn after()"),
        "production AFTER the module: {out}"
    );
    assert!(!out.contains("fn t()"), "test code must be dropped: {out}");

    // A brace inside a literal must not close the module early, or the
    // lines after it are handed over as production.
    for literal in [
        r#"    let s = "}";"#,
        r#"    let s = "{";"#,
        "    let c = '}';",
        r#"    let s = "{a}";"#,
        "    // }",
        "    /* } */",
    ] {
        let src = format!(
            "fn prod() {{}}\n#[cfg(test)]\nmod tests {{\n{literal}\n    fn leak() {{}}\n}}\n"
        );
        let out = kept(&src);
        assert!(
            !out.contains("fn leak()"),
            "a brace in `{literal}` ended the module early:\n{out}"
        );
    }

    // A RAW string spanning lines, carrying braces. This is the shape
    // that was still broken after the single-line cases were handled.
    let src = concat!(
        "fn prod() {}\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    const J: &str = r#\"{\n",
        "      \"agents\": { \"a\": {} }\n",
        "    }\"#;\n",
        "    fn leak() {}\n",
        "}\n",
    );
    let out = kept(src);
    assert!(
        !out.contains("fn leak()"),
        "a multi-line raw string ended the module early:\n{out}"
    );

    // And a normal string spanning lines, which Rust also allows.
    let src = concat!(
        "fn prod() {}\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    const S: &str = \"{\n",
        "      }\";\n",
        "    fn leak() {}\n",
        "}\n",
    );
    assert!(
        !kept(src).contains("fn leak()"),
        "a multi-line string ended the module early:\n{}",
        kept(src)
    );

    // A block comment spanning lines.
    let src = concat!(
        "fn prod() {}\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    /* {\n",
        "       } */\n",
        "    fn leak() {}\n",
        "}\n",
    );
    assert!(
        !kept(src).contains("fn leak()"),
        "a multi-line block comment ended the module early"
    );

    // An escaped char literal is longer than four bytes: `'\u{1b}'`
    // carries a brace. This is the shape that ended `stop_hook.rs`'s test
    // module early and handed its fixture `git` call to the gate.
    for literal in [
        "    assert!(!hint.contains('\\u{1b}'));",
        "    let c = '\\u{7d}';",
        "    let c = '\\'';",
        "    let c = '\\\\';",
        "    let c = 'é'; let d = '}';",
    ] {
        let src = format!(
            "fn prod() {{}}\n#[cfg(test)]\nmod tests {{\n{literal}\n    fn leak() {{}}\n}}\nfn after() {{}}\n"
        );
        let out = kept(&src);
        assert!(
            !out.contains("fn leak()"),
            "`{literal}` ended the module early:\n{out}"
        );
        assert!(
            out.contains("fn after()"),
            "`{literal}` swallowed production:\n{out}"
        );
    }

    // A test item without a body ends at its semicolon; the production
    // after it is kept.
    for item in [
        "#[cfg(test)]\n#[path = \"x_tests.rs\"]\nmod tests;",
        "#[cfg(test)] #[path = \"x_tests.rs\"] mod tests;",
        "#[cfg(test)]\nuse std::fmt;",
    ] {
        let src = format!("fn prod() {{}}\n{item}\nfn after() {{}}\n#[cfg(test)]\nfn t() {{}}\n");
        let out = kept(&src);
        assert!(
            out.contains("fn after()"),
            "`{item}` swallowed production:\n{out}"
        );
        assert!(!out.contains("fn t()"), "{out}");
    }

    // Line numbers survive: a dropped line is an empty line.
    let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn b() {}\n";
    assert_eq!(kept(src).lines().nth(5), Some("fn b() {}"));

    // A lifetime is not a char literal: `'a` must not swallow the rest
    // of the line and lose its braces.
    let src = "fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn t<'a>(x: &'a str) {}\n    fn leak() {}\n}\nfn after() {}\n";
    let out = kept(src);
    assert!(!out.contains("fn leak()"), "{out}");
    assert!(
        out.contains("fn after()"),
        "and the module still ends: {out}"
    );
}

pub fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The workspace `crates/` dir (this test's manifest is `crates/cli`).
pub fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .to_path_buf()
}
