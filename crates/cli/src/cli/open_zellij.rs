//! `clank open zellij` — auto-generate a zellij layout (KDL)
//! at `<repo>/.clank/zellij/layout.kdl` and spawn
//! `zellij --layout <path>`. The pane commands pin
//! `--repo <abs-path>` so the spawned session's cwd doesn't
//! affect the resolved repo (codex 361b104 catch).

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::OpenZellijArgs;
use super::{repo_basename, resolve_repo};

pub async fn run(args: OpenZellijArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    // Registration is the resolved team set
    // (`teams-based-agent-registration`): exactly one master plus
    // its reviewers, no role-triage needed.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!("this repo has no team configured. Run `clank init --team <name>` first.");
    };
    let master_label = set.master.as_str().to_string();
    let reviewer_labels: Vec<String> = set
        .commit_reviewers
        .iter()
        .chain(set.gate_reviewers.iter())
        .map(|a| a.label.as_str().to_string())
        .collect();
    let repo_path_str = repo.display().to_string();
    // User-authored layout chrome from `~/.clank/config.json`
    // (`zellij-layout-config-around-agent-panes`); None → the
    // built-in template.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let user_template = home
        .as_deref()
        .map(crate::cli::team::read_user_config)
        .transpose()?
        .and_then(|cfg| cfg.zellij.and_then(|z| z.layout));
    // Orientation from the SPAWNING terminal's dimensions
    // (zellij-default-layout); ioctl stays at the shell, compose
    // stays pure over (cols, rows).
    let (rows, cols) = crate::cli::status_tui::term_size();
    let kdl = compose_kdl(
        &basename,
        &repo_path_str,
        &master_label,
        &reviewer_labels,
        user_template.as_deref(),
        (cols, rows),
    )?;
    let layout_path = layout_file_path(&repo);
    let name = session_name(&basename);

    // Inside zellij: new tab in the current session (no dedup —
    // see in_session_tab_argv). Outside: attach-or-create against
    // the deterministic session name.
    let (pre_argv, spawn_argv) = if std::env::var_os("ZELLIJ").is_some() {
        (None, in_session_tab_argv(&layout_path))
    } else {
        let listing = zellij_list_sessions();
        match decide_session(&listing, &name) {
            SessionPlan::Attach => (None, attach_argv(&name)),
            SessionPlan::Create => (None, create_argv(&layout_path, &name)),
            SessionPlan::DeleteDeadThenCreate => {
                (Some(delete_argv(&name)), create_argv(&layout_path, &name))
            }
        }
    };

    if args.print {
        println!("{kdl}");
        if let Some(pre) = &pre_argv {
            eprintln!("pre-spawn: {}", pre.join(" "));
        }
        eprintln!("spawn: {}", spawn_argv.join(" "));
        return Ok(());
    }

    write_layout_file(&repo, &kdl)?;
    crate::init_facts::ensure_clank_gitignore_entry(&repo, "/zellij/")
        .context("ensuring /zellij/ gitignore entry")?;

    if let Some(pre) = &pre_argv {
        // Best-effort: a failed delete of a dead session just means
        // the create below errors visibly.
        let _ = std::process::Command::new(&pre[0]).args(&pre[1..]).status();
    }
    if spawn_argv[1] == "attach" {
        eprintln!("attaching to existing session `{name}`");
    }
    let status = std::process::Command::new(&spawn_argv[0])
        .args(&spawn_argv[1..])
        .status()
        .context("spawning zellij")?;
    if !status.success() {
        anyhow::bail!("zellij exited {status}");
    }
    Ok(())
}

