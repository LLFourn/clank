//! Enforcement gate: the book in `book/` says only true things.
//!
//! The book is prose about a moving CLI. Without this, a renamed flag
//! leaves a page teaching a command that no longer exists, a moved
//! heading leaves a dead link, and a recording can ship a token the
//! recorder failed to scrub — all silently, because nothing builds the
//! book (the-clank-book, "Keeping it true").
//!
//! Every check is a function over a book root so each is driven
//! against a fixture below; `the_book_is_true` runs them all over the
//! real `book/`.

use clank::cli::command::Cli;
use clap::Parser;
use clap::error::ErrorKind;
use fancy_regex::Regex;
use std::path::{Path, PathBuf};

/// What a page writes as `<var>name</var>`. One token for every
/// placeholder, so a flag that only accepts an enum value must be
/// shown with a real value, not a placeholder.
const PLACEHOLDER: &str = "placeholder";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("bad regex {pattern:?}: {e}"))
}

fn real_book() -> PathBuf {
    crate::common::crates_dir()
        .parent()
        .expect("crates/ has a parent")
        .join("book")
}

fn files_with_ext(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == ext) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn rel<'a>(root: &Path, p: &'a Path) -> std::borrow::Cow<'a, str> {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy()
}

/// A commented-out link or command is not shown to readers.
fn without_comments(html: &str) -> String {
    re(r"(?s)<!--.*?-->").replace_all(html, "").into_owned()
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// The text a reader sees in a code element: placeholders substituted,
/// markup (highlighting spans) dropped, entities decoded.
fn code_text(inner: &str) -> String {
    let substituted = re(r"(?s)<var\b[^>]*>.*?</var>").replace_all(inner, PLACEHOLDER);
    decode_entities(&re(r"<[^>]*>").replace_all(&substituted, ""))
}

/// Shell words of one line, quotes honoured, an unquoted `#` word
/// starting a comment.
fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            None | Some('#') => return Ok(words),
            _ => {}
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            match c {
                '\'' => loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated '".into()),
                    }
                },
                '"' => loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => word.extend(chars.next()),
                        Some(c) => word.push(c),
                        None => return Err("unterminated \"".into()),
                    }
                },
                '\\' => word.extend(chars.next()),
                c => word.push(c),
            }
        }
        words.push(word);
    }
}

/// How much of a command a code element must state.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Shown {
    /// A `<pre>` block: something to type, so it must parse whole.
    Typed,
    /// An inline `<code>` in prose, which may name a command without
    /// its required arguments ("run `clank feedback write`").
    Named,
}

/// Every `clank …` line a page shows, with how it is shown.
fn command_lines(html: &str) -> Vec<(Shown, String)> {
    let html = without_comments(html);
    let pre = re(r"(?s)<pre\b[^>]*>(.*?)</pre>");
    let mut elements: Vec<(Shown, String)> = pre
        .captures_iter(&html)
        .map(|c| (Shown::Typed, code_text(&c.unwrap()[1])))
        .collect();
    let prose = pre.replace_all(&html, "");
    elements.extend(
        re(r"(?s)<code\b[^>]*>(.*?)</code>")
            .captures_iter(&prose)
            .map(|c| (Shown::Named, code_text(&c.unwrap()[1]))),
    );

    let mut out = Vec::new();
    for (shown, text) in elements {
        let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
        for line in joined.lines() {
            let line = line.trim();
            let line = line.strip_prefix("$ ").unwrap_or(line);
            if line == "clank" || line.starts_with("clank ") {
                out.push((shown, line.to_string()));
            }
        }
    }
    out
}

fn parse_failure(shown: Shown, line: &str) -> Option<String> {
    let words = match shell_words(line) {
        Ok(w) => w,
        Err(e) => return Some(e),
    };
    let err = Cli::try_parse_from(&words).err()?;
    let accepted = match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => true,
        ErrorKind::MissingRequiredArgument
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::MissingSubcommand => shown == Shown::Named,
        _ => false,
    };
    (!accepted).then(|| err.render().to_string())
}

/// Every command line the book shows that the real `Cli` rejects,
/// plus how many lines were checked.
fn unparsable_commands(root: &Path) -> (Vec<String>, usize) {
    let mut bad = Vec::new();
    let mut checked = 0;
    for page in files_with_ext(root, "html") {
        for (shown, line) in command_lines(&read(&page)) {
            checked += 1;
            if let Some(why) = parse_failure(shown, &line) {
                bad.push(format!("{}: `{line}`: {}", rel(root, &page), why.trim()));
            }
        }
    }
    (bad, checked)
}

