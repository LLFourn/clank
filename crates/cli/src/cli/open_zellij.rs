//! `clank open zellij` — auto-generate a zellij layout (KDL)
//! at `<repo>/.clank/zellij/layout.kdl` and spawn
//! `zellij --layout <path>`. The pane commands pin
//! `--repo <abs-path>` so the spawned session's cwd doesn't
//! affect the resolved repo (codex 361b104 catch).

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::vocab::Role;

use super::OpenZellijArgs;
use super::config::{DefaultAgent, load_merged_agents};
use super::{repo_basename, resolve_repo};

pub async fn run(args: OpenZellijArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let agents = load_merged_agents(&repo, home.as_deref())
        .with_context(|| format!("loading agent declaration for `{}`", repo.display()))?;
    let (master, reviewers) = classify_roles(&agents)?;
    let repo_path_str = repo.display().to_string();
    let kdl = compose_kdl(&basename, &repo_path_str, master, &reviewers);
    let layout_path = layout_file_path(&repo);
    let spawn_argv = compose_spawn_argv(&layout_path);

    if args.print {
        println!("{kdl}");
        eprintln!("spawn: {}", spawn_argv.join(" "));
        return Ok(());
    }

    write_layout_file(&repo, &kdl)?;
    ensure_gitignore_zellij_entry(&repo)?;

    let status = std::process::Command::new("zellij")
        .arg("--layout")
        .arg(&layout_path)
        .status()
        .context("spawning zellij --layout")?;
    if !status.success() {
        anyhow::bail!("zellij --layout exited {status}");
    }
    Ok(())
}

fn layout_file_path(repo: &Path) -> PathBuf {
    repo.join(".clank/zellij/layout.kdl")
}

/// Write the KDL atomically: write to `<path>.tmp`, fsync, rename.
fn write_layout_file(repo: &Path, kdl: &str) -> anyhow::Result<PathBuf> {
    let path = layout_file_path(repo);
    let dir = path.parent().expect("layout path has parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating `{}`", dir.display()))?;
    let tmp = dir.join("layout.kdl.tmp");
    std::fs::write(&tmp, kdl).with_context(|| format!("writing `{}`", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming `{}` → `{}`", tmp.display(), path.display()))?;
    Ok(path)
}

/// Idempotently ensure `<repo>/.clank/.gitignore` contains a
/// `/zellij/` line so the generated layout file isn't tracked.
fn ensure_gitignore_zellij_entry(repo: &Path) -> anyhow::Result<()> {
    const ENTRY: &str = "/zellij/";
    let path = repo.join(".clank/.gitignore");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating `{}`", parent.display()))?;
    }
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("reading `{}`", path.display())));
        }
    };
    if body.lines().any(|l| l.trim() == ENTRY) {
        return Ok(());
    }
    let mut new_body = body;
    if !new_body.is_empty() && !new_body.ends_with('\n') {
        new_body.push('\n');
    }
    new_body.push_str(ENTRY);
    new_body.push('\n');
    std::fs::write(&path, new_body).with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}

/// Triage the declaration into (master, reviewers).
///
/// - zero masters → error (suggest `clank agent add ... --role master`)
/// - multiple masters → error (suggest `clank agent promote`)
/// - exactly one master → ok; reviewers preserve declaration order
fn classify_roles(agents: &[DefaultAgent]) -> anyhow::Result<(&DefaultAgent, Vec<&DefaultAgent>)> {
    let masters: Vec<&DefaultAgent> = agents.iter().filter(|a| a.role == Role::Master).collect();
    match masters.len() {
        0 => anyhow::bail!(
            "no master agent registered for this repo — register one with \
             `clank agent add <label> --role master`"
        ),
        1 => {
            let master = masters[0];
            let reviewers: Vec<&DefaultAgent> =
                agents.iter().filter(|a| a.role == Role::Reviewer).collect();
            Ok((master, reviewers))
        }
        _ => {
            let names: Vec<String> = masters
                .iter()
                .map(|a| a.label.as_str().to_string())
                .collect();
            anyhow::bail!(
                "multiple master agents registered: {}. zellij layout requires \
                 exactly one master. Resolve with `clank agent promote <label>` \
                 to make exactly one of them master (all OTHERS automatically \
                 become reviewers).",
                names.join(", ")
            )
        }
    }
}

