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
    let source = resolve_repo(args.repo.as_deref())?;
    let in_zellij = std::env::var_os("ZELLIJ").is_some();

    // --all = the pwd repo AND all its worktrees (`git worktree list`),
    // regardless of which session/worktree we're in.
    if args.all {
        let targets = worktree_paths(&source)?;
        if in_zellij {
            // Inside a session: reconcile — add each target's tab to the
            // CURRENT session, skipping any already open. `open_one`'s
            // in-session tab spawn is non-blocking, so the loop runs to
            // completion (unlike an outside attach/create per target,
            // which would block on the first — codex 03be1fb).
            for target in &targets {
                open_one(target, args.print)?;
            }
        } else {
            // Outside: reconcile the worktree tabs into the
            // `clank-<repo>` session (create it, or add the missing
            // tabs to a live one), then attach.
            open_all(&source, &targets, args.print)?;
        }
        return Ok(());
    }

    // Single target: --fork/--pr name an existing worktree, bare = the
    // source repo. `open_one` works inside (tab) or outside (session).
    let target = if let Some(name) = args.fork.as_deref() {
        fork_path(&source, name)?
    } else if let Some(pr) = args.pr {
        fork_path(&source, &format!("pr-{pr}"))?
    } else {
        source
    };
    open_one(&target, args.print)
}

/// Resolve an EXISTING fork worktree `<source>/.clank/worktrees/<name>`,
/// erroring if it's absent — `open` opens, `clank fork` creates
/// (open-and-fork-idempotent).
fn fork_path(source: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let p = source.join(".clank/worktrees").join(name);
    if !p.is_dir() {
        anyhow::bail!(
            "no fork `{name}` at `{}` — create it with `clank fork {name}`",
            p.display()
        );
    }
    Ok(p)
}

