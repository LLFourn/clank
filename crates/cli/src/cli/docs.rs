//! `clank docs cli`: the book's command reference, generated from the
//! clap tree so it cannot say anything `--help` does not.
//!
//! Rendered from clap's data rather than its help formatter, whose
//! wrapping follows the terminal and whose `--version` carries the
//! build stamp: the checked-in pages must come out the same on every
//! machine.

use crate::cli::html::esc;
use clap::{Arg, ArgAction, Command};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct DocsArgs {
    #[command(subcommand)]
    pub command: DocsCommand,
}

#[derive(clap::Subcommand, Debug)]
pub enum DocsCommand {
    /// Write the command reference pages into a directory
    ///
    /// One page per visible command and subcommand, and an index. A
    /// page left in the directory by a command that no longer exists is
    /// removed.
    Cli {
        /// Where the pages go: `book/reference` for the book
        dir: PathBuf,
    },
}

pub fn run(args: DocsArgs) -> anyhow::Result<()> {
    use clap::CommandFactory;
    match args.command {
        DocsCommand::Cli { dir } => {
            write_reference(&dir, &reference_pages(crate::cli::command::Cli::command()))
        }
    }
}

/// Every page of `cmd`'s reference, by file name: `index.html` for
/// `cmd` itself and `<full-path>.html` for each visible subcommand.
pub fn reference_pages(mut cmd: Command) -> BTreeMap<String, String> {
    if cmd.get_bin_name().is_none() {
        let name = cmd.get_name().to_string();
        cmd = cmd.bin_name(name);
    }
    cmd.build();
    let mut pages = BTreeMap::new();
    add_pages(&cmd, &[], &mut pages);
    pages
}