/// `zellij list-sessions -n` stdout, or empty when the command
/// fails — zellij exits non-zero when NO sessions exist, which is
/// exactly the Create case, so failure degrades to "no sessions".
fn zellij_list_sessions() -> String {
    std::process::Command::new("zellij")
        .args(["list-sessions", "-n"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
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

/// The marker node a template must contain; clank replaces it
/// with the composed agent pane group. A NODE (not a string
/// token) so comments/string-literals can't false-match and the
/// substituted result is valid KDL by construction (ruthless
/// fb3f85f concern 1).
pub(crate) const AGENTS_MARKER: &str = "clank_agents";

/// Built-in zero-config template. Uses `default_tab_template` so
/// the bars apply to runtime-spawned tabs too — previously the
/// bars were panes of clank's tab only and NEW tabs dropped them
/// (the plan's problem #2; fixed for zero-config users as well
/// per ruthless fb3f85f concern 2 option b). `__TAB__` is an
/// internal placeholder interpolated (escaped) before parsing —
/// it never appears in user templates, which own their tab names.
const BUILT_IN_TEMPLATE: &str = r#"layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        children
        pane size=2 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
    tab name="__TAB__" {
        clank_agents
    }
}
"#;

/// Compose the final layout: pick the template (user-authored
/// from `~/.clank/config.json#/zellij/layout`, else the built-in)
/// and tree-substitute the agent pane group at the `clank_agents`
/// marker. One code path for both — the built-in IS a template.
///
/// Every interpolated string value goes through [`kdl_escape`] —
/// codex caught on 818d8be that `AgentLabel::parse` permits `"`,
/// newlines, control chars, and other KDL-significant chars.
///
/// `repo_path` is the absolute repo path injected into every
/// pane's `clank agent start --repo <path>` so the spawned
/// session's cwd doesn't affect the resolved repo (codex 361b104).
fn compose_kdl(
    tab_name: &str,
    repo_path: &str,
    master: &str,
    reviewers: &[String],
    user_template: Option<&str>,
    term: (u16, u16),
) -> anyhow::Result<String> {
    let orientation = Orientation::detect(term);
    let agents = agent_group_kdl(repo_path, master, reviewers, orientation);
    match user_template {
        // User templates: unchanged marker contract — the detected
        // orientation's group, NO swap blocks (swap_tiled_layout is
        // a layout-root construct; splicing it into arbitrary user
        // templates is fragile — template authors write their own
        // swaps).
        Some(t) => substitute_marker(t, &agents),
        None => {
            let built_in = BUILT_IN_TEMPLATE.replace("__TAB__", &kdl_escape(tab_name));
            let base = substitute_marker(&built_in, &agents)?;
            // BOTH orientations ship as swap variants so alt+[ /
            // alt+] flips the arrangement at runtime.
            add_swap_variants(&base, repo_path, master, reviewers)
        }
    }
}

/// Which way the stage/stack axis runs, from the spawning
/// terminal's shape. Terminal cells are ~2:1 (h:w), so a visually
/// square terminal is ~2:1 cols:rows — wider than that reads as
/// landscape. The ioctl's 24x80 fallback lands on landscape, the
/// safe default.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Orientation {
    Landscape,
    Portrait,
}

impl Orientation {
    fn detect((cols, rows): (u16, u16)) -> Self {
        if (cols as u32) >= 2 * rows as u32 {
            Orientation::Landscape
        } else {
            Orientation::Portrait
        }
    }
    fn other(self) -> Self {
        match self {
            Orientation::Landscape => Orientation::Portrait,
            Orientation::Portrait => Orientation::Landscape,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Orientation::Landscape => "landscape",
            Orientation::Portrait => "portrait",
        }
    }
}

/// The agent pane group clank owns: master is the STAGE (~65%),
/// reviewers STACK in the smaller region, and a `clank status
/// --tui` instrument pane sits beside/below the stack
/// (zellij-default-layout). Landscape: master left, right column
/// = stack over tui. Portrait: master top, bottom row = stack
/// beside tui. Zero reviewers: no stack region — stage + tui.
///
/// zellij KDL: `split_direction="vertical"` lays children out as
/// COLUMNS, `"horizontal"` as ROWS.
fn agent_group_kdl(
    repo_path: &str,
    master: &str,
    reviewers: &[String],
    orientation: Orientation,
) -> String {
    let mut out = String::new();
    let (outer, inner) = match orientation {
        Orientation::Landscape => ("vertical", "horizontal"),
        Orientation::Portrait => ("horizontal", "vertical"),
    };
    out.push_str(&format!("pane split_direction=\"{outer}\" {{\n"));
    // The stage.
    push_agent_pane(
        &mut out,
        "    ",
        " size=\"65%\"",
        master,
        "master",
        repo_path,
    );
    // The side region: reviewer stack + instrument pane.
    out.push_str(&format!(
        "    pane size=\"35%\" split_direction=\"{inner}\" {{\n"
    ));
    if !reviewers.is_empty() {
        out.push_str("        pane stacked=true {\n");
        for reviewer in reviewers {
            push_agent_pane(
                &mut out,
                "            ",
                "",
                reviewer.as_str(),
                "reviewer",
                repo_path,
            );
        }
        out.push_str("        }\n");
    }
    // The instrument pane: repo pinned via cwd AND --repo (codex
    // 7d3b5d1 — the 361b104/8075d43 lineage applies to every
    // generated command, not just agent panes). Small on purpose:
    // the TUI degrades gracefully (1-row bar invariant).
    let tui_size = match orientation {
        Orientation::Landscape => "size=10",
        Orientation::Portrait => "size=\"30%\"",
    };
    let repo_esc = kdl_escape(repo_path);
    out.push_str(&format!(
        "        pane {tui_size} name=\"status\" cwd=\"{repo_esc}\" {{\n"
    ));
    out.push_str("            command \"clank\"\n");
    out.push_str(&format!(
        "            args \"status\" \"--repo\" \"{repo_esc}\" \"--tui\"\n"
    ));
    out.push_str("        }\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

/// Insert `swap_tiled_layout` blocks (one per orientation) into
/// the composed built-in layout so alt+[ / alt+] flips the
/// arrangement. Tree-inserted into the parsed doc (valid KDL by
/// construction), never string-spliced.
fn add_swap_variants(
    base: &str,
    repo_path: &str,
    master: &str,
    reviewers: &[String],
) -> anyhow::Result<String> {
    let mut doc: kdl::KdlDocument = base.parse().expect("composed built-in layout is valid KDL");
    let mut swaps = String::new();
    for o in [Orientation::Landscape, Orientation::Portrait] {
        swaps.push_str(&format!("swap_tiled_layout name=\"{}\" {{\n", o.name()));
        swaps.push_str("    tab {\n");
        for line in agent_group_kdl(repo_path, master, reviewers, o).lines() {
            swaps.push_str("        ");
            swaps.push_str(line);
            swaps.push('\n');
        }
        swaps.push_str("    }\n");
        swaps.push_str("}\n");
    }
    let swaps_doc: kdl::KdlDocument = swaps
        .parse()
        .expect("clank-generated swap variants are valid KDL");
    let layout_node = doc
        .nodes_mut()
        .iter_mut()
        .find(|n| n.name().value() == "layout")
        .ok_or_else(|| anyhow::anyhow!("composed layout lost its `layout` root"))?;
    if let Some(children) = layout_node.children_mut() {
        for node in swaps_doc.nodes() {
            children.nodes_mut().push(node.clone());
        }
    }
    Ok(doc.to_string())
}

/// Parse the template, find the `clank_agents` marker node(s),
/// replace each with the agent group, serialize. Errors when the
/// template isn't valid KDL or has no marker — both name the
/// problem at the source (config validation / open time), never
/// at zellij launch.
fn substitute_marker(template: &str, agents_kdl: &str) -> anyhow::Result<String> {
    let mut doc: kdl::KdlDocument = template.parse().map_err(|e: kdl::KdlError| {
        anyhow::anyhow!("zellij layout template is not valid KDL: {e}")
    })?;
    let agents_doc: kdl::KdlDocument = agents_kdl
        .parse()
        .expect("clank-generated agent group is valid KDL");
    let replaced = substitute_in_doc(&mut doc, agents_doc.nodes());
    if replaced == 0 {
        anyhow::bail!(
            "zellij layout template has no `{AGENTS_MARKER}` marker node — clank doesn't \
             know where the agent panes go. Add a `{AGENTS_MARKER}` node where the panes \
             should be."
        );
    }
    Ok(doc.to_string())
}

/// Depth-first replace of every `clank_agents` node with clones
/// of `agents`. Returns how many markers were replaced.
fn substitute_in_doc(doc: &mut kdl::KdlDocument, agents: &[kdl::KdlNode]) -> usize {
    let mut replaced = 0;
    let nodes = doc.nodes_mut();
    let mut i = 0;
    while i < nodes.len() {
        if nodes[i].name().value() == AGENTS_MARKER {
            nodes.splice(i..=i, agents.iter().cloned());
            replaced += 1;
            i += agents.len();
            continue;
        }
        if let Some(children) = nodes[i].children_mut() {
            replaced += substitute_in_doc(children, agents);
        }
        i += 1;
    }
    replaced
}

/// Validate a user template early (config load / `clank doctor`):
/// parses as KDL and contains the marker. The substituted result
/// is valid by construction (tree substitution), so these two
/// checks are the whole contract (ruthless fb3f85f concern 3).
pub(crate) fn validate_template(template: &str) -> anyhow::Result<()> {
    substitute_marker(template, "pane\n").map(|_| ())
}

/// One agent pane. `cwd` is per-pane so the launched tool (e.g.
/// `claude --resume`, which doesn't take a path argument) runs in
/// the repo regardless of the shell that invoked
/// `zellij --layout`; `--repo` pins clank-side resolution (codex
/// 8075d43 + 361b104).
fn push_agent_pane(
    out: &mut String,
    indent: &str,
    extra_attrs: &str,
    label: &str,
    role_str: &str,
    repo_path: &str,
) {
    let label_esc = kdl_escape(label);
    let role_esc = kdl_escape(role_str);
    let repo_esc = kdl_escape(repo_path);
    out.push_str(&format!(
        "{indent}pane{extra_attrs} name=\"{label_esc} ({role_esc})\" cwd=\"{repo_esc}\" {{\n"
    ));
    out.push_str(&format!("{indent}    command \"clank\"\n"));
    out.push_str(&format!(
        "{indent}    args \"agent\" \"start\" \"{label_esc}\" \"--repo\" \"{repo_esc}\"\n"
    ));
    out.push_str(&format!("{indent}}}\n"));
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

/// The argv used to spawn zellij — also emitted on stderr in
/// `--print` mode so tests + tools can inspect without observing
/// the spawn.
///
/// Session serialization is disabled: clank sessions are cheap to
/// regenerate from the layout, and each serializing server polls
/// `ps -ao ppid,args` on an interval — accumulated detached
/// sessions congest the whole machine (observed ~320 servers
/// pinning every core with concurrent `ps` scans).
/// Deterministic per-repo session name (`zellij-session-dedup`):
/// zellij enforces NAME UNIQUENESS, so a stable name makes
/// duplicate clank sessions unrepresentable — re-running
/// `clank open zellij` attaches instead of minting another
/// randomly-named session full of fresh agent instances.
fn session_name(basename: &str) -> String {
    format!("clank-{basename}")
}

/// What to do about the named session, decided PURELY over
/// `zellij list-sessions -n` output (`-n` = no ANSI; each line is
/// `<name> [Created …]` with a trailing `(EXITED - …)` marker for
/// dead sessions and `(current)` for the one we're inside).
/// First-whitespace-token name match keeps the parse robust to
/// trailing-format drift; the EXITED marker is the only other
/// thing we read (ruthless a9ac348 concern 2).
///
/// Race note (concern 1, pinned by probe): zellij can't create
/// two sessions with one name, so if a session appears between
/// the list and the spawn, `--session` fails with a visible
/// "already exists" error — the race fails SAFE (an error, never
/// a silent duplicate).
#[derive(Debug, PartialEq, Clone, Copy)]
enum SessionPlan {
    Create,
    Attach,
    DeleteDeadThenCreate,
}

fn decide_session(list_output: &str, name: &str) -> SessionPlan {
    for line in list_output.lines() {
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        if first != name {
            continue;
        }
        if line.contains("EXITED") {
            // Serialization is off for clank sessions, so a dead
            // one can't resurrect meaningfully — clear and recreate.
            return SessionPlan::DeleteDeadThenCreate;
        }
        return SessionPlan::Attach;
    }
    SessionPlan::Create
}

/// Create a fresh named session with the layout. Session
/// serialization is disabled: clank sessions are cheap to
/// regenerate from the layout, and each serializing server polls
/// `ps -ao ppid,args` on an interval — accumulated detached
/// sessions congest the whole machine (observed ~320 servers
/// pinning every core with concurrent `ps` scans).
fn create_argv(layout_path: &Path, name: &str) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "--session".to_string(),
        name.to_string(),
        // MUST be `--new-session-with-layout`, NOT `--layout`:
        // alongside `--session`, plain `--layout` means "add this
        // layout as a tab to session <name>", which errors with
        // "There is no active session!" when the session doesn't
        // exist yet (the Create case). `--new-session-with-layout`
        // always starts a fresh session. (Verified live against
        // zellij 0.44.)
        "--new-session-with-layout".to_string(),
        layout_path.display().to_string(),
        "options".to_string(),
        "--session-serialization".to_string(),
        "false".to_string(),
    ]
}

fn attach_argv(name: &str) -> Vec<String> {
    vec!["zellij".to_string(), "attach".to_string(), name.to_string()]
}

fn delete_argv(name: &str) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "delete-session".to_string(),
        name.to_string(),
    ]
}