/// Every worktree of `repo`'s repository — the main checkout PLUS every
/// linked worktree — via `git worktree list --porcelain`. This is the
/// `--all` target set: the repo itself and all its forks.
fn worktree_paths(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("running `git worktree list`")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git worktree list` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_worktree_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git worktree list --porcelain` stdout → worktree paths
/// (the `worktree <path>` lines, in listed order: main first).
fn parse_worktree_list(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// One tab in a multi-tab `--all` layout: a worktree's name + the
/// agent panes to spawn for it.
struct TabSpec {
    name: String,
    repo_path: String,
    master: String,
    reviewers: Vec<String>,
}

/// Open tab names in the (possibly detached) session `name`, or empty
/// when the session doesn't exist / can't be queried. Unlike the
/// in-session [`zellij_tab_names`], this DEGRADES to empty: for `--all`
/// an absent session is the normal "create it fresh" case, not an
/// error. (`zellij -s <name> action` targets a named session even when
/// we're not attached to it.)
fn session_tab_names(name: &str) -> Vec<String> {
    std::process::Command::new("zellij")
        .args(["-s", name, "action", "query-tab-names"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The target basenames whose tab isn't already open in the session —
/// the `--all` reconcile (open-and-fork-idempotent). Empty ⇒ every tab
/// is already there (attach, don't re-add). Pure / testable.
fn missing_tabs(target_basenames: &[String], open_tabs: &[String]) -> Vec<String> {
    target_basenames
        .iter()
        .filter(|b| !tab_is_open(b, open_tabs))
        .cloned()
        .collect()
}

/// What `--all` outside a session should do. The argv split mirrors
/// [`decide_session`]: an absent or dead `clank-<repo>` session can't
/// take the add-tabs `--layout` (it errors "no active session" — see
/// [`create_argv`]), so it is (re)created fresh with EVERY target tab;
/// only a LIVE session takes the add-tabs argv, and then only for the
/// tabs not already open (codex e33389b). Pure / testable.
#[derive(Clone, PartialEq, Debug)]
enum AllPlan {
    /// (Re)create a fresh session with every target tab. `delete_first`
    /// clears a dead same-named session first.
    CreateFresh { delete_first: bool },
    /// Live session: add these (currently missing) tabs.
    AddTabs(Vec<String>),
    /// Live session already has every target tab: just attach.
    Attach,
}

fn plan_all(session: SessionPlan, target_basenames: &[String], open_tabs: &[String]) -> AllPlan {
    match session {
        SessionPlan::Create => AllPlan::CreateFresh {
            delete_first: false,
        },
        SessionPlan::DeleteDeadThenCreate => AllPlan::CreateFresh { delete_first: true },
        SessionPlan::Attach => {
            let missing = missing_tabs(target_basenames, open_tabs);
            if missing.is_empty() {
                AllPlan::Attach
            } else {
                AllPlan::AddTabs(missing)
            }
        }
    }
}

/// `--all` outside a session: ensure the `clank-<repo>` session has a
/// tab for every target worktree, then drop the operator in. Routes
/// through [`decide_session`] — an absent/dead session is (re)created
/// fresh with all tabs; a LIVE one gets ONLY its missing tabs added via
/// the add-tabs `--layout` argv (a plain `attach` would ignore the
/// layout, leaving a one-tab session short — codex a0c53c9); a live
/// session that already has every tab is just attached. Teamless
/// worktrees are skipped with a notice.
fn open_all(source: &Path, targets: &[PathBuf], print: bool) -> anyhow::Result<()> {
    let name = session_name(&repo_basename(source)?);
    let basenames: Vec<String> = targets
        .iter()
        .map(|t| repo_basename(t))
        .collect::<anyhow::Result<_>>()?;
    let session = decide_session(&zellij_list_sessions(), &name);
    // Only a live session needs a tab query to compute what's missing.
    let open_tabs = match session {
        SessionPlan::Attach => session_tab_names(&name),
        _ => Vec::new(),
    };
    let plan = plan_all(session, &basenames, &open_tabs);

    // Attach-only: every tab is already there, no layout to write.
    if plan == AllPlan::Attach {
        let argv = attach_argv(&name);
        if print {
            eprintln!("spawn: {}", argv.join(" "));
        } else {
            eprintln!("session `{name}` already has every worktree tab; attaching");
            spawn_zellij(&argv)?;
        }
        return Ok(());
    }

    // Layout targets: every target for a fresh session, only the
    // missing ones when adding to a live session.
    let include: Option<&[String]> = match &plan {
        AllPlan::AddTabs(missing) => Some(missing),
        _ => None,
    };
    let tabs = resolve_tabs(targets, include)?;
    if tabs.is_empty() {
        anyhow::bail!("no openable worktrees (none have clank agents configured)");
    }

    let (rows, cols) = crate::cli::status_tui::term_size();
    let kdl = compose_multitab(&tabs, (cols, rows))?;
    let layout_path = layout_file_path(source);
    let (pre, spawn) = match plan {
        AllPlan::CreateFresh { delete_first } => (
            delete_first.then(|| delete_argv(&name)),
            create_argv(&layout_path, &name),
        ),
        AllPlan::AddTabs(_) => (None, add_tabs_argv(&layout_path, &name)),
        AllPlan::Attach => unreachable!("handled above"),
    };

    if print {
        println!("{kdl}");
        if let Some(pre) = &pre {
            eprintln!("pre-spawn: {}", pre.join(" "));
        }
        eprintln!("spawn: {}", spawn.join(" "));
        return Ok(());
    }
    write_layout_file(source, &kdl)?;
    crate::init_facts::ensure_clank_gitignore_entry(source, "/zellij/")
        .context("ensuring /zellij/ gitignore entry")?;
    if let Some(pre) = &pre {
        let _ = std::process::Command::new(&pre[0]).args(&pre[1..]).status();
    }
    spawn_zellij(&spawn)
}

/// Resolve target worktrees to tab specs, skipping teamless ones with a
/// notice. `include = Some(names)` restricts to those basenames (the
/// add-tabs case); `None` includes every target (a fresh session).
fn resolve_tabs(targets: &[PathBuf], include: Option<&[String]>) -> anyhow::Result<Vec<TabSpec>> {
    let mut tabs = Vec::new();
    for t in targets {
        let basename = repo_basename(t)?;
        if include.is_some_and(|names| !names.contains(&basename)) {
            continue;
        }
        let Some(set) = crate::agent_store::try_resolve_via_team(t)? else {
            eprintln!("skipping `{}` (no agents configured)", t.display());
            continue;
        };
        let reviewers = set
            .commit_reviewers
            .iter()
            .chain(set.gate_reviewers.iter())
            .map(|a| a.label.as_str().to_string())
            .collect();
        tabs.push(TabSpec {
            name: basename,
            repo_path: t.display().to_string(),
            master: set.master.as_str().to_string(),
            reviewers,
        });
    }
    Ok(tabs)
}

/// Run a zellij spawn argv, mapping failure to a clear error.
fn spawn_zellij(argv: &[String]) -> anyhow::Result<()> {
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("spawning zellij")?;
    if !status.success() {
        anyhow::bail!("zellij exited {status}");
    }
    Ok(())
}

/// Compose a multi-tab layout: the shared `default_tab_template` plus
/// one `tab name="<worktree>" { <agent panes> }` per target. Unlike the
/// single-tab `compose_kdl` this drops the alt-[/] swap variants —
/// they're a layout-root construct that can't represent N tabs of
/// different agents — and uses the built-in tab structure (no per-tab
/// user template). The result is parse-validated as KDL.
fn compose_multitab(tabs: &[TabSpec], term: (u16, u16)) -> anyhow::Result<String> {
    let orientation = Orientation::detect(term);
    let mut s = String::from(
        "layout {\n    \
         default_tab_template {\n        \
         pane size=1 borderless=true {\n            \
         plugin location=\"zellij:tab-bar\"\n        }\n        \
         children\n        \
         pane size=2 borderless=true {\n            \
         plugin location=\"zellij:status-bar\"\n        }\n    }\n",
    );
    for tab in tabs {
        s.push_str(&format!("    tab name=\"{}\" {{\n", kdl_escape(&tab.name)));
        for line in
            agent_group_kdl(&tab.repo_path, &tab.master, &tab.reviewers, orientation).lines()
        {
            if line.is_empty() {
                s.push('\n');
            } else {
                s.push_str("        ");
                s.push_str(line);
                s.push('\n');
            }
        }
        s.push_str("    }\n");
    }
    s.push_str("}\n");
    // Fail loudly if we somehow produced invalid KDL rather than handing
    // zellij a broken layout.
    s.parse::<kdl::KdlDocument>()
        .map_err(|e| anyhow::anyhow!("composed multi-tab layout is invalid KDL: {e}"))?;
    Ok(s)
}

/// Open (or, inside a session, idempotently ensure) the agent
/// workspace for one repo/worktree.
fn open_one(repo: &Path, print: bool) -> anyhow::Result<()> {
    let repo = repo.to_path_buf();
    let basename = repo_basename(&repo)?;
    // Registration is the resolved roster
    // (`teams-based-agent-registration`): exactly one master plus
    // its reviewers, no role-triage needed.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no agents configured. Run `clank agent add <name>` + \
             `clank agent set-master <name>` to build a roster, or `clank init --team <name>` \
             to seed one from a template."
        );
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

    if print {
        println!("{kdl}");
        if let Some(pre) = &pre_argv {
            eprintln!("pre-spawn: {}", pre.join(" "));
        }
        eprintln!("spawn: {}", spawn_argv.join(" "));
        return Ok(());
    }

    // Idempotent open (open-and-fork-idempotent): inside a session,
    // don't spawn a second tab for a worktree that already has one.
    // The tab name is the repo basename; `tab_is_open` matches it
    // glyph-stripped against the live tab list. The `?` only runs when
    // `$ZELLIJ` is set (short-circuit), so a failure to query is a
    // real in-session misconfiguration and surfaces as an error.
    if std::env::var_os("ZELLIJ").is_some() && tab_is_open(&basename, &zellij_tab_names()?) {
        eprintln!("tab `{basename}` already open");
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

/// `zellij action query-tab-names` → one open tab name per line.
///
/// ERRORS rather than degrading: this is only ever called when we're
/// already INSIDE a session (`$ZELLIJ` set), so `zellij` must be
/// runnable — if it isn't, silently returning "no tabs" would make the
/// dedup reconcile (open-and-fork-idempotent) misbehave (always "not
/// open" → double-open), masking a real misconfiguration. The caller
/// propagates with `?` so the operator sees a clear message. (Contrast
/// `zellij_list_sessions`, which degrades on purpose: outside a session
/// a non-zero exit legitimately means "no sessions".)
fn zellij_tab_names() -> anyhow::Result<Vec<String>> {
    let out = std::process::Command::new("zellij")
        .args(["action", "query-tab-names"])
        .output()
        .context("running `zellij action query-tab-names` (is zellij on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "`zellij action query-tab-names` exited {} — can't reconcile open tabs",
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// True if a tab for worktree `tab` (its basename / fork name) is
/// already open. zellij reports tab names with a possible leading
/// status glyph (tui-tab-mirror-bar-emoji), so compare glyph-stripped.
fn tab_is_open(tab: &str, open_names: &[String]) -> bool {
    open_names
        .iter()
        .any(|n| crate::cli::status_tui::strip_leading_emoji(n) == tab)
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
/// The pane name for an agent: `"<label> (<role>)"`. The SINGLE
/// source of this format — the zellij layout names panes with it here,
/// and `status --tui` matches it to map panes back to agents
/// (tui-agent-pane-status-emoji). A round-trip test pins the two
/// together so the format can't drift silently.
pub(crate) fn agent_pane_title(label: &str, role_str: &str) -> String {
    format!("{label} ({role_str})")
}

fn push_agent_pane(
    out: &mut String,
    indent: &str,
    extra_attrs: &str,
    label: &str,
    role_str: &str,
    repo_path: &str,
) {
    let name_esc = kdl_escape(&agent_pane_title(label, role_str));
    let repo_esc = kdl_escape(repo_path);
    out.push_str(&format!(
        "{indent}pane{extra_attrs} name=\"{name_esc}\" cwd=\"{repo_esc}\" {{\n"
    ));
    out.push_str(&format!("{indent}    command \"clank\"\n"));
    let args_kdl = agent_start_argv(label, repo_path)
        .iter()
        .map(|tok| format!("\"{}\"", kdl_escape(tok)))
        .collect::<Vec<_>>()
        .join(" ");
    out.push_str(&format!("{indent}    args {args_kdl}\n"));
    out.push_str(&format!("{indent}}}\n"));
}

/// The argv (after `clank`) that launches an agent's pane: `agent start
/// <label> --repo <repo>`. The single source of truth for the launch-time
/// KDL layout ([`push_agent_pane`]) AND the live `new-pane` path — and
/// the exact `terminal_command` zellij reports for that pane, which is how
/// the live add/remove path finds it.
pub(crate) fn agent_start_argv(label: &str, repo_path: &str) -> Vec<String> {
    vec![
        "agent".to_string(),
        "start".to_string(),
        label.to_string(),
        "--repo".to_string(),
        repo_path.to_string(),
    ]
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

/// Add the layout's tabs to the LIVE session `name`. Alongside
/// `--session`, plain `--layout` means "add as tab(s)" — the inverse of
/// [`create_argv`]'s `--new-session-with-layout`. It errors when the
/// session isn't live, so callers MUST gate on a live session (the
/// `--all` Attach branch).
fn add_tabs_argv(layout_path: &Path, name: &str) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "--session".to_string(),
        name.to_string(),
        "--layout".to_string(),
        layout_path.display().to_string(),
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
    fn agent_start_argv_is_the_pane_launch_invocation() {
        assert_eq!(
            agent_start_argv("bob", "/repo"),
            vec!["agent", "start", "bob", "--repo", "/repo"]
        );
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
    fn tab_is_open_matches_glyph_stripped() {
        // zellij reports tab names with a possible leading status glyph
        // (tui-tab-mirror-bar-emoji); the reconcile must match the
        // worktree/fork name glyph-stripped (open-and-fork-idempotent).
        let open = vec![
            "🔨 clank".to_string(),
            "👀 device-prompt-animations".to_string(),
            "status".to_string(),
        ];
        assert!(tab_is_open("clank", &open));
        assert!(tab_is_open("device-prompt-animations", &open));
        // No glyph → exact match still works.
        assert!(tab_is_open("status", &open));
        // A worktree with no open tab is not "open".
        assert!(!tab_is_open("frostsnap", &open));
        assert!(!tab_is_open("clan", &open), "no prefix match");
    }

    #[test]
    fn plan_all_routes_by_session_state() {
        let targets = vec![
            "clank".to_string(),
            "fork-a".to_string(),
            "fork-b".to_string(),
        ];
        // Absent session: can't take `--layout` (errors "no active
        // session"), so create fresh with every tab.
        assert_eq!(
            plan_all(SessionPlan::Create, &targets, &[]),
            AllPlan::CreateFresh {
                delete_first: false
            }
        );
        // Dead session: delete it, then create fresh with every tab.
        assert_eq!(
            plan_all(SessionPlan::DeleteDeadThenCreate, &targets, &[]),
            AllPlan::CreateFresh { delete_first: true }
        );
        // Live session missing a tab (codex a0c53c9): add ONLY the
        // missing one — open tabs carry status glyphs, so match stripped.
        let open = vec!["🔨 clank".to_string(), "👀 fork-a".to_string()];
        assert_eq!(
            plan_all(SessionPlan::Attach, &targets, &open),
            AllPlan::AddTabs(vec!["fork-b".to_string()])
        );
        // Live session already has every tab → attach, add nothing.
        let all_open = vec![
            "clank".to_string(),
            "fork-a".to_string(),
            "fork-b".to_string(),
        ];
        assert_eq!(
            plan_all(SessionPlan::Attach, &targets, &all_open),
            AllPlan::Attach
        );
    }

    #[test]
    fn fork_path_errors_when_absent_resolves_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        // Absent → error pointing at `clank fork`.
        let err = fork_path(src, "ghost").unwrap_err().to_string();
        assert!(err.contains("ghost") && err.contains("clank fork"), "{err}");
        // Present → the worktree path.
        std::fs::create_dir_all(src.join(".clank/worktrees/foo")).unwrap();
        assert_eq!(
            fork_path(src, "foo").unwrap(),
            src.join(".clank/worktrees/foo")
        );
    }

    #[test]
    fn parse_worktree_list_extracts_paths_main_first() {
        let porcelain = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
             worktree /repo/.clank/worktrees/foo\nHEAD def\nbranch refs/heads/foo\n";
        assert_eq!(
            parse_worktree_list(porcelain),
            vec![
                PathBuf::from("/repo"),
                PathBuf::from("/repo/.clank/worktrees/foo"),
            ]
        );
    }

    #[test]
    fn compose_multitab_has_a_tab_per_target_and_is_valid_kdl() {
        let tabs = vec![
            TabSpec {
                name: "main".into(),
                repo_path: "/repo".into(),
                master: "alice".into(),
                reviewers: vec!["bob".into()],
            },
            TabSpec {
                name: "foo".into(),
                repo_path: "/repo/.clank/worktrees/foo".into(),
                master: "alice".into(),
                reviewers: vec![],
            },
        ];
        let kdl = compose_multitab(&tabs, LANDSCAPE).unwrap();
        assert!(kdl.contains("tab name=\"main\""));
        assert!(kdl.contains("tab name=\"foo\""));
        // Each tab pins its OWN --repo.
        assert!(kdl.contains("\"--repo\" \"/repo\""));
        assert!(kdl.contains("\"--repo\" \"/repo/.clank/worktrees/foo\""));
        // One shared template, no per-orientation swaps for multi-tab.
        assert_eq!(kdl.matches("default_tab_template").count(), 1);
        assert!(!kdl.contains("swap_tiled_layout"));
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
        // Add-tabs (live session): plain `--layout`, NOT
        // `--new-session-with-layout` — adds the layout's tabs to the
        // running session rather than starting a new one.
        assert_eq!(
            add_tabs_argv(layout, "clank-r"),
            vec![
                "zellij",
                "--session",
                "clank-r",
                "--layout",
                "/r/.clank/zellij/layout.kdl",
            ]
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