/// Writes `pages` into `dir` and removes any other `.html` file there,
/// so a removed command takes its page with it.
pub fn write_reference(dir: &Path, pages: &BTreeMap<String, String>) -> anyhow::Result<()> {
    use anyhow::Context;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        let stale = path.extension().is_some_and(|e| e == "html")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| !pages.contains_key(n));
        if stale {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    for (name, html) in pages {
        let path = dir.join(name);
        std::fs::write(&path, html).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn visible_subcommands(cmd: &Command) -> Vec<&Command> {
    let mut subs: Vec<&Command> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        .collect();
    subs.sort_by_key(|s| s.get_name());
    subs
}

fn page_name(path: &[&Command]) -> String {
    if path.len() <= 1 {
        return "index.html".to_string();
    }
    let names: Vec<&str> = path.iter().map(|c| c.get_name()).collect();
    format!("{}.html", names.join("-"))
}

/// The full invocation, as clap's build wrote it.
fn invocation(cmd: &Command) -> &str {
    cmd.get_bin_name().unwrap_or(cmd.get_name())
}

fn add_pages<'a>(
    cmd: &'a Command,
    ancestors: &[&'a Command],
    pages: &mut BTreeMap<String, String>,
) {
    let mut path = ancestors.to_vec();
    path.push(cmd);
    let name = page_name(&path);
    let html = page(&path, &rail(path[0], &name));
    assert!(
        pages.insert(name.clone(), html).is_none(),
        "two commands share the page {name}"
    );
    for sub in visible_subcommands(cmd) {
        add_pages(sub, &path, pages);
    }
}

/// The reference's own navigation: the index and each top-level
/// command, with `current` marked.
fn rail(root: &Command, current: &str) -> String {
    let item = |href: &str, text: &str| {
        let mark = if href == current {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!(
            "        <li><a href=\"{href}\"{mark}>{}</a></li>\n",
            esc(text)
        )
    };
    let mut items = item("index.html", "All commands");
    for sub in visible_subcommands(root) {
        items.push_str(&item(&page_name(&[root, sub]), invocation(sub)));
    }
    format!(
        "<nav class=\"rail\" id=\"chapters\" aria-label=\"Commands\">
  <div class=\"rail-inner\">
    <a class=\"brand\" href=\"../index.html\">clank</a>
    <span class=\"brand-sub\">Command reference</span>
    <ol class=\"chapters commands\">
{items}    </ol>
    <p class=\"rail-foot\"><a href=\"../index.html\">Back to the book</a></p>
  </div>
</nav>
"
    )
}

const THEME_SCRIPT: &str = r#"<script>try{var t=new URLSearchParams(location.search).get("theme");if(t==="dark"||t==="light")document.documentElement.dataset.theme=t}catch(e){}</script>"#;

fn page(path: &[&Command], rail: &str) -> String {
    let cmd = *path.last().expect("a page has a command");
    let is_index = path.len() == 1;
    let title = if is_index {
        "Command reference".to_string()
    } else {
        invocation(cmd).to_string()
    };
    let about = cmd.get_about().map(|a| a.to_string()).unwrap_or_default();

    let mut body = String::new();
    let mut crumbs = vec![];
    if !is_index {
        crumbs.push("<a href=\"index.html\">Command reference</a>".to_string());
        for i in 2..path.len() {
            crumbs.push(format!(
                "<a href=\"{}\">{}</a>",
                page_name(&path[..i]),
                esc(invocation(path[i - 1]))
            ));
        }
    }
    if !crumbs.is_empty() {
        body.push_str(&format!(
            "<p class=\"chapter-no\">{}</p>\n",
            crumbs.join(" / ")
        ));
    }
    body.push_str(&format!("<h1>{}</h1>\n", esc(&title)));
    if is_index {
        body.push_str(&format!(
            "<p class=\"lede\">Every <code>{}</code> command, generated from the command-line definitions the binary is built from. Each page says what that command's <code>--help</code> says.</p>\n",
            esc(invocation(cmd))
        ));
        if !about.is_empty() {
            body.push_str(&format!("<p>{}</p>\n", inline(&about)));
        }
    } else if !about.is_empty() {
        body.push_str(&format!("<p class=\"lede\">{}</p>\n", inline(&about)));
    }

    if let Some(before) = cmd.get_before_long_help().or(cmd.get_before_help()) {
        body.push_str(&prose(&before.to_string()));
    }
    if let Some(long) = cmd.get_long_about() {
        let long = long.to_string();
        let rest = long
            .strip_prefix(about.as_str())
            .map(str::trim_start)
            .unwrap_or(&long);
        body.push_str(&prose(rest));
    }

    // Not a `<pre>`: book_is_true parses every `<pre>` line as a command
    // to type, and a usage line is a pattern, not a command.
    let usage = cmd.clone().render_usage().to_string();
    let usage = usage.strip_prefix("Usage: ").unwrap_or(&usage);
    let usage: Vec<String> = usage.lines().map(|l| esc(l.trim())).collect();
    body.push_str(&format!(
        "<h2 id=\"usage\">Usage</h2>\n<p class=\"usage\"><samp>{}</samp></p>\n",
        usage.join("\n")
    ));

    let subs = visible_subcommands(cmd);
    if !subs.is_empty() {
        body.push_str(&commands_table(path, &subs));
    }

    let args: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !a.is_hide_long_help_set())
        .collect();
    let (mut positionals, options): (Vec<&Arg>, Vec<&Arg>) =
        args.into_iter().partition(|a| a.is_positional());
    positionals.sort_by_key(|a| a.get_index());
    if !positionals.is_empty() {
        body.push_str("<h2 id=\"arguments\">Arguments</h2>\n<dl class=\"terms\">\n");
        for arg in &positionals {
            body.push_str(&arg_entry(arg, path));
        }
        body.push_str("</dl>\n");
    }
    if !options.is_empty() {
        body.push_str("<h2 id=\"options\">Options</h2>\n<dl class=\"terms\">\n");
        for arg in &options {
            body.push_str(&arg_entry(arg, path));
        }
        body.push_str("</dl>\n");
    }

    if let Some(after) = cmd.get_after_long_help().or(cmd.get_after_help()) {
        body.push_str(&prose(&after.to_string()));
    }

    let description = if about.is_empty() {
        title.clone()
    } else {
        about.clone()
    };
    format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>{title} | The clank book</title>
<meta name=\"description\" content=\"{description}\">
{THEME_SCRIPT}
<link rel=\"stylesheet\" href=\"../book.css\">
</head>
<body>
<a class=\"skip\" href=\"#text\">Skip to the text</a>
<header class=\"bar\">
  <a class=\"brand\" href=\"../index.html\">clank</a>
  <a class=\"bar-link\" href=\"#chapters\">Commands</a>
</header>
<main id=\"text\">
  <article class=\"page\">
{body}  </article>
</main>
{rail}</body>
</html>
",
        title = esc(&title),
        description = esc(&description),
    )
}

fn commands_table(path: &[&Command], subs: &[&Command]) -> String {
    let mut rows = String::new();
    for sub in subs {
        let mut sub_path = path.to_vec();
        sub_path.push(sub);
        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        let aliases: Vec<&str> = sub.get_visible_aliases().collect();
        let also = if aliases.is_empty() {
            String::new()
        } else {
            format!(" Also <code>{}</code>.", esc(&aliases.join(", ")))
        };
        rows.push_str(&format!(
            "    <tr><td><a href=\"{}\"><code>{}</code></a></td><td>{}{also}</td></tr>\n",
            page_name(&sub_path),
            esc(sub.get_name()),
            inline(&about)
        ));
    }
    format!(
        "<h2 id=\"commands\">Commands</h2>
<div class=\"table-scroll\">
<table>
  <thead><tr><th scope=\"col\">Command</th><th scope=\"col\">What it does</th></tr></thead>
  <tbody>
{rows}  </tbody>
</table>
</div>
"
    )
}

fn takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

fn short_flag(c: char) -> String {
    ['-', c].iter().collect()
}

/// The flags and the values one occurrence takes, as clap's help writes
/// them: clap's `Display` for an arg is its long (or short) name then
/// that value suffix, so the suffix is reused rather than re-derived.
fn arg_signature(arg: &Arg) -> String {
    let shown = arg.to_string();
    if arg.is_positional() {
        return shown;
    }
    let name = match (arg.get_long(), arg.get_short()) {
        (Some(l), _) => format!("--{l}"),
        (None, Some(s)) => short_flag(s),
        (None, None) => String::new(),
    };
    let suffix = shown.strip_prefix(name.as_str()).unwrap_or(&shown);
    let mut flags = Vec::new();
    if let Some(s) = arg.get_short() {
        flags.push(short_flag(s));
    }
    if let Some(l) = arg.get_long() {
        flags.push(format!("--{l}"));
    }
    format!("{}{suffix}", flags.join(", "))
}

/// Whether the option may be given more than once, which its signature
/// cannot say: `<V>...` there means several values in one occurrence.
fn is_repeatable(arg: &Arg) -> bool {
    !arg.is_positional() && matches!(arg.get_action(), ArgAction::Append | ArgAction::Count)
}

/// The command a global `arg` was declared on: the outermost one on
/// `path` that has it.
fn declared_on<'a>(arg: &Arg, path: &[&'a Command]) -> Option<&'a Command> {
    path.iter()
        .copied()
        .find(|c| c.get_arguments().any(|a| a.get_id() == arg.get_id()))
}