/// Inside an existing zellij session ($ZELLIJ set) the layout
/// opens as a new TAB in the current session — no --session flag
/// (zellij ignores/conflicts it in-session). NOTE: a re-run
/// INSIDE a clank session still adds a duplicate tab of agents —
/// within-session dedup is OUT OF SCOPE here (concern 3); the
/// per-agent pidfile guard is the future cover for that path.
fn in_session_tab_argv(layout_path: &Path) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "--layout".to_string(),
        layout_path.display().to_string(),
        "options".to_string(),
        "--session-serialization".to_string(),
        "false".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewers(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    const TEST_REPO: &str = "/tmp/test-repo";
    /// 200x50 cells — comfortably landscape (cols >= 2*rows).
    const LANDSCAPE: (u16, u16) = (200, 50);
    /// 80x60 cells — portrait (cols < 2*rows).
    const PORTRAIT: (u16, u16) = (80, 60);
    const TEST_TAB: &str = "test-repo";

    #[test]
    fn compose_kdl_includes_tab_bar_and_status_bar_plugins() {
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LANDSCAPE).unwrap();
        assert!(kdl.contains("plugin location=\"zellij:tab-bar\""));
        assert!(kdl.contains("plugin location=\"zellij:status-bar\""));
    }

    #[test]
    fn compose_kdl_wraps_panes_in_tab_block_with_name() {
        let kdl = compose_kdl("basename", TEST_REPO, "alice", &[], None, LANDSCAPE).unwrap();
        assert!(
            kdl.contains("tab name=\"basename\""),
            "KDL should wrap panes in a tab block with name; got:\n{kdl}"
        );
    }

    #[test]
    fn compose_kdl_master_and_reviewer_panes_use_clank_agent_start_with_repo() {
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob", "carol"]),
            None,
            LANDSCAPE,
        )
        .unwrap();
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
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob"]),
            None,
            LANDSCAPE,
        )
        .unwrap();
        assert!(
            kdl.contains(&format!("name=\"alice (master)\" cwd=\"{TEST_REPO}\"")),
            "master pane should set cwd; got:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!("name=\"bob (reviewer)\" cwd=\"{TEST_REPO}\"")),
            "reviewer pane should set cwd; got:\n{kdl}"
        );
    }

    #[test]
    fn compose_kdl_reviewer_order_matches_input_order() {
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "m",
            &reviewers(&["bob", "alice", "codex"]),
            None,
            LANDSCAPE,
        )
        .unwrap();
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, r#"weird"name"#, &[], None, LANDSCAPE).unwrap();
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
        let kdl = compose_kdl(
            r#"weird"tab"#,
            r#"/tmp/dir"with"quotes"#,
            "m",
            &[],
            None,
            LANDSCAPE,
        )
        .unwrap();
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
    fn layout_file_path_is_under_clank_zellij_dir() {
        let p = layout_file_path(Path::new("/tmp/repo"));
        assert_eq!(p, Path::new("/tmp/repo/.clank/zellij/layout.kdl"));
    }

    #[test]
    fn write_layout_file_writes_kdl_to_clank_zellij_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_layout_file(dir.path(), "layout { }\n").unwrap();
        assert_eq!(path, dir.path().join(".clank/zellij/layout.kdl"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "layout { }\n");
    }

    // ── template substitution (zellij-layout-config-around-agent-panes) ──

    #[test]
    fn built_in_layout_uses_default_tab_template_and_parses() {
        // Ruthless fb3f85f concern 2 option (b): the zero-config
        // built-in now uses default_tab_template, so bars apply to
        // runtime-spawned tabs too (the plan's problem #2, fixed
        // for everyone, not just template authors).
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LANDSCAPE).unwrap();
        assert!(kdl.contains("default_tab_template"), "got:\n{kdl}");
        let _: kdl::KdlDocument = kdl.parse().expect("built-in output is valid KDL");
        assert!(
            !kdl.contains(AGENTS_MARKER),
            "marker must be substituted away; got:\n{kdl}"
        );
    }

    #[test]
    fn user_template_chrome_preserved_and_marker_substituted() {
        // The documented example: user-authored chrome (compact-bar
        // + a `clank status --tui` pane) around the marker.
        let template = r##"layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="compact-bar"
        }
        children
    }
    tab name="my-clank" {
        clank_agents
        pane size=8 {
            command "clank"
            args "status" "--tui"
        }
    }
}
"##;
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob"]),
            Some(template),
            LANDSCAPE,
        )
        .unwrap();
        // Chrome preserved verbatim.
        assert!(
            kdl.contains("plugin location=\"compact-bar\""),
            "got:\n{kdl}"
        );
        assert!(kdl.contains("tab name=\"my-clank\""));
        assert!(kdl.contains("\"status\" \"--tui\""));
        // The built-in bars are NOT injected — user owns the chrome.
        assert!(!kdl.contains("zellij:tab-bar"));
        // Marker replaced with the agent group.
        assert!(!kdl.contains(AGENTS_MARKER));
        assert!(kdl.contains("name=\"alice (master)\""));
        assert!(kdl.contains("name=\"bob (reviewer)\""));
        let _: kdl::KdlDocument = kdl.parse().expect("substituted output is valid KDL");
    }

    #[test]
    fn marker_found_in_nested_children() {
        let template = r##"layout {
    tab {
        pane split_direction="vertical" {
            clank_agents
        }
    }
}
"##;
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LANDSCAPE).unwrap();
        assert!(!kdl.contains(AGENTS_MARKER));
        assert!(kdl.contains("name=\"m (master)\""));
    }

    #[test]
    fn template_without_marker_errors_naming_it() {
        let template = "layout {\n    tab {\n        pane\n    }\n}\n";
        let err =
            compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LANDSCAPE).unwrap_err();
        assert!(
            err.to_string().contains("clank_agents"),
            "error must name the marker; got: {err}"
        );
    }

    #[test]
    fn template_with_invalid_kdl_errors_at_compose_not_zellij() {
        let template = "layout { tab { pane "; // unclosed
        let err =
            compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LANDSCAPE).unwrap_err();
        assert!(err.to_string().contains("not valid KDL"), "got: {err}");
    }

    #[test]
    fn marker_inside_comment_or_string_is_not_substituted() {
        // Ruthless fb3f85f concern 1: a NODE marker can't false-match
        // text. The comment + string mention the marker but the only
        // real node is in the tab.
        let template = r##"layout {
    // put clank_agents here someday
    tab name="clank_agents" {
        clank_agents
    }
}
"##;
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LANDSCAPE).unwrap();
        // The comment + the tab NAME survive untouched; the node is
        // replaced.
        assert!(kdl.contains("// put clank_agents here someday"));
        assert!(kdl.contains("tab name=\"clank_agents\""));
        assert!(kdl.contains("name=\"m (master)\""));
    }

    #[test]
    fn validate_template_checks_parse_and_marker() {
        assert!(validate_template("layout {\n    clank_agents\n}\n").is_ok());
        assert!(
            validate_template("layout {\n    pane\n}\n")
                .unwrap_err()
                .to_string()
                .contains("clank_agents")
        );
        assert!(
            validate_template("layout { pane ")
                .unwrap_err()
                .to_string()
                .contains("not valid KDL")
        );
    }

    // ── zellij-default-layout: stage / stack / tui + orientation ──

    #[test]
    fn landscape_stage_stack_and_pinned_tui_pane() {
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob", "carol"]),
            None,
            LANDSCAPE,
        )
        .unwrap();
        // Stage: master gets the big pane.
        assert!(
            kdl.contains("pane size=\"65%\" name=\"alice (master)\""),
            "master is the 65% stage:\n{kdl}"
        );
        // Reviewers stack (one visible, rest collapsed bars).
        assert!(kdl.contains("pane stacked=true"), "reviewer stack:\n{kdl}");
        // The instrument pane is repo-pinned BOTH ways (codex
        // 7d3b5d1): cwd AND --repo, against a repo that is NOT the
        // test process cwd.
        assert!(
            kdl.contains(&format!("name=\"status\" cwd=\"{TEST_REPO}\"")),
            "tui pane cwd pinned:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!(
                "args \"status\" \"--repo\" \"{TEST_REPO}\" \"--tui\""
            )),
            "tui pane --repo pinned:\n{kdl}"
        );
        let _: kdl::KdlDocument = kdl.parse().expect("valid KDL");
    }

    #[test]
    fn portrait_tui_pane_pinned_like_landscape() {
        // 5aec9b9 acceptance: the status pane's repo pin (cwd AND
        // --repo) holds in BOTH orientations, against a repo that
        // is not the process cwd.
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob"]),
            None,
            PORTRAIT,
        )
        .unwrap();
        assert!(
            kdl.contains(&format!("name=\"status\" cwd=\"{TEST_REPO}\"")),
            "portrait tui cwd pinned:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!(
                "args \"status\" \"--repo\" \"{TEST_REPO}\" \"--tui\""
            )),
            "portrait tui --repo pinned:\n{kdl}"
        );
        let _: kdl::KdlDocument = kdl.parse().expect("valid KDL");
    }

    #[test]
    fn orientation_detected_from_terminal_dims() {
        // Landscape dims → outer split is COLUMNS (vertical);
        // portrait dims → ROWS (horizontal). The base tab carries
        // the detected arrangement.
        let land = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LANDSCAPE).unwrap();
        let port = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, PORTRAIT).unwrap();
        let base_tab = |k: &str| {
            let i = k.find("tab name=").unwrap();
            let j = k.find("swap_tiled_layout").unwrap_or(k.len());
            k[i..j].to_string()
        };
        assert!(
            base_tab(&land).contains("pane split_direction=\"vertical\""),
            "landscape base = columns:\n{land}"
        );
        assert!(
            base_tab(&port).contains("pane split_direction=\"horizontal\""),
            "portrait base = rows:\n{port}"
        );
    }

    #[test]
    fn built_in_ships_both_swap_variants_user_templates_get_none() {
        let built_in = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LANDSCAPE).unwrap();
        assert!(
            built_in.contains("swap_tiled_layout name=\"landscape\"")
                && built_in.contains("swap_tiled_layout name=\"portrait\""),
            "both variants for alt+[ flipping:\n{built_in}"
        );
        let user = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "m",
            &[],
            Some("layout {\n    clank_agents\n}\n"),
            LANDSCAPE,
        )
        .unwrap();
        assert!(
            !user.contains("swap_tiled_layout"),
            "user templates own their swaps:\n{user}"
        );
    }

    #[test]
    fn zero_reviewers_skips_stack_keeps_tui() {
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LANDSCAPE).unwrap();
        assert!(
            !kdl.contains("stacked=true"),
            "no empty stack region:\n{kdl}"
        );
        assert!(
            kdl.contains("\"--tui\""),
            "instrument pane still ships:\n{kdl}"
        );
    }

    // ── session dedup (zellij-session-dedup) ──

    #[test]
    fn decide_session_three_branches() {
        // Canned `list-sessions -n` output, per the live probe:
        // `<name> [Created …]` + `(EXITED - attach to resurrect)`
        // for dead sessions, `(current)` for the one we're inside.
        let listing = "\
clank-clank [Created 31m 15s ago] (current)
other-repo [Created 2h ago]
dead-one [Created 10h ago] (EXITED - attach to resurrect)
";
        assert_eq!(
            decide_session(listing, "clank-clank"),
            SessionPlan::Attach,
            "live (even current) → attach"
        );
        assert_eq!(decide_session(listing, "other-repo"), SessionPlan::Attach);
        assert_eq!(
            decide_session(listing, "dead-one"),
            SessionPlan::DeleteDeadThenCreate,
            "EXITED → delete + recreate (serialization is off)"
        );
        assert_eq!(
            decide_session(listing, "clank-absent"),
            SessionPlan::Create,
            "absent → create"
        );
        assert_eq!(
            decide_session("", "anything"),
            SessionPlan::Create,
            "empty listing (zellij errors when no sessions) → create"
        );
    }

    #[test]
    fn decide_session_matches_whole_name_token_only() {
        // `clank-foo` must not match `clank-foobar` (first-token
        // equality, not prefix).
        let listing = "clank-foobar [Created 1m ago]\n";
        assert_eq!(decide_session(listing, "clank-foo"), SessionPlan::Create);
    }

    #[test]
    fn argv_shapes_for_each_plan() {
        let layout = Path::new("/r/.clank/zellij/layout.kdl");
        assert_eq!(
            create_argv(layout, "clank-r"),
            vec![
                "zellij",
                "--session",
                "clank-r",
                "--new-session-with-layout",
                "/r/.clank/zellij/layout.kdl",
                "options",
                "--session-serialization",
                "false",
            ]
        );
        assert_eq!(attach_argv("clank-r"), vec!["zellij", "attach", "clank-r"]);
        assert_eq!(
            delete_argv("clank-r"),
            vec!["zellij", "delete-session", "clank-r"]
        );
        // The in-session tab path carries NO --session flag: inside
        // zellij even an explicit --session is overridden into
        // new-tab behavior (probed live, the hard way — a stray
        // probe tabbed a duplicate agent set into the operator's
        // session). Within-session dedup is the pidfile guard's
        // job, out of scope here (concern 3).
        assert_eq!(
            in_session_tab_argv(layout),
            vec![
                "zellij",
                "--layout",
                "/r/.clank/zellij/layout.kdl",
                "options",
                "--session-serialization",
                "false",
            ]
        );
    }

    #[test]
    fn session_name_is_deterministic_per_repo() {
        assert_eq!(session_name("clank"), "clank-clank");
        assert_eq!(session_name("bindex-fun"), "clank-bindex-fun");
    }
}