/// Hand-rolled KDL composer. Every interpolated string value goes
/// through [`kdl_escape`] — codex caught on 818d8be that
/// `AgentLabel::parse` permits `"`, newlines, control chars,
/// and other KDL-significant chars (it only blocks empty,
/// `.`/`..`, leading `.`, `/`, and `\\`). Naive interpolation
/// would produce malformed (or injected) KDL when zellij tries
/// to consume it.
///
/// `repo_path` is the absolute repo path injected into every
/// pane's `clank agent start --repo <path>` so the spawned
/// session's cwd doesn't affect the resolved repo. Codex caught
/// the gap on 361b104.
fn compose_kdl(
    tab_name: &str,
    repo_path: &str,
    master: &DefaultAgent,
    reviewers: &[&DefaultAgent],
) -> String {
    let mut out = String::new();
    let tab_name_esc = kdl_escape(tab_name);
    out.push_str("layout {\n");
    out.push_str(&format!("    tab name=\"{tab_name_esc}\" {{\n"));
    out.push_str("        pane size=1 borderless=true {\n");
    out.push_str("            plugin location=\"zellij:tab-bar\"\n");
    out.push_str("        }\n");
    out.push_str("        pane split_direction=\"horizontal\" {\n");
    push_pane(&mut out, master.label.as_str(), "master", repo_path);
    for reviewer in reviewers {
        push_pane(&mut out, reviewer.label.as_str(), "reviewer", repo_path);
    }
    out.push_str("        }\n");
    out.push_str("        pane size=2 borderless=true {\n");
    out.push_str("            plugin location=\"zellij:status-bar\"\n");
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

fn push_pane(out: &mut String, label: &str, role_str: &str, repo_path: &str) {
    let label_esc = kdl_escape(label);
    let role_esc = kdl_escape(role_str);
    let repo_esc = kdl_escape(repo_path);
    // `cwd` is per-pane so the launched tool (e.g. `claude
    // --resume`, which doesn't take a path argument) runs in
    // the repo regardless of the shell that invoked
    // `zellij --layout`. Codex caught on 8075d43 that pinning
    // `--repo` on `clank agent start` only fixes clank-side
    // resolution; the exec'd tool inherits process cwd.
    out.push_str(&format!(
        "            pane name=\"{label_esc} ({role_esc})\" cwd=\"{repo_esc}\" {{\n"
    ));
    out.push_str("                command \"clank\"\n");
    out.push_str(&format!(
        "                args \"agent\" \"start\" \"{label_esc}\" \"--repo\" \"{repo_esc}\"\n"
    ));
    out.push_str("            }\n");
}

/// Escape a string for use inside a KDL `"..."` quoted string.
/// KDL's escape syntax matches C-style: `\\`, `\"`, `\n`, `\r`,
/// `\t`, plus `\u{XXXX}` for other control characters.
fn kdl_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The argv we would pass to `zellij --layout <path>` — emitted
/// on stderr in `--print` mode so tests + tools can inspect
/// without observing the spawn.
fn compose_spawn_argv(layout_path: &Path) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "--layout".to_string(),
        layout_path.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::ids::AgentLabel;

    fn agent(label: &str, role: Role) -> DefaultAgent {
        DefaultAgent {
            label: AgentLabel::parse(label).unwrap(),
            role,
            tool: None,
            launch: None,
            initial_prompt: None,
        }
    }

    #[test]
    fn classify_roles_zero_master_errors_with_suggestion() {
        let agents = vec![agent("alice", Role::Reviewer)];
        let err = classify_roles(&agents).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no master") && msg.contains("clank agent add"),
            "diagnostic should name the fix; got: {msg}"
        );
    }

    #[test]
    fn classify_roles_multi_master_errors_with_both_labels() {
        let agents = vec![
            agent("alice", Role::Master),
            agent("bob", Role::Master),
            agent("carol", Role::Reviewer),
        ];
        let err = classify_roles(&agents).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("alice") && msg.contains("bob"),
            "multi-master diagnostic must list BOTH masters; got: {msg}"
        );
        assert!(
            msg.contains("clank agent promote"),
            "diagnostic should suggest promote; got: {msg}"
        );
    }

    #[test]
    fn classify_roles_single_master_returns_master_and_reviewers_in_order() {
        let agents = vec![
            agent("master", Role::Master),
            agent("bob", Role::Reviewer),
            agent("alice", Role::Reviewer),
            agent("codex", Role::Reviewer),
        ];
        let (master, reviewers) = classify_roles(&agents).unwrap();
        assert_eq!(master.label.as_str(), "master");
        let labels: Vec<&str> = reviewers.iter().map(|a| a.label.as_str()).collect();
        // Declaration order preserved (NOT alphabetical — codex came after alice).
        assert_eq!(labels, vec!["bob", "alice", "codex"]);
    }

    const TEST_REPO: &str = "/tmp/test-repo";
    const TEST_TAB: &str = "test-repo";

    #[test]
    fn compose_kdl_includes_tab_bar_and_status_bar_plugins() {
        let master = agent("m", Role::Master);
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, &master, &[]);
        assert!(kdl.contains("plugin location=\"zellij:tab-bar\""));
        assert!(kdl.contains("plugin location=\"zellij:status-bar\""));
    }

    #[test]
    fn compose_kdl_wraps_panes_in_tab_block_with_name() {
        let master = agent("alice", Role::Master);
        let kdl = compose_kdl("basename", TEST_REPO, &master, &[]);
        assert!(
            kdl.contains("tab name=\"basename\""),
            "KDL should wrap panes in a tab block with name; got:\n{kdl}"
        );
    }

    #[test]
    fn compose_kdl_master_and_reviewer_panes_use_clank_agent_start_with_repo() {
        let master = agent("alice", Role::Master);
        let bob = agent("bob", Role::Reviewer);
        let carol = agent("carol", Role::Reviewer);
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, &master, &[&bob, &carol]);
        assert!(kdl.contains("name=\"alice (master)\""));
        assert!(kdl.contains("name=\"bob (reviewer)\""));
        assert!(kdl.contains("name=\"carol (reviewer)\""));
        assert!(kdl.contains("command \"clank\""));
        // Each pane's args MUST pin --repo per codex 361b104.
        assert!(
            kdl.contains(&format!(
                "args \"agent\" \"start\" \"alice\" \"--repo\" \"{TEST_REPO}\""
            )),
            "alice's args must pin --repo; got:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!(
                "args \"agent\" \"start\" \"bob\" \"--repo\" \"{TEST_REPO}\""
            )),
            "bob's args must pin --repo; got:\n{kdl}"
        );
        assert!(kdl.contains(&format!(
            "args \"agent\" \"start\" \"carol\" \"--repo\" \"{TEST_REPO}\""
        )));
    }

    #[test]
    fn compose_kdl_panes_set_cwd_to_repo_path() {
        // Codex 8075d43: pinning `--repo` on `clank agent start`
        // fixes config resolution but the exec'd tool (e.g.
        // `claude --resume`) inherits process cwd. Each pane's
        // KDL block sets `cwd="<repo>"` so the spawned tool
        // lands in the repo regardless of the shell that
        // invoked zellij.
        let master = agent("alice", Role::Master);
        let bob = agent("bob", Role::Reviewer);
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, &master, &[&bob]);
        // Master pane.
        assert!(
            kdl.contains(&format!("name=\"alice (master)\" cwd=\"{TEST_REPO}\"")),
            "master pane should set cwd; got:\n{kdl}"
        );
        // Reviewer pane.
        assert!(
            kdl.contains(&format!("name=\"bob (reviewer)\" cwd=\"{TEST_REPO}\"")),
            "reviewer pane should set cwd; got:\n{kdl}"
        );
    }

    #[test]
    fn compose_kdl_reviewer_order_matches_input_order() {
        let master = agent("m", Role::Master);
        let bob = agent("bob", Role::Reviewer);
        let alice = agent("alice", Role::Reviewer);
        let codex = agent("codex", Role::Reviewer);
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, &master, &[&bob, &alice, &codex]);
        let bob_idx = kdl.find("name=\"bob (reviewer)\"").unwrap();
        let alice_idx = kdl.find("name=\"alice (reviewer)\"").unwrap();
        let codex_idx = kdl.find("name=\"codex (reviewer)\"").unwrap();
        assert!(bob_idx < alice_idx, "bob must come before alice");
        assert!(alice_idx < codex_idx, "alice must come before codex");
    }

    #[test]
    fn kdl_escape_escapes_quotes_and_backslashes() {
        // Codex caught on 818d8be that AgentLabel doesn't restrict
        // `"`, `\\`, newlines, etc. — interpolating raw labels
        // into KDL strings would yield malformed output. The
        // escape helper must round-trip these to KDL's C-style
        // escapes.
        assert_eq!(kdl_escape(r#"with"quote"#), r#"with\"quote"#);
        assert_eq!(kdl_escape(r"with\backslash"), r"with\\backslash");
        assert_eq!(kdl_escape("with\nnewline"), "with\\nnewline");
        assert_eq!(kdl_escape("with\rreturn"), "with\\rreturn");
        assert_eq!(kdl_escape("with\ttab"), "with\\ttab");
        assert_eq!(kdl_escape("plain"), "plain");
    }

    #[test]
    fn kdl_escape_escapes_other_control_chars_as_unicode() {
        // Control chars beyond the common ones (0x00–0x1f minus
        // \n/\r/\t) get the \u{XXXX} form so KDL can parse them
        // unambiguously.
        let result = kdl_escape("a\x01b\x7fc");
        assert!(
            result.contains("\\u{1}"),
            "should escape 0x01; got: {result}"
        );
        assert!(
            result.contains("\\u{7f}"),
            "should escape DEL (0x7f); got: {result}"
        );
    }

    #[test]
    fn compose_kdl_escapes_quote_in_label() {
        // Pane name + args interpolations both go through
        // kdl_escape. A pathological label with a literal quote
        // must not break the layout string.
        let master = DefaultAgent {
            label: AgentLabel::parse(r#"weird"name"#).unwrap(),
            role: Role::Master,
            tool: None,
            launch: None,
            initial_prompt: None,
        };
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, &master, &[]);
        // The raw `"name"` text MUST appear escaped, not as a
        // bare `"` that would close the KDL string early.
        assert!(
            kdl.contains(r#"weird\"name"#),
            "label's quote must be escaped in KDL; got:\n{kdl}"
        );
        // Sanity: the well-formed KDL has matching `"` characters
        // (every unescaped `"` is balanced).
        let total_quotes = kdl.matches('"').count();
        let escaped_quotes = kdl.matches("\\\"").count();
        let unescaped = total_quotes - escaped_quotes;
        assert_eq!(
            unescaped % 2,
            0,
            "unescaped quotes must come in pairs; got {unescaped} (total={total_quotes}, escaped={escaped_quotes}). KDL:\n{kdl}"
        );
    }

    #[test]
    fn compose_kdl_escapes_quote_in_tab_name_and_repo_path() {
        // Tab name and repo path both flow through kdl_escape so
        // a pathological cwd or basename can't break the layout.
        let master = agent("m", Role::Master);
        let kdl = compose_kdl(r#"weird"tab"#, r#"/tmp/dir"with"quotes"#, &master, &[]);
        assert!(
            kdl.contains(r#"weird\"tab"#),
            "tab name quote must be escaped; got:\n{kdl}"
        );
        assert!(
            kdl.contains(r#"/tmp/dir\"with\"quotes"#),
            "repo path quote must be escaped; got:\n{kdl}"
        );
    }

    #[test]
    fn compose_spawn_argv_is_zellij_layout_path() {
        let argv = compose_spawn_argv(Path::new("/tmp/repo/.clank/zellij/layout.kdl"));
        assert_eq!(argv[0], "zellij");
        assert_eq!(argv[1], "--layout");
        assert_eq!(argv[2], "/tmp/repo/.clank/zellij/layout.kdl");
        assert_eq!(argv.len(), 3, "no extra args; got: {argv:?}");
    }

    #[test]
    fn layout_file_path_is_under_clank_zellij_dir() {
        let p = layout_file_path(Path::new("/tmp/repo"));
        assert_eq!(p, Path::new("/tmp/repo/.clank/zellij/layout.kdl"));
    }

    #[test]
    fn ensure_gitignore_creates_file_with_entry_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        ensure_gitignore_zellij_entry(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert!(
            body.lines().any(|l| l.trim() == "/zellij/"),
            "expected /zellij/ in gitignore; got: {body:?}"
        );
    }

    #[test]
    fn ensure_gitignore_appends_entry_when_other_entries_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/.gitignore"), "/agents/\n/cache/\n").unwrap();
        ensure_gitignore_zellij_entry(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert!(body.contains("/agents/"), "existing entries preserved");
        assert!(body.contains("/cache/"));
        assert!(body.contains("/zellij/"), "new entry appended");
    }

    #[test]
    fn ensure_gitignore_is_idempotent_when_entry_already_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/.gitignore"), "/zellij/\n").unwrap();
        ensure_gitignore_zellij_entry(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(
            body.matches("/zellij/").count(),
            1,
            "second invocation must NOT add a duplicate entry; got body:\n{body}"
        );
    }

    #[test]
    fn write_layout_file_writes_kdl_to_clank_zellij_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_layout_file(dir.path(), "layout { }\n").unwrap();
        assert_eq!(path, dir.path().join(".clank/zellij/layout.kdl"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "layout { }\n");
    }
}