fn arg_entry(arg: &Arg, path: &[&Command]) -> String {
    let anchor = format!(
        "{}-{}",
        if arg.is_positional() { "arg" } else { "option" },
        arg.get_id().as_str()
    );
    let mut dd = String::new();
    if let Some(help) = arg.get_long_help().or(arg.get_help()) {
        dd.push_str(&prose(&help.to_string()));
    }
    let mut facts = Vec::new();
    if arg.is_required_set() && !arg.is_positional() {
        facts.push("Required.".to_string());
    }
    if is_repeatable(arg) {
        facts.push("May be repeated.".to_string());
    }
    if takes_value(arg) && !arg.is_hide_default_value_set() {
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|v| format!("<code>{}</code>", esc(&v.to_string_lossy())))
            .collect();
        if !defaults.is_empty() {
            facts.push(format!("Default: {}.", defaults.join(", ")));
        }
    }
    if let Some(env) = arg.get_env().filter(|_| !arg.is_hide_env_set()) {
        facts.push(format!(
            "Read from <code>{}</code> when not given.",
            esc(&env.to_string_lossy())
        ));
    }
    let aliases: Vec<String> = arg
        .get_visible_short_aliases()
        .unwrap_or_default()
        .into_iter()
        .map(short_flag)
        .chain(
            arg.get_visible_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(|l| format!("--{l}")),
        )
        .collect();
    if !aliases.is_empty() {
        facts.push(format!("Also <code>{}</code>.", esc(&aliases.join(", "))));
    }
    if arg.is_global_set() {
        let cmd = *path.last().expect("an argument has a command");
        match declared_on(arg, path) {
            Some(origin) if !std::ptr::eq(origin, cmd) => {
                let at = path
                    .iter()
                    .position(|c| std::ptr::eq(*c, origin))
                    .expect("the origin is on the path");
                facts.push(format!(
                    "Global, inherited from <a href=\"{}\"><code>{}</code></a>.",
                    page_name(&path[..=at]),
                    esc(invocation(origin))
                ));
            }
            _ => facts.push("Global: every subcommand accepts it too.".to_string()),
        }
    }
    if !facts.is_empty() {
        dd.push_str(&format!("<p>{}</p>\n", facts.join(" ")));
    }
    if takes_value(arg) && !arg.is_hide_possible_values_set() {
        let values: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|v| !v.is_hide_set())
            .map(|v| match v.get_help() {
                Some(help) => format!(
                    "<li><code>{}</code>: {}</li>",
                    esc(v.get_name()),
                    inline(&help.to_string())
                ),
                None => format!("<li><code>{}</code></li>", esc(v.get_name())),
            })
            .collect();
        if !values.is_empty() {
            dd.push_str(&format!(
                "<p>Possible values:</p>\n<ul>\n{}\n</ul>\n",
                values.join("\n")
            ));
        }
    }
    format!(
        "  <dt id=\"{}\"><code>{}</code></dt>\n  <dd>\n{dd}  </dd>\n",
        esc(&anchor),
        esc(&arg_signature(arg))
    )
}