fn ids(html: &str) -> Vec<String> {
    re(r#"\bid\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
        .captures_iter(html)
        .map(|c| {
            let c = c.unwrap();
            c.get(1).or(c.get(2)).unwrap().as_str().to_string()
        })
        .collect()
}

/// Every relative `href`/`src` whose file is missing or outside the
/// book, and every `#anchor` with no matching `id` in its target page.
fn broken_links(root: &Path) -> Vec<String> {
    let root = root.canonicalize().expect("book root exists");
    let attr = re(r#"\b(?:href|src)\s*=\s*(?:"([^"]*)"|'([^']*)')"#);
    let scheme = re(r"^[A-Za-z][A-Za-z0-9+.-]*:");
    let mut bad = Vec::new();
    for page in files_with_ext(&root, "html") {
        let html = without_comments(&read(&page));
        for c in attr.captures_iter(&html) {
            let c = c.unwrap();
            let target = decode_entities(c.get(1).or(c.get(2)).unwrap().as_str());
            if target.is_empty() || target.starts_with("//") || scheme.is_match(&target).unwrap() {
                continue;
            }
            let (path, anchor) = match target.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (target.as_str(), None),
            };
            let path = path.split('?').next().unwrap();
            let file = if path.is_empty() {
                page.clone()
            } else {
                page.parent().unwrap().join(path)
            };
            let where_ = format!("{}: `{target}`", rel(&root, &page));
            let Ok(file) = file.canonicalize() else {
                bad.push(format!("{where_}: no such file"));
                continue;
            };
            if !file.starts_with(&root) {
                bad.push(format!("{where_}: outside book/"));
                continue;
            }
            if let Some(anchor) = anchor.filter(|a| !a.is_empty())
                && !ids(&read(&file)).iter().any(|id| id == anchor)
            {
                bad.push(format!(
                    "{where_}: no id=\"{anchor}\" in {}",
                    rel(&root, &file)
                ));
            }
        }
    }
    bad
}

/// Every `data-cast` scene with no `casts/<scene>.js`.
fn missing_casts(root: &Path) -> Vec<String> {
    let attr = re(r#"\bdata-cast\s*=\s*(?:"([^"]*)"|'([^']*)')"#);
    let mut bad = Vec::new();
    for page in files_with_ext(root, "html") {
        let html = without_comments(&read(&page));
        for c in attr.captures_iter(&html) {
            let c = c.unwrap();
            let scene = c.get(1).or(c.get(2)).unwrap().as_str();
            if !root.join("casts").join(format!("{scene}.js")).is_file() {
                bad.push(format!(
                    "{}: data-cast=\"{scene}\": no casts/{scene}.js",
                    rel(root, &page)
                ));
            }
        }
    }
    bad
}

/// Every place a page or stylesheet would fetch from the network.
/// An `<a href>` is the reader's choice to leave, so it is not one.
fn network_loads(root: &Path) -> Vec<String> {
    let remote = r#"\s*=?\s*["']?\s*(?:https?:)?//"#;
    let loads = [
        re(&format!(r"(?i)\bsrc{remote}")),
        re(&format!(r"(?i)<link\b[^>]*\bhref{remote}")),
        re(r#"(?i)\burl\(\s*["']?\s*(?:https?:)?//"#),
        re(r#"(?i)@import\s+(?:url\()?\s*["']?\s*(?:https?:)?//"#),
    ];
    let mut bad = Vec::new();
    let mut files = files_with_ext(root, "html");
    files.extend(files_with_ext(root, "css"));
    for f in files {
        let text = without_comments(&read(&f));
        for (i, line) in text.lines().enumerate() {
            if loads.iter().any(|r| r.is_match(line).unwrap()) {
                bad.push(format!("{}:{}: {}", rel(root, &f), i + 1, line.trim()));
            }
        }
    }
    bad
}

fn scrub_patterns(root: &Path) -> Vec<Regex> {
    let patterns: Vec<Regex> = read(&root.join("scrub-patterns.txt"))
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(re)
        .collect();
    assert!(
        !patterns.is_empty(),
        "scrub-patterns.txt bans nothing, so every cast would pass"
    );
    patterns
}

/// The terminal escapes `scrub-escapes.txt` lists, as one alternation.
/// The recorder strips the same set, so both scrub the text a reader
/// sees once colours and cursor moves are gone.
fn scrub_escapes(root: &Path) -> Regex {
    let body = read(&root.join("scrub-escapes.txt"));
    let alternatives: Vec<&str> = body
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    assert!(
        !alternatives.is_empty(),
        "scrub-escapes.txt strips nothing, so an escaped token would pass"
    );
    re(&alternatives.join("|"))
}

/// What a reader can see in a cast: its asciicast header, every output
/// event joined so a token split across writes is whole again, and any
/// other event's text.
fn cast_text(root: &Path, file: &Path) -> Result<String, String> {
    let scene = file.file_stem().unwrap().to_string_lossy();
    let src = read(file);
    let prefix = format!("(window.CLANK_CASTS ||= {{}})[\"{scene}\"] = ");
    let body = src
        .trim()
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(';'))
        .ok_or_else(|| format!("{}: not `{prefix}<json string>;`", rel(root, file)))?;
    let cast: String = serde_json::from_str(body)
        .map_err(|e| format!("{}: not a JSON string: {e}", rel(root, file)))?;

    let mut lines = cast.lines();
    let mut text = lines.next().unwrap_or_default().to_string();
    let mut output = String::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let event: (f64, String, String) = serde_json::from_str(line)
            .map_err(|e| format!("{}: bad event {line:?}: {e}", rel(root, file)))?;
        if event.1 == "o" {
            output.push_str(&event.2);
        } else {
            text.push('\n');
            text.push_str(&event.2);
        }
    }
    text.push('\n');
    text.push_str(&output);
    Ok(text)
}

/// Every cast that does not decode, or shows text a scrub rule bans.
fn unscrubbed_casts(root: &Path) -> (Vec<String>, usize) {
    let patterns = scrub_patterns(root);
    let escapes = scrub_escapes(root);
    let casts = files_with_ext(&root.join("casts"), "js");
    let mut bad = Vec::new();
    for file in &casts {
        match cast_text(root, file) {
            Err(e) => bad.push(e),
            Ok(text) => {
                let stripped = escapes.replace_all(&text, "");
                for p in &patterns {
                    let found = match p.find(&text).unwrap() {
                        Some(m) => Some(m.as_str().to_string()),
                        None => p.find(&stripped).unwrap().map(|m| m.as_str().to_string()),
                    };
                    if let Some(m) = found {
                        bad.push(format!(
                            "{}: /{}/ matches {:?}",
                            rel(root, file),
                            p.as_str(),
                            m
                        ));
                    }
                }
            }
        }
    }
    (bad, casts.len())
}

/// Drives every check over the real `book/`. A missing book fails: the
/// gate exists to keep it true, so its absence is not a pass.
#[test]
fn the_book_is_true() {
    let root = real_book();
    assert!(
        root.join("index.html").is_file(),
        "{} has no index.html",
        root.display()
    );
    let (commands, checked) = unparsable_commands(&root);
    let (scrub, casts) = unscrubbed_casts(&root);
    assert!(checked > 0, "the book shows no clank command to check");
    assert!(casts > 0, "the book has no casts to scrub-check");
    let failures: Vec<String> = [
        commands,
        broken_links(&root),
        missing_casts(&root),
        network_loads(&root),
        scrub,
    ]
    .concat();
    assert!(
        failures.is_empty(),
        "the book is not true:\n{}",
        failures.join("\n")
    );
}

const GOOD_CAST: &str = r#"(window.CLANK_CASTS ||= {})["demo"] = "{\"version\": 2, \"width\": 80, \"height\": 24}\n[0.1, \"o\", \"$ clank status\\r\\n\"]\n[0.2, \"o\", \"ok\\r\\n\"]\n";"#;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("casts")).unwrap();
    std::fs::write(
        root.join("index.html"),
        r##"<!doctype html>
<link rel="stylesheet" href="style.css">
<h1 id="top">Clank</h1>
<p>Run <code>clank feedback write</code> when done, or see <a href="guide.html#review">review</a>.</p>
<pre><code># every agent at a glance
$ clank status
clank agent start <var>label</var> \
  --print   # what it would run
clank --help
some output, not a command
</code></pre>
<!-- <a href="gone.html">old</a> -->
<a href="https://github.com/LLFourn/clank">source</a>
<div data-cast="demo"></div>
<script src="player.js"></script>
"##,
    )
    .unwrap();
    std::fs::write(
        root.join("guide.html"),
        r#"<h2 id="review">Review</h2><a href="index.html#top">back</a>"#,
    )
    .unwrap();
    std::fs::write(
        root.join("style.css"),
        "body { background: url(\"bg.png\"); }\n",
    )
    .unwrap();
    std::fs::write(root.join("bg.png"), "").unwrap();
    std::fs::write(root.join("player.js"), "").unwrap();
    std::fs::write(root.join("casts/demo.js"), GOOD_CAST).unwrap();
    std::fs::copy(
        real_book().join("scrub-escapes.txt"),
        root.join("scrub-escapes.txt"),
    )
    .unwrap();
    std::fs::write(
        root.join("scrub-patterns.txt"),
        "# tokens\nghp_[A-Za-z0-9]{8,}\n/Users/\n",
    )
    .unwrap();
    dir
}

fn edit(root: &Path, file: &str, old: &str, new: &str) {
    let p = root.join(file);
    let t = read(&p);
    assert!(t.contains(old), "fixture {file} lacks {old:?}");
    std::fs::write(&p, t.replace(old, new)).unwrap();
}

/// The fixture book, written to the conventions, passes every check;
/// each failing test below is one edit away from it.
#[test]
fn a_true_fixture_passes_every_check() {
    let dir = fixture();
    let root = dir.path();
    let (commands, checked) = unparsable_commands(root);
    assert_eq!(commands, Vec::<String>::new());
    assert_eq!(
        checked,
        4,
        "{:?}",
        command_lines(&read(&root.join("index.html")))
    );
    assert_eq!(broken_links(root), Vec::<String>::new());
    assert_eq!(missing_casts(root), Vec::<String>::new());
    assert_eq!(network_loads(root), Vec::<String>::new());
    assert_eq!(unscrubbed_casts(root), (Vec::new(), 1));
}

/// A renamed subcommand in a `<pre>` block fails the parse.
#[test]
fn a_renamed_subcommand_fails() {
    let dir = fixture();
    edit(dir.path(), "index.html", "$ clank status", "$ clank statuz");
    let (bad, _) = unparsable_commands(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("clank statuz"), "{bad:?}");
}

/// A removed flag on a continued line fails: the continuation is part
/// of the command, not a line of its own.
#[test]
fn an_unknown_flag_on_a_continuation_fails() {
    let dir = fixture();
    edit(dir.path(), "index.html", "  --print", "  --prnt");
    let (bad, _) = unparsable_commands(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
}

/// A `<pre>` command missing its required argument fails, though the
/// same words inline in prose pass.
#[test]
fn a_typed_command_must_be_complete() {
    let dir = fixture();
    edit(
        dir.path(),
        "index.html",
        "$ clank status",
        "$ clank feedback write",
    );
    let (bad, _) = unparsable_commands(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
}

/// An anchor with no matching id in its target page fails.
#[test]
fn a_broken_anchor_fails() {
    let dir = fixture();
    edit(
        dir.path(),
        "guide.html",
        r#"id="review""#,
        r#"id="reviews""#,
    );
    let bad = broken_links(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("#review"), "{bad:?}");
}

/// A link to a missing page, or out of the book, fails.
#[test]
fn a_missing_or_escaping_link_fails() {
    let dir = fixture();
    edit(dir.path(), "guide.html", "index.html#top", "../README.md");
    edit(
        dir.path(),
        "index.html",
        r#"src="player.js""#,
        r#"src="gone.js""#,
    );
    let bad = broken_links(dir.path());
    assert_eq!(bad.len(), 2, "{bad:?}");
}

/// A `data-cast` with no recording fails.
#[test]
fn a_missing_cast_fails() {
    let dir = fixture();
    std::fs::remove_file(dir.path().join("casts/demo.js")).unwrap();
    assert_eq!(missing_casts(dir.path()).len(), 1);
}

/// Each way a page or stylesheet can fetch from the network fails.
#[test]
fn a_network_load_fails() {
    let dir = fixture();
    let root = dir.path();
    edit(
        root,
        "index.html",
        r#"src="player.js""#,
        r#"src="https://cdn.example/player.js""#,
    );
    edit(
        root,
        "index.html",
        r#"href="style.css""#,
        r#"href="//fonts.example/x.css""#,
    );
    edit(
        root,
        "style.css",
        "url(\"bg.png\")",
        "url(https://example.com/bg.png)",
    );
    std::fs::write(
        root.join("extra.css"),
        "@import \"https://fonts.example/f.css\";\n",
    )
    .unwrap();
    assert_eq!(network_loads(root).len(), 4, "{:?}", network_loads(root));
}

/// A token fails when the terminal drew it with an escape inside, split
/// across two writes, as the recorder's stripped pass refuses it.
#[test]
fn a_token_with_an_escape_inside_fails() {
    let dir = fixture();
    edit(
        dir.path(),
        "casts/demo.js",
        r#"\"ok\\r\\n\""#,
        r#"\"ghp_ab\\u001b[0mcd\"]\n[0.3, \"o\", \"ef\\u001b]0;title\\u0007gh1234\""#,
    );
    let (bad, _) = unscrubbed_casts(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("ghp_abcdefgh1234"), "{bad:?}");
}

/// A token in a cast fails even when the terminal wrote it in pieces.
#[test]
fn a_token_split_across_output_events_fails() {
    let dir = fixture();
    edit(
        dir.path(),
        "casts/demo.js",
        r#"\"ok\\r\\n\""#,
        r#"\"ghp_abcd\"]\n[0.3, \"o\", \"efgh1234\""#,
    );
    let (bad, _) = unscrubbed_casts(dir.path());
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("ghp_abcdefgh1234"), "{bad:?}");
}