/// Help text as HTML: blank-line paragraphs, `- ` lists, and a block
/// whose first line ends in `:` over indented lines (an `Examples:`
/// block) as a heading over preformatted text.
fn prose(text: &str) -> String {
    let mut out = String::new();
    for block in text.split("\n\n").map(|b| b.trim_matches('\n')) {
        if block.trim().is_empty() {
            continue;
        }
        let lines: Vec<&str> = block.lines().collect();
        let indented = |l: &&str| l.starts_with(' ') || l.trim().is_empty();
        if lines.len() > 1 && lines[0].ends_with(':') && lines[1..].iter().all(indented) {
            let heading = lines[0].trim_end_matches(':').trim();
            let indent = lines[1..]
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            let body: Vec<String> = lines[1..]
                .iter()
                .map(|l| esc(l.get(indent..).unwrap_or("").trim_end()))
                .collect();
            out.push_str(&format!(
                "<h2 id=\"{}\">{}</h2>\n<pre><code>{}</code></pre>\n",
                esc(&slug(heading)),
                esc(heading),
                body.join("\n")
            ));
        } else if lines.iter().all(|l| l.trim_start().starts_with("- ")) {
            let items: Vec<String> = lines
                .iter()
                .map(|l| format!("<li>{}</li>", inline(&l.trim_start()[2..])))
                .collect();
            out.push_str(&format!("<ul>\n{}\n</ul>\n", items.join("\n")));
        } else {
            out.push_str(&format!("<p>{}</p>\n", inline(&lines.join(" "))));
        }
    }
    out
}

fn slug(heading: &str) -> String {
    heading
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// One line of help text as HTML, its backquoted spans as code.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut parts = text.split('`');
    let mut in_code = false;
    if let Some(first) = parts.next() {
        out.push_str(&esc(first));
    }
    let rest: Vec<&str> = parts.collect();
    let unbalanced = rest.len() % 2 == 1;
    for (i, part) in rest.iter().enumerate() {
        in_code = !in_code;
        if unbalanced && i == rest.len() - 1 {
            out.push('`');
            out.push_str(&esc(part));
        } else if in_code {
            out.push_str(&format!("<code>{}</code>", esc(part)));
        } else {
            out.push_str(&esc(part));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    fn fixture() -> Command {
        Command::new("tool")
            .version("9.9.9-stamp")
            .about("A tool for <things> & more")
            .arg(
                Arg::new("repo")
                    .long("repo")
                    .global(true)
                    .value_name("PATH")
                    .help("The repo to act on"),
            )
            .arg(
                Arg::new("secret")
                    .long("secret")
                    .hide(true)
                    .help("Never shown"),
            )
            .subcommand(
                Command::new("group")
                    .about("A group of things")
                    .subcommand(
                        Command::new("leaf")
                            .about("Does the <b>leaf</b> thing")
                            .long_about(
                                "Does the <b>leaf</b> thing\n\nRun `tool group leaf --mode fast` to hurry.",
                            )
                            .after_long_help("Examples:\n  tool group leaf one\n  # quietly\n  tool group leaf one --mode slow")
                            .arg(
                                Arg::new("name")
                                    .required(true)
                                    .value_name("NAME")
                                    .help("Which leaf"),
                            )
                            .arg(
                                Arg::new("mode")
                                    .long("mode")
                                    .value_parser(["fast", "slow"])
                                    .default_value("slow")
                                    .help("How fast"),
                            )
                            .arg(
                                Arg::new("plan")
                                    .long("plan")
                                    .value_name("NAME")
                                    .num_args(0..=1)
                                    .help("One plan, or the active one"),
                            )
                            .arg(
                                Arg::new("env")
                                    .long("env")
                                    .value_name("KEY=VAL")
                                    .action(ArgAction::Append)
                                    .help("One variable each time"),
                            )
                            .arg(
                                Arg::new("pair")
                                    .long("pair")
                                    .value_names(["FROM", "TO"])
                                    .help("Two values at once"),
                            )
                            .arg(
                                Arg::new("files")
                                    .long("files")
                                    .value_name("FILE")
                                    .num_args(1..)
                                    .help("Many values at once"),
                            )
                            .arg(
                                Arg::new("verbose")
                                    .short('v')
                                    .long("verbose")
                                    .action(ArgAction::SetTrue)
                                    .help("Say more"),
                            ),
                    )
                    .subcommand(Command::new("ghost").hide(true).about("Hidden leaf")),
            )
            .subcommand(Command::new("plumbing").hide(true).about("Hidden group"))
    }

    fn pages() -> BTreeMap<String, String> {
        reference_pages(fixture())
    }

    #[test]
    fn every_visible_command_gets_a_page_and_hidden_ones_none() {
        let names: Vec<String> = pages().into_keys().collect();
        assert_eq!(
            names,
            ["index.html", "tool-group-leaf.html", "tool-group.html"]
        );
        let all: String = pages().into_values().collect();
        for hidden in ["ghost", "plumbing", "Hidden", "--secret", "Never shown"] {
            assert!(!all.contains(hidden), "{hidden} leaked");
        }
    }

    #[test]
    fn a_nested_page_names_its_full_path_and_inherits_the_global_option() {
        let pages = pages();
        let leaf = &pages["tool-group-leaf.html"];
        assert!(leaf.contains("<h1>tool group leaf</h1>"), "{leaf}");
        assert!(
            leaf.contains("<samp>tool group leaf [OPTIONS] &lt;NAME&gt;</samp>"),
            "{leaf}"
        );
        assert!(
            leaf.contains("<code>--repo &lt;PATH&gt;</code>"),
            "the inherited global is listed: {leaf}"
        );
        assert!(
            leaf.contains("Global, inherited from <a href=\"index.html\"><code>tool</code></a>."),
            "{leaf}"
        );
        assert!(leaf.contains("The repo to act on"), "{leaf}");
        assert!(
            pages["index.html"].contains("Global: every subcommand accepts it too."),
            "{}",
            pages["index.html"]
        );
    }

    #[test]
    fn options_keep_their_help_defaults_and_possible_values() {
        let leaf = &pages()["tool-group-leaf.html"];
        for want in [
            "<code>&lt;NAME&gt;</code>",
            "Which leaf",
            "<code>--mode &lt;mode&gt;</code>",
            "How fast",
            "Default: <code>slow</code>.",
            "<li><code>fast</code></li>",
            "<li><code>slow</code></li>",
            "<code>-v, --verbose</code>",
            "Say more",
            "<code>tool group leaf --mode fast</code>",
            "<h2 id=\"examples\">Examples</h2>\n<pre><code>tool group leaf one\n# quietly\ntool group leaf one --mode slow</code></pre>",
        ] {
            assert!(leaf.contains(want), "missing {want:?}:\n{leaf}");
        }
        assert!(
            !leaf.contains("Default: <code>false</code>"),
            "a flag's implicit default is not a default the reader sets: {leaf}"
        );
    }

    /// What one occurrence takes, and whether it may recur, are separate:
    /// a repeatable option does not take several values per occurrence.
    #[test]
    fn values_per_occurrence_and_repetition_render_as_clap_does() {
        let leaf = &pages()["tool-group-leaf.html"];
        let entry = |id: &str| {
            let start = leaf
                .find(&format!("<dt id=\"option-{id}\">"))
                .unwrap_or_else(|| panic!("no --{id}: {leaf}"));
            let end = leaf[start..].find("</dd>").unwrap() + start;
            leaf[start..end].to_string()
        };
        let plan = entry("plan");
        assert!(
            plan.contains("<code>--plan [&lt;NAME&gt;]</code>"),
            "{plan}"
        );
        assert!(!plan.contains("May be repeated."), "{plan}");
        let env = entry("env");
        assert!(env.contains("<code>--env &lt;KEY=VAL&gt;</code>"), "{env}");
        assert!(env.contains("May be repeated."), "{env}");
        let pair = entry("pair");
        assert!(
            pair.contains("<code>--pair &lt;FROM&gt; &lt;TO&gt;</code>"),
            "{pair}"
        );
        let files = entry("files");
        assert!(
            files.contains("<code>--files &lt;FILE&gt;...</code>"),
            "{files}"
        );
        assert!(!files.contains("May be repeated."), "{files}");

        let mut cmd = fixture();
        cmd.build();
        let mut leaf_cmd = cmd
            .find_subcommand("group")
            .and_then(|g| g.find_subcommand("leaf"))
            .unwrap()
            .clone();
        let help = leaf_cmd.render_long_help().to_string();
        for shown in [
            "--plan [<NAME>]",
            "--env <KEY=VAL>",
            "--pair <FROM> <TO>",
            "--files <FILE>...",
        ] {
            assert!(help.contains(shown), "clap's help lacks {shown:?}:\n{help}");
        }
    }

    /// Every argument of every real command is signed as that command's
    /// own `--help` signs it.
    #[test]
    fn every_real_signature_is_the_one_help_prints() {
        use clap::CommandFactory;
        fn walk(cmd: &Command, bad: &mut Vec<String>) {
            let help = cmd.clone().render_long_help().to_string();
            for arg in cmd.get_arguments().filter(|a| !a.is_hide_set()) {
                let signature = arg_signature(arg);
                if !help.lines().any(|l| l.trim_start().starts_with(&signature)) {
                    bad.push(format!("{}: {signature}", invocation(cmd)));
                }
            }
            for sub in visible_subcommands(cmd) {
                walk(sub, bad);
            }
        }
        let mut root = crate::cli::command::Cli::command().bin_name("clank");
        root.build();
        let mut bad = Vec::new();
        walk(&root, &mut bad);
        assert!(bad.is_empty(), "signed unlike --help: {bad:#?}");
    }

    #[test]
    fn html_in_help_is_escaped() {
        let pages = pages();
        assert!(pages["index.html"].contains("A tool for &lt;things&gt; &amp; more"));
        let leaf = &pages["tool-group-leaf.html"];
        assert!(
            leaf.contains("Does the &lt;b&gt;leaf&lt;/b&gt; thing"),
            "{leaf}"
        );
        assert!(!leaf.contains("<b>leaf</b>"), "{leaf}");
    }

    #[test]
    fn a_group_page_links_its_visible_children() {
        let group = &pages()["tool-group.html"];
        assert!(
            group.contains("<a href=\"tool-group-leaf.html\"><code>leaf</code></a>"),
            "{group}"
        );
        assert!(
            group.contains("<a href=\"index.html\">Command reference</a>"),
            "{group}"
        );
    }

    #[test]
    fn the_version_stamp_is_not_rendered() {
        let all: String = pages().into_values().collect();
        assert!(!all.contains("9.9.9-stamp"));
    }

    #[test]
    fn two_runs_are_byte_identical() {
        assert_eq!(pages(), pages());
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_reference(a.path(), &pages()).unwrap();
        write_reference(b.path(), &pages()).unwrap();
        for name in pages().keys() {
            assert_eq!(
                std::fs::read(a.path().join(name)).unwrap(),
                std::fs::read(b.path().join(name)).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn writing_removes_a_page_whose_command_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool-removed.html"), "old").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "kept").unwrap();
        write_reference(dir.path(), &pages()).unwrap();
        assert!(!dir.path().join("tool-removed.html").exists());
        assert!(dir.path().join("notes.txt").exists());
        assert!(dir.path().join("tool-group-leaf.html").exists());
    }
}
