//! `clank open zellij` — auto-generate a zellij layout (KDL)
//! at `<repo>/.clank/zellij/layout.kdl` and spawn
//! `zellij --layout <path>`. The pane commands pin
//! `--repo <abs-path>` so the spawned session's cwd doesn't
//! affect the resolved repo (codex 361b104 catch).

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::OpenZellijArgs;
use super::{repo_basename, resolve_repo};

/// Whether this process runs inside a live zellij session.
///
/// The ONLY reader of `$ZELLIJ`. Everything else asks this, so "am I
/// in zellij" has one definition and the ownership gate can forbid
/// the raw read everywhere else (zellij-is-the-workspace).
pub(crate) fn in_session() -> bool {
    std::env::var_os("ZELLIJ").is_some()
}

pub async fn run(args: OpenZellijArgs) -> anyhow::Result<()> {
    // `--print` composes and prints; it never needs the binary, and a
    // test that inspects the argv must not require zellij installed.
    if !args.print {
        require_zellij()?;
    }
    let source = resolve_repo(args.repo.as_deref())?;
    let in_zellij = in_session();

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

/// Resolve an EXISTING fork by name through THE shared model
/// ([`crate::cli::fork::resolve_fork`]): a registered worktree on
/// branch `<name>` or a descriptor-validated clone — one name
/// namespace, so at most one exists. Errors if absent: `open` opens,
/// `clank fork` creates (open-and-fork-idempotent).
fn fork_path(source: &Path, name: &str) -> anyhow::Result<PathBuf> {
    match crate::cli::fork::resolve_fork(source, name)? {
        Some((_, p)) => Ok(p),
        None => anyhow::bail!("no fork `{name}` — create it with `clank fork create {name}`"),
    }
}

/// Every worktree of `repo`'s repository — the main checkout PLUS every
/// linked worktree — via `git worktree list --porcelain`, PLUS every
/// descriptor-VALIDATED clone fork from the shared model
/// ([`crate::cli::fork::clone_fork_paths`]; git's worktree registry
/// structurally cannot see clones). This is the `--all` target set:
/// the repo itself and all its forks.
fn worktree_paths(repo: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let stdout = crate::git_plumbing::worktree_list_porcelain(repo)?;
    let mut targets = parse_worktree_list(&stdout);
    targets.extend(crate::cli::fork::clone_fork_paths(repo));
    Ok(targets)
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

    let (rows, cols) = layout_term_size();
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
            .reviewers
            .iter()
            .map(|r| r.label.as_str().to_string())
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
///
/// zellij's own stderr is INHERITED, so whatever it complained about
/// is already on the terminal above this message. The job here is to
/// say what clank was doing and what to try — not to repeat an exit
/// code the user can see.
fn spawn_zellij(argv: &[String]) -> anyhow::Result<()> {
    let what = spawn_kind(argv);
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| {
            format!(
                "could not start zellij to {what}. `clank doctor` reports whether \
                 zellij is installed and reachable."
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "zellij failed to {what} (exit {status}) — its own error is printed \
             above. If the session exists but its server is dead, \
             `zellij delete-session <name>` and re-run `clank open`."
        );
    }
    Ok(())
}

/// What a spawn argv is FOR, in the user's terms, for error messages.
fn spawn_kind(argv: &[String]) -> &'static str {
    match argv.get(1).map(String::as_str) {
        Some("attach") => "attach to the repo's session",
        Some("action") => "add a tab to the current session",
        Some("--layout") | Some("-l") => "start the repo's session",
        _ => "open the workspace",
    }
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
        for line in agent_group_kdl(
            &tab.repo_path,
            &tab.master,
            ReviewerStack::Roster(&tab.reviewers),
            orientation,
        )
        .lines()
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
             `clank agent promote <name>` to build a roster, or `clank init --team <name>` \
             to seed one from a template."
        );
    };
    let master_label = set.master.as_str().to_string();
    let reviewer_labels: Vec<String> = set
        .reviewers
        .iter()
        .map(|r| r.label.as_str().to_string())
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
    // Orientation from the real WINDOW's dimensions — the client
    // tty when inside a session, the spawning terminal otherwise
    // (zellij-default-layout, zellij-in-session-orientation);
    // measurement stays at the shell, compose stays pure over
    // (cols, rows).
    let (rows, cols) = layout_term_size();
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
    let (pre_argv, spawn_argv) = if in_session() {
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
    if in_session() && tab_is_open(&basename, &zellij_tab_names()?) {
        eprintln!("tab `{basename}` already open");
        return Ok(());
    }

    write_layout_file(&repo, &kdl)?;

    if let Some(pre) = &pre_argv {
        // Best-effort: a failed delete of a dead session just means
        // the create below errors visibly.
        let _ = std::process::Command::new(&pre[0]).args(&pre[1..]).status();
    }
    if spawn_argv[1] == "attach" {
        eprintln!("attaching to existing session `{name}`");
    }
    spawn_zellij(&spawn_argv)
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
        .context("could not run `zellij action query-tab-names`")?;
    // By here the binary is proven present, so a failure is the
    // SESSION: `$ZELLIJ` is set but the server behind it does not
    // answer — a dead or stale session, not a missing install.
    if !out.status.success() {
        anyhow::bail!(
            "the zellij session this shell belongs to did not answer \
             (`query-tab-names` exited {}). `$ZELLIJ` is set but the server \
             behind it may be dead; open a fresh terminal and re-run `clank open`.",
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
    let agents = agent_group_kdl(
        repo_path,
        master,
        ReviewerStack::Roster(reviewers),
        orientation,
    );
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
            add_swap_variants(&base, repo_path, master)
        }
    }
}

/// (rows, cols) for layout-orientation decisions. Outside zellij:
/// the stdout ioctl (the real terminal). INSIDE zellij, stdout is the
/// invoking PANE's PTY — a wide shell split reads as landscape no
/// matter how portrait the window is — so measure the attached zellij
/// CLIENT's controlling tty instead, falling back to the pane ioctl
/// when no client is identifiable. Never worse than the pane
/// measurement (zellij-in-session-orientation).
fn layout_term_size() -> (u16, u16) {
    if in_session()
        && let Some(size) = zellij_client_window_size()
    {
        return size;
    }
    crate::cli::term::term_size()
}

/// Measure the real window from inside a session: find the zellij
/// client process for `$ZELLIJ_SESSION_NAME` in `ps` output and read
/// its controlling tty's winsize. Subprocess justified: zellij's CLI
/// exposes no window geometry (`list-panes`/`list-tabs` carry none),
/// and escape-sequence size queries are answered with PANE dimensions
/// — the very trap this exists to avoid.
fn zellij_client_window_size() -> Option<(u16, u16)> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "tty=,command="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let session = std::env::var("ZELLIJ_SESSION_NAME").ok();
    let tty = pick_zellij_client_tty(&text, session.as_deref())?;
    crate::cli::term::winsize_of_tty(&format!("/dev/{tty}"))
}

/// Pure picker over `ps -axo tty=,command=` lines: the tty of the
/// zellij CLIENT attached to `session`. A client line is one whose
/// program is zellij itself (basename match, so absolute paths count)
/// on a real tty, excluding transient `zellij action`/`setup`/`ls`
/// invocations. The session name must appear as a standalone argv
/// token (`attach <name>`, `--session <name>`, `-s <name>` all
/// satisfy this; a layout PATH merely containing the name does not —
/// paths are single slash-joined tokens). With no named match a SOLE
/// client is unambiguous — use it; several → `None` (two windows can
/// disagree, so fall back to the deterministic pane ioctl). First
/// named match wins ties: arbitrary but deterministic.
fn pick_zellij_client_tty(ps_output: &str, session: Option<&str>) -> Option<String> {
    let mut clients: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in ps_output.lines() {
        let mut parts = line.split_whitespace();
        let Some(tty) = parts.next() else { continue };
        if tty == "??" || tty == "?" {
            continue; // daemonized zellij (e.g. the server) — no terminal
        }
        let argv: Vec<&str> = parts.collect();
        let Some(prog) = argv.first() else { continue };
        if prog.rsplit('/').next().unwrap_or(prog) != "zellij" {
            continue;
        }
        if argv
            .iter()
            .any(|a| matches!(*a, "action" | "setup" | "ls" | "list-sessions"))
        {
            continue;
        }
        clients.push((tty, argv));
    }
    if let Some(name) = session
        && let Some((tty, _)) = clients.iter().find(|(_, argv)| argv.contains(&name))
    {
        return Some((*tty).to_string());
    }
    match clients.as_slice() {
        [(tty, _)] => Some((*tty).to_string()),
        _ => None,
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

/// What the reviewer region of a pane group holds.
///
/// A tab's own layout LAUNCHES panes, so it names each reviewer. A
/// swap variant is applied to panes that already exist, however many:
/// zellij puts every pane without a command-matched slot at the
/// layout's `children` node. A variant that named the opening roster
/// had no such node, and a reviewer added later fell through to a
/// plain split of the stage on every alt+[ / alt+]
/// (a-swap-layout-describes-a-shape-not-a-roster).
#[derive(Clone, Copy)]
enum ReviewerStack<'a> {
    Roster(&'a [String]),
    AnyPresent,
}

/// The agent pane group clank owns: master is the STAGE (~65%),
/// reviewers STACK in the smaller region, and a `clank status
/// --tui` instrument pane sits beside/below the stack
/// (zellij-default-layout). Landscape: master left, right column
/// = stack over tui. Portrait: master top, bottom row = stack
/// beside tui. An empty roster: no stack region — stage + tui.
///
/// zellij KDL: `split_direction="vertical"` lays children out as
/// COLUMNS, `"horizontal"` as ROWS.
fn agent_group_kdl(
    repo_path: &str,
    master: &str,
    stack: ReviewerStack<'_>,
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
    match stack {
        ReviewerStack::Roster(reviewers) if reviewers.is_empty() => {}
        ReviewerStack::Roster(reviewers) => {
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
        ReviewerStack::AnyPresent => {
            out.push_str("        pane stacked=true {\n");
            out.push_str("            children\n");
            out.push_str("        }\n");
        }
    }
    // The instrument pane: repo pinned via cwd AND --repo (codex
    // 7d3b5d1 — the 361b104/8075d43 lineage applies to every
    // generated command, not just agent panes). PERCENTAGE in both
    // orientations: zellij treats an absolute layout size as a FIXED
    // pane and refuses interactive resizes
    // (zellij-status-pane-resizable) — and 10 rows was too short
    // anyway. The TUI still degrades gracefully (1-row bar
    // invariant) if the user squeezes it.
    let tui_size = match orientation {
        Orientation::Landscape => "size=\"30%\"",
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
///
/// The variants take no roster: their stack is a `children` slot, so
/// they fit the reviewers present at swap time, not the ones present
/// at open. They carry that slot even when the roster opened empty —
/// the tab layout has nothing to launch then, but the first reviewer
/// added later needs somewhere to land.
fn add_swap_variants(base: &str, repo_path: &str, master: &str) -> anyhow::Result<String> {
    let mut doc: kdl::KdlDocument = base.parse().expect("composed built-in layout is valid KDL");
    let mut swaps = String::new();
    for o in [Orientation::Landscape, Orientation::Portrait] {
        swaps.push_str(&format!("swap_tiled_layout name=\"{}\" {{\n", o.name()));
        swaps.push_str("    tab {\n");
        for line in agent_group_kdl(repo_path, master, ReviewerStack::AnyPresent, o).lines() {
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

/// The command string zellij reports as a pane's `terminal_command` when
/// launched from [`agent_start_argv`]: `clank agent start <label> --repo
/// <repo>`. This is an agent pane's UNIQUE identity within a session —
/// the bare label repeats across worktree tabs, but the absolute
/// `--repo` path disambiguates them.
pub(crate) fn agent_start_command(label: &str, repo_path: &str) -> String {
    format!("clank {}", agent_start_argv(label, repo_path).join(" "))
}

/// Subset of a `zellij action list-panes --json --command` element.
/// Extra fields are ignored (serde skips unknown keys by default).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ZellijPane {
    id: u32,
    #[serde(default)]
    is_plugin: bool,
    #[serde(default)]
    terminal_command: Option<String>,
    /// Pane title from `list-panes --json` (verified live: present,
    /// with any status-emoji prefix). Carries the ROLE the pane was
    /// titled with (`… (master)` / `… (reviewer)`), which the command
    /// string does not.
    #[serde(default)]
    title: String,
    /// Which tab the pane lives in — `list-panes --json` spans ALL tabs,
    /// so this scopes a relocation to the caller's active tab.
    #[serde(default)]
    tab_id: u32,
    /// Focused WITHIN its tab. Every tab reports one, so this alone
    /// cannot name the user's focus — paired with [`active_tab_id`] it
    /// can. Default false when absent, which costs a focus restore
    /// rather than a wrong one.
    #[serde(default)]
    is_focused: bool,
    /// The pane's process has ended (zellij holds the pane open with
    /// its exit code). Deliberately NOT part of identifying a pane —
    /// see [`agent_pane_label`] — and used only to choose WHICH copy
    /// to close in [`remove_target_ids`].
    #[serde(default)]
    exited: bool,
    /// Present only with `--tab`; empty otherwise.
    #[serde(default)]
    tab_name: String,
    /// Pane geometry (terminal cells) from `list-panes --json`: top-left
    /// position + size. Feeds [`tab_dims`] so orientation is read from the
    /// tab's true extent, not the caller's own pane. Default 0 when absent.
    #[serde(default)]
    pane_x: u16,
    #[serde(default)]
    pane_y: u16,
    #[serde(default)]
    pane_columns: u16,
    #[serde(default)]
    pane_rows: u16,
}

impl ZellijPane {
    /// The pane-id string zellij's `focus-pane-id` / `close-pane
    /// --pane-id` accept (`terminal_<id>` / `plugin_<id>`).
    pub(crate) fn pane_id(&self) -> String {
        let kind = if self.is_plugin { "plugin" } else { "terminal" };
        format!("{kind}_{}", self.id)
    }

    /// Whether a RUNNING process in this pane was launched with exactly
    /// `command`. `exited` matters here and nowhere else in identity: a
    /// pane zellij keeps open after its process ended still reports the
    /// command, and [`find_pane_by_command`] wants that (so a dead copy
    /// can be found and closed), but a dead pane holds no session
    /// (a-session-has-one-holder).
    pub(crate) fn runs(&self, command: &str) -> bool {
        !self.is_plugin && !self.exited && self.terminal_command.as_deref() == Some(command)
    }

    pub(crate) fn tab_name(&self) -> &str {
        &self.tab_name
    }
}

/// The terminal pane whose running command is exactly `cmd`, if any.
fn find_pane_by_command<'a>(panes: &'a [ZellijPane], cmd: &str) -> Option<&'a ZellijPane> {
    panes
        .iter()
        .find(|p| !p.is_plugin && p.terminal_command.as_deref() == Some(cmd))
}

/// The agent label a pane runs for `repo_path`, parsed from the exact
/// `clank agent start <label> --repo <path>` invocation
/// [`agent_start_command`] composes. `None` for any other command
/// shape or another repo's agents — the reconciler must only ever see
/// THIS repo's panes (tui-zellij-pane-reconcile).
///
/// EXIT STATE IS NOT CONSULTED, and that is the single rule every
/// reader shares — the idempotence guard in [`add_reviewer_pane`],
/// the multiplicity count in `plan_panes`, and the removals. A
/// crashed agent's pane is still that agent's pane, so the reconciler
/// leaves it standing rather than opening a replacement: respawn is
/// MANUAL, by design. Auto-respawn would relaunch a tool that just
/// failed, and for one that refuses a second session
/// (`already has an active writer`) it would relaunch it straight
/// back into that error on every pass.
///
/// The cost is accepted: a crashed agent blocks its own replacement
/// until someone acts. What must not follow is a corpse outliving a
/// live duplicate, which is why [`remove_target_ids`] closes exited
/// copies first.
fn agent_pane_label<'a>(pane: &'a ZellijPane, repo_path: &str) -> Option<&'a str> {
    if pane.is_plugin {
        return None;
    }
    // Strip the KNOWN repo suffix rather than splitting at the first
    // space: `AgentLabel` permits spaces (it forbids only empty,
    // dot-segments and `/`), so a label like `two words` produced the
    // exact command `clank agent start two words --repo /repo` and a
    // first-space split read it as `two` with a tail that never
    // matched. That agent was then permanently absent — and since
    // verification now reads this same parser, permanently absent
    // means reconciliation never converges (codex on 1fe53e0).
    //
    // Anchoring on both ends keeps what the split gave for free: a
    // different repo does not match, and a trailing extra argument
    // leaves the suffix un-terminal so it does not either.
    let rest = pane
        .terminal_command
        .as_deref()?
        .strip_prefix("clank agent start ")?;
    let label = rest.strip_suffix(&format!(" --repo {repo_path}"))?;
    (!label.is_empty()).then_some(label)
}

/// One `--json` pane listing for a whole reconcile pass
/// (zellij-one-listing-per-pass: this call is the measured ~1.1s unit,
/// so the worker takes it ONCE and threads it through the primitives).
/// `None` outside zellij or on failure.
pub(crate) fn snapshot_panes() -> Option<Vec<ZellijPane>> {
    if !in_session() {
        return None;
    }
    list_agent_panes()
}

/// Project a pane listing to this repo's agents as
/// `(label, master_titled)` — the label from the exact launch command,
/// the role from the pane TITLE (`… (master)` suffix; titles are
/// stamped by the layout and the promote relocation). Pure.
pub(crate) fn agent_pane_pairs(panes: &[ZellijPane], repo: &Path) -> Vec<(String, bool)> {
    let repo_str = repo.to_string_lossy();
    panes
        .iter()
        .filter_map(|p| {
            agent_pane_label(p, &repo_str).map(|l| (l.to_owned(), p.title.ends_with(" (master)")))
        })
        .collect()
}

/// Put focus back on `id` after a focus-stealing reconcile op.
///
/// Pass-level focus restore (zellij-one-listing-per-pass): the worker
/// captures once before its first action and restores once after the
/// last, replacing the old per-op capture/restore. See
/// [`pass_focus_target`] for how the target is derived.
pub(crate) fn focus_pane(id: &str) {
    zellij_action(&["focus-pane-id", id]);
}

/// The pass's focus-restore target: the pane the USER has focused,
/// falling back to the caller's own pane when it cannot be determined
/// — the fallback the old per-op restores had (codex 5d498d0).
///
/// Derived from the listing the pass ALREADY holds plus one
/// `current-tab-info`, never from `list-clients`. That action routes
/// through zellij's `populate_session_layout_metadata`, which shells
/// out to `ps -ao ppid,args` to label every pane's process: measured
/// on this machine at 1035-1041 ms (±3 ms over six runs) against 1071
/// ms for the bare `ps`, versus ~30 ms for `list-panes` and 34 ms for
/// `current-tab-info`. The cost tracks processes on the MACHINE, not
/// panes in the session, so it grows with every agent started
/// anywhere — for one integer.
pub(crate) fn pass_focus_target(panes: &[ZellijPane]) -> Option<String> {
    focus_target_from(focused_pane_in(panes, active_tab_id()), caller_pane_id())
}

fn focus_target_from(focused: Option<String>, caller: Option<String>) -> Option<String> {
    focused.or(caller)
}

/// The focused pane of `tab` — `None` without a tab, since every tab
/// reports a focused pane and picking one at random would move the
/// user's focus rather than restore it.
fn focused_pane_in(panes: &[ZellijPane], tab: Option<u32>) -> Option<String> {
    let tab = tab?;
    panes
        .iter()
        .find(|p| p.is_focused && p.tab_id == tab)
        .map(ZellijPane::pane_id)
}

/// The active tab's id, from `current-tab-info`'s `id:` line.
fn active_tab_id() -> Option<u32> {
    let out = zellij_action(&["current-tab-info"])?;
    parse_active_tab_id(&String::from_utf8_lossy(&out))
}

fn parse_active_tab_id(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|l| l.strip_prefix("id:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The first terminal pane running any of `cmds` — used to anchor a new
/// reviewer pane onto the EXISTING reviewer stack (focus a current stack
/// member, then `new-pane --stacked` joins that stack).
fn find_anchor_pane<'a>(panes: &'a [ZellijPane], cmds: &[String]) -> Option<&'a ZellijPane> {
    panes.iter().find(|p| {
        !p.is_plugin
            && p.terminal_command
                .as_deref()
                .is_some_and(|c| cmds.iter().any(|x| x == c))
    })
}

/// A reviewer pane in `tab`, identified by the title zellij reports
/// rather than by the command it was launched with.
///
/// ANCHORS placement only — never classification.
/// `zellij-pane-placement-and-cost` rejected titles for deciding WHO a
/// pane is: a title carries no ownership marker and cannot round-trip
/// the legal label domain, so a wrong read fails OPEN and never
/// converges. A wrong anchor costs one misplaced pane and reaches no
/// roster decision, which is why the same objection does not apply.
///
/// Needed because [`find_anchor_pane`] matches the launch command
/// byte-for-byte, so any divergence — a repo path spelled differently,
/// a pane started before a rename, an agent restarted by hand — leaves
/// a visible reviewer that cannot anchor anything. A hand-titled pane
/// can anchor too; that is the accepted cost of not requiring a
/// command, and it buys one misplaced pane at worst.
///
/// `tab` scoping is an invariant, not hygiene: `list-panes` spans ALL
/// tabs, `(reviewer)` appears in nearly every clank tab, and zellij
/// 0.45.0 exposes no action that moves a pane between tabs
/// (`BreakPane*` are keybindings only) — so anchoring onto a foreign
/// tab strands the pane there until someone closes and respawns it.
///
/// The status TUI stamps a status glyph onto agent titles and both
/// shapes are live at once, so the glyph is stripped before matching —
/// the same rule `parse_agent_panes` classifies with.
fn find_reviewer_pane_by_title(panes: &[ZellijPane], tab: u32) -> Option<&ZellijPane> {
    let suffix = format!(" ({})", clank_core::vocab::Role::Reviewer.as_str());
    panes.iter().find(|p| {
        !p.is_plugin
            && p.tab_id == tab
            && crate::cli::status_tui::strip_leading_emoji(&p.title).ends_with(&suffix)
    })
}

/// Where to focus before `new-pane`, and whether the caller must be
/// told which pane was used.
#[derive(Debug, Default, PartialEq, Eq)]
struct AnchorChoice {
    /// Focus decides which TAB `new-pane` opens in — nothing more.
    focus: Option<String>,
    /// Set only when [`stack_reviewer_panes`] cannot rediscover the
    /// anchor from the CURRENT roster, so the caller has to carry the
    /// id over itself.
    report: Option<String>,
    /// Whether a new pane may be born directly into the anchor's
    /// stack — true ONLY for an exact-command current reviewer whose
    /// column is verified to hold nothing else.
    ///
    /// Deliberately much narrower than "the anchor looks like a
    /// reviewer". Joining a stack is IRREVERSIBLE: `stack-panes` can
    /// only build a stack, never take a pane out of one (measured,
    /// `status_tui/zellij.rs`). So every anchor whose stack cannot be
    /// PROVEN clean — the caller pane, a title match that admits
    /// hand-titled false positives, a departing reviewer, or a column
    /// already contaminated by the instrument pane — keeps the
    /// recoverable create-then-`stack-panes` path, where the cost of
    /// being wrong stays one misplaced pane (codex on 5f29616).
    stackable: bool,
}

/// Pick the anchor for a pane about to be created, in descending order
/// of how well it identifies the repo's tab:
///
/// 1. a CURRENT reviewer of this repo, by launch command — the stack
///    call rediscovers it, so it is not reported;
/// 2. a DEPARTING reviewer, by launch command — its pane is still live
///    (removes run after the layout) and knows the tab the replacement
///    belongs in, but `stack` is given the new roster and cannot name
///    it, so it is reported;
/// 3. any reviewer pane in the CALLER's tab, by title — reported for
///    the same reason;
/// 4. the caller's own pane — a tab, not a reviewer, so nothing to
///    report.
fn select_anchor(
    panes: &[ZellijPane],
    current_cmds: &[String],
    departing_cmds: &[String],
    caller: Option<String>,
) -> AnchorChoice {
    if let Some(p) = find_anchor_pane(panes, current_cmds) {
        return AnchorChoice {
            focus: Some(p.pane_id()),
            report: None,
            stackable: stack_is_clean(panes, p, current_cmds),
        };
    }
    let reported = find_anchor_pane(panes, departing_cmds)
        .map(ZellijPane::pane_id)
        .or_else(|| {
            caller
                .as_deref()
                .and_then(|r| panes.iter().find(|p| p.pane_id() == r))
                .map(|p| p.tab_id)
                .and_then(|tab| find_reviewer_pane_by_title(panes, tab))
                .map(ZellijPane::pane_id)
        });
    AnchorChoice {
        // Every anchor below the exact-command match is unproven: a
        // title match accepts hand-titled false positives, a departing
        // reviewer is on its way out, and the caller fallback is a tab
        // hint rather than a stack. None may be joined irreversibly.
        stackable: false,
        focus: reported.clone().or(caller),
        report: reported,
    }
}

/// Two panes in the same column span of the same tab.
fn same_column(a: &ZellijPane, b: &ZellijPane) -> bool {
    a.tab_id == b.tab_id && a.pane_x == b.pane_x && a.pane_columns == b.pane_columns
}

/// Full placement identity. `tab_id` FIRST: identical coordinates in
/// two different tabs are two panes, not one stack.
fn pane_geom(p: &ZellijPane) -> (u32, u16, u16, u16, u16) {
    (p.tab_id, p.pane_x, p.pane_y, p.pane_columns, p.pane_rows)
}

/// Whether two panes are members of the SAME zellij stack.
///
/// Same-column ABUTMENT is the normal layout, not a stack: the
/// instrument pane sits directly under the reviewer region by design
/// (`agent_group_kdl`). What marks a true sibling is the collapse to a
/// single title row, or identical geometry. Reading the whole column
/// as the stack made the healthy landscape tab look contaminated and
/// disabled the fast path exactly where the jump shows (codex on
/// ed9fc0e).
fn same_stack(a: &ZellijPane, b: &ZellijPane) -> bool {
    pane_geom(a) == pane_geom(b)
        || (same_column(a, b)
            && (a.pane_rows == 1 || b.pane_rows == 1)
            && (a.pane_y + a.pane_rows == b.pane_y || b.pane_y + b.pane_rows == a.pane_y))
}

/// The extent of a reviewer set that forms ONE stack.
enum StackSpan {
    /// Every member reports the whole stack area.
    Identical,
    /// One expanded member plus one-row title bars, tiling a
    /// contiguous run in one column: rows `[top, bottom)`.
    Run { top: u16, bottom: u16 },
}

/// Prove `mine` is a single stack and describe its extent.
///
/// `None` when they are not one stack — a drifted or unmeasurable
/// arrangement claims no span, so callers fail CLOSED.
fn reviewer_stack_span(mine: &[&ZellijPane]) -> Option<StackSpan> {
    let first = mine.first()?;
    // Every geometry field is `#[serde(default)]`, so an absent one
    // reads as 0 — and all-zero panes compare IDENTICAL, which would
    // report a stack we never saw (codex on 7b3536c).
    if mine.iter().any(|p| p.pane_columns == 0 || p.pane_rows == 0) {
        return None;
    }
    if mine.iter().all(|p| pane_geom(p) == pane_geom(first)) {
        return Some(StackSpan::Identical);
    }
    let one_column = mine.iter().all(|p| same_column(p, first));
    // Exactly one expanded AND every other member exactly one row —
    // the captured shape. "Not expanded" would admit a zero-row pane,
    // which also satisfies the contiguity equation below.
    let one_expanded = mine.iter().filter(|p| p.pane_rows > 1).count() == 1;
    let rest_collapsed = mine.iter().filter(|p| p.pane_rows == 1).count() == mine.len() - 1;
    let mut rows: Vec<(u16, u16)> = mine.iter().map(|p| (p.pane_y, p.pane_rows)).collect();
    rows.sort();
    let contiguous = rows.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0);
    (one_column && one_expanded && rest_collapsed && contiguous).then(|| StackSpan::Run {
        top: rows[0].0,
        bottom: rows[rows.len() - 1].0 + rows[rows.len() - 1].1,
    })
}

/// Whether `foreign` is a MEMBER of the stack `span` describes.
///
/// A tile beginning exactly at the run's bottom is OUTSIDE. That is
/// the healthy instrument pane: it abuts the reviewer stack in every
/// landscape tab without joining it, which is why adjacency alone can
/// never decide membership (codex on 1e57467).
fn inside_stack(foreign: &ZellijPane, first: &ZellijPane, span: &StackSpan) -> bool {
    match span {
        StackSpan::Identical => pane_geom(foreign) == pane_geom(first),
        StackSpan::Run { top, bottom } => {
            same_column(foreign, first) && foreign.pane_y >= *top && foreign.pane_y < *bottom
        }
    }
}

/// Whether the anchor's STACK holds only current reviewers of this
/// repo.
///
/// The question is MEMBERSHIP, not neighbourhood, and membership is a
/// property of the reviewer SET rather than of any pair. A healthy
/// landscape tab has the full-height instrument pane starting exactly
/// at the reviewer run's bottom: adjacent to the last collapsed
/// reviewer, and not in the stack. Deciding by adjacency — even
/// transitively — walks straight across that boundary and refuses the
/// fast path in the normal multi-reviewer shape (codex on 1e57467).
///
/// So the reviewer set must first be PROVEN to be one stack, and only
/// panes inside that proven extent count as members. An arrangement
/// that proves nothing is not clean.
fn stack_is_clean(panes: &[ZellijPane], anchor: &ZellijPane, current_cmds: &[String]) -> bool {
    if anchor.pane_columns == 0 || anchor.pane_rows == 0 {
        return false;
    }
    let is_current_reviewer = |p: &ZellijPane| {
        !p.is_plugin
            && p.terminal_command
                .as_deref()
                .is_some_and(|c| current_cmds.iter().any(|x| x == c))
    };
    let tab: Vec<&ZellijPane> = panes.iter().filter(|p| p.tab_id == anchor.tab_id).collect();
    let mine: Vec<&ZellijPane> = tab
        .iter()
        .copied()
        .filter(|p| is_current_reviewer(p))
        .collect();
    let foreign = || tab.iter().copied().filter(|p| !is_current_reviewer(p));

    // The anchor alone: no stack of its own yet, so the only question
    // is whether anything else already shares one with it.
    if mine.len() <= 1 {
        return !foreign().any(|p| same_stack(p, anchor));
    }
    let Some(span) = reviewer_stack_span(&mine) else {
        return false;
    };
    let first = mine[0];
    !foreign().any(|p| inside_stack(p, first, &span))
}

/// What [`add_reviewer_pane`] made, and what it leaned on to place it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReviewerPaneAdd {
    /// The pane created this pass — `None` when one already existed or
    /// creation failed.
    pub created: Option<String>,
    /// The pane that anchored the new one, when [`stack_reviewer_panes`]
    /// cannot rediscover it — a pane matched by TITLE (no launch
    /// command to be named by) or a DEPARTING reviewer (named by
    /// command, but no longer on the roster `stack` is given).
    ///
    /// Reported because `stack` names members by exact launch command
    /// against the CURRENT roster: an anchor it cannot name is absent
    /// from the id list, the list falls short of the two entries
    /// `stack-panes` needs, and the call is skipped — so the pane this
    /// anchored would never join the stack it was placed against.
    pub anchor: Option<String>,
}

/// The pane the current clank process runs in, identified authoritatively
/// from the `ZELLIJ_PANE_ID` env var zellij sets per-pane. A clank command
/// always runs in a terminal pane (never the plugin pane that can share
/// the same numeric id), so the ref is `terminal_<id>`. `None` when unset.
fn caller_pane_id() -> Option<String> {
    let id = std::env::var("ZELLIJ_PANE_ID").ok()?;
    (!id.is_empty()).then(|| format!("terminal_{id}"))
}

/// Where the calling clank process runs, for a scan that must not
/// count the caller's own pane as a holder: `clank agent start` runs
/// INSIDE the pane whose `terminal_command` it would otherwise find.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallerIdentity {
    /// No pane is the caller.
    NotInZellij,
    /// Inside zellij without a pane id: some pane MAY be the caller,
    /// and there is no way to tell which.
    Unidentified,
    /// Pane ids repeat across sessions, so the pair is the identity.
    Pane { session: String, pane_id: String },
}

pub(crate) fn caller_identity() -> CallerIdentity {
    if !in_session() {
        return CallerIdentity::NotInZellij;
    }
    let session = std::env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    match (session, caller_pane_id()) {
        (Some(session), Some(pane_id)) => CallerIdentity::Pane { session, pane_id },
        _ => CallerIdentity::Unidentified,
    }
}

/// A pane tagged with the session it lives in.
#[derive(Debug)]
pub(crate) struct SessionPane {
    pub(crate) session: String,
    pub(crate) pane: ZellijPane,
}

/// The names of sessions whose server is up, from `list-sessions -n`
/// output: the first token of each line, skipping the EXITED ones
/// (their server is gone and would not answer an action anyway).
fn live_session_names(list_output: &str) -> Vec<String> {
    list_output
        .lines()
        .filter(|l| !l.contains("EXITED"))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// Every pane of every live session, each tagged with its session:
/// one `list-panes` per session, all started at once, collected until
/// `deadline`. A session that has not answered by then is killed and
/// dropped — measured here: two stale two-month-old servers took 8.7 s
/// and 4.8 s to answer while every healthy one took under 0.2 s, and
/// an agent launch cannot wait on that. Dropping a session means its
/// panes are not seen, which fails OPEN (no holder found).
pub(crate) fn panes_in_all_sessions(deadline: std::time::Duration) -> Vec<SessionPane> {
    use std::io::Read;
    use std::process::Stdio;

    struct Pending {
        session: String,
        child: std::process::Child,
        reader: std::thread::JoinHandle<Vec<u8>>,
    }
    let mut pending: Vec<Pending> = live_session_names(&zellij_list_sessions())
        .into_iter()
        .filter_map(|session| {
            let mut child = std::process::Command::new("zellij")
                .args([
                    "-s",
                    &session,
                    "action",
                    "list-panes",
                    "--json",
                    "--command",
                    "--tab",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut stdout = child.stdout.take()?;
            // Drained on its own thread so a listing larger than the
            // pipe buffer cannot wedge the child before it exits.
            let reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                buf
            });
            Some(Pending {
                session,
                child,
                reader,
            })
        })
        .collect();

    let started = std::time::Instant::now();
    let mut panes = Vec::new();
    loop {
        let mut i = 0;
        while i < pending.len() {
            match pending[i].child.try_wait() {
                Ok(Some(status)) => {
                    let done = pending.swap_remove(i);
                    if status.success()
                        && let Ok(buf) = done.reader.join()
                        && let Ok(listed) = serde_json::from_slice::<Vec<ZellijPane>>(&buf)
                    {
                        panes.extend(listed.into_iter().map(|pane| SessionPane {
                            session: done.session.clone(),
                            pane,
                        }));
                    }
                }
                Ok(None) => i += 1,
                Err(_) => {
                    pending.swap_remove(i);
                }
            }
        }
        if pending.is_empty() || started.elapsed() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for mut late in pending {
        let _ = late.child.kill();
        let _ = late.child.wait();
    }
    panes
}

/// Whether `new-pane` can place a pane INTO a stack as it creates it.
///
/// Probed from the help TEXT, not the exit status: `new-pane` exists on
/// every client, so its exit code distinguishes nothing — only the flag
/// list does. Not from `--version` either, for the reason
/// [`placement_capability`] gives.
///
/// This probe matters more than the arrangement it buys. zellij's
/// parser REJECTS an unknown flag, so guessing wrong does not misplace
/// the pane — it fails `new-pane` outright and the reviewer never
/// spawns at all, which is far worse than the visible jump this
/// removes.
fn new_pane_can_stack() -> bool {
    std::process::Command::new("zellij")
        .args(["action", "new-pane", "--help"])
        .output()
        .is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("--stacked")
        })
}

/// The `new-pane` argv, split out so the placement decision is
/// assertable without spawning a zellij.
fn new_pane_argv(label: &str, repo: &str, stacked: bool) -> Vec<String> {
    let mut argv = vec!["new-pane".to_string()];
    if stacked {
        argv.push("--stacked".to_string());
    }
    argv.extend([
        "--name".to_string(),
        agent_pane_title(label, "reviewer"),
        "--cwd".to_string(),
        repo.to_string(),
        "--".to_string(),
        "clank".to_string(),
    ]);
    argv.extend(agent_start_argv(label, repo));
    argv
}

// ── Subprocess helpers the status TUI's reconciler calls ──────────
//
// These lived beside the reconciler, which made the TUI file a second
// spawner. Every `zellij` process now starts from THIS file, so the
// ownership gate can name one owner (zellij-is-the-workspace).

/// The current tab's `(stable id, name)`, or `None` outside zellij or
/// if the query fails. Raw `current-tab-info` text is parsed by the
/// caller, which keeps the parser pure and testable without a spawn.
pub(crate) fn current_tab_info() -> Option<String> {
    if !in_session() {
        return None;
    }
    let out = std::process::Command::new("zellij")
        .args(["action", "current-tab-info"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn rename_tab(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): capture + discard the child's
    // stdout/stderr so a rename error never bleeds onto the alt-screen
    // the TUI owns. The loop is event-driven, so an inherited error
    // line would PERSIST until the next watcher event, not flicker
    // (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-tab-by-id", id, name])
        .output();
}

/// Raw `list-panes` text (`PANE_ID  TYPE  TITLE`, one per line) — the
/// cheap listing the retitler parses. Distinct from
/// [`list_agent_panes`], which is the `--json` form the reconciler
/// needs geometry from.
pub(crate) fn list_panes_text() -> Option<String> {
    let out = std::process::Command::new("zellij")
        .args(["action", "list-panes"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn rename_pane(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): isolate the child's stdout/stderr
    // from the alt-screen — a stale pane id (closed between list-panes
    // and the rename) or any zellij hiccup must not bleed an error line
    // onto the TUI, which the event-driven loop would leave until the
    // next watcher event (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-pane", "--pane-id", id, name])
        .output();
}

/// zellij is the workspace, so `clank open` without it is a setup
/// problem, not a runtime one. Say so ONCE, before composing anything:
/// the failure otherwise surfaced as a raw OS error from the spawn
/// (`spawning zellij: No such file or directory`) after a silent
/// empty `list-sessions`, which told the user nothing about what to
/// install (zellij-is-the-workspace).
fn require_zellij() -> anyhow::Result<()> {
    require_zellij_given(placement_capability())
}

/// The decision, split from the probe so it is testable without
/// uninstalling zellij: a missing binary is the ONE capability state
/// that stops `open`. An old client is not — it still opens, it just
/// cannot stack panes, and `doctor` reports that separately.
fn require_zellij_given(cap: PlacementCapability) -> anyhow::Result<()> {
    match cap {
        PlacementCapability::NoZellij => anyhow::bail!(
            "zellij is not installed (or not on PATH). `clank open` runs the team in a \
             zellij session — install it from https://zellij.dev and re-run."
        ),
        PlacementCapability::ClientTooOld | PlacementCapability::ClientSupports => Ok(()),
    }
}

/// What the INSTALLED zellij client can do about pane placement.
///
/// Three outcomes, kept apart because the operator's next step
/// differs: no zellij at all is not a problem to fix, an old client
/// is an upgrade, and a capable client is nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementCapability {
    /// No zellij on PATH — nothing to say.
    NoZellij,
    /// A client without `stack-panes`: reviewer panes will not be
    /// stacked, and the only symptom is a wrong arrangement.
    ClientTooOld,
    /// The client parser accepts it. NOT a statement about a running
    /// SERVER: replacing the binary under a live session leaves an
    /// older server that may reject what this client can spell
    /// (zellij-pane-placement-and-cost).
    ClientSupports,
}

pub fn placement_capability() -> PlacementCapability {
    // Probed by CAPABILITY, not by parsing `zellij --version`: a HEAD
    // build can report a version AHEAD of the latest release, so
    // version arithmetic answers a different question than "does this
    // binary have the action".
    match std::process::Command::new("zellij")
        .args(["action", "stack-panes", "--help"])
        .output()
    {
        Err(_) => PlacementCapability::NoZellij,
        Ok(o) if o.status.success() => PlacementCapability::ClientSupports,
        Ok(_) => PlacementCapability::ClientTooOld,
    }
}

/// Run `zellij action <args>`, swallowing output and errors. Best-effort
/// by construction: a zellij hiccup must never fail the caller (the
/// roster mutation already persisted) nor bleed onto a pane. Returns
/// stdout on success.
fn zellij_action(args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("zellij")
        .arg("action")
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn list_agent_panes() -> Option<Vec<ZellijPane>> {
    let stdout = zellij_action(&["list-panes", "--json", "--command"])?;
    serde_json::from_slice(&stdout).ok()
}

/// The full `(cols, rows)` extent of a zellij tab, from the live
/// `list-panes` geometry: the max right/bottom edge over every pane in that
/// tab (terminal AND plugin — the tab/status-bar plugins span the full width,
/// so they pin the true extent). This is what `clank open`'s orientation
/// detection wants, as opposed to `term_size()` which is the caller's OWN
/// pane (the 65% stage) and mis-reads a portrait tab as landscape. `(0, 0)`
/// when the tab has no panes or the geometry fields are absent (callers fall
/// back to `term_size`).
fn tab_dims(panes: &[ZellijPane], tab_id: u32) -> (u16, u16) {
    let mut cols = 0u16;
    let mut rows = 0u16;
    for p in panes.iter().filter(|p| p.tab_id == tab_id) {
        cols = cols.max(p.pane_x.saturating_add(p.pane_columns));
        rows = rows.max(p.pane_y.saturating_add(p.pane_rows));
    }
    (cols, rows)
}

/// Best-effort: open a pane for a newly-added reviewer and put it in
/// this repo's reviewer stack, working from the pass's shared `panes`
/// listing (zellij-one-listing-per-pass).
/// Idempotent: no-op if the pane already exists in the listing.
///
/// Stacking is NOT done here — see [`stack_reviewer_panes`], which
/// the caller runs ONCE over the whole desired set. Doing it per-add
/// off the pass-start snapshot silently skipped stacking whenever two
/// reviewers were added at once: each call saw no existing reviewer
/// and a one-element set (codex on 2d273c6). Returning the created id
/// lets the caller accumulate what this pass actually made.
///
/// Placement is by pane id via `zellij action stack-panes`, NOT by
/// `new-pane --stacked` onto whatever is focused. That
/// focus-dependence is how the instrument pane ended up inside the
/// reviewer stack: with no reviewer anchor the old code focused the
/// caller, and a roster add is issued FROM the status TUI, so the
/// caller IS the status pane. Naming the reviewer ids explicitly makes
/// the instrument pane unstackable by construction — it is simply
/// never in the list (zellij-pane-placement-and-cost).
///
/// ONE exception, and it does not reopen that hole. When the anchor is
/// an exact-command CURRENT reviewer AND its column is verified to
/// hold nothing else ([`stack_is_clean`]), the pane is born stacked so
/// it never appears loose and jumps. The old bug was stacking onto an
/// UNVERIFIED focus; here the focus is proven to be a clean reviewer
/// column from the same pass-start listing, and every unproven anchor
/// still takes the explicit-id path. This matters because joining a
/// stack cannot be undone — so the fast path must be provably right,
/// not merely likely.
///
/// Measured properties of `stack-panes` this relies on: running
/// processes survive it, panes outside the id list are untouched, and
/// a stale id is tolerated (exit 0, nothing damaged) — which matters
/// because the listing threaded in here can be a pass old.
pub(crate) fn add_reviewer_pane(
    repo: &Path,
    label: &str,
    other_reviewers: &[String],
    departing: &[String],
    panes: &[ZellijPane],
) -> ReviewerPaneAdd {
    let repo_str = repo.to_string_lossy();
    if find_pane_by_command(panes, &agent_start_command(label, &repo_str)).is_some() {
        return ReviewerPaneAdd::default();
    }
    // Focus decides which TAB `new-pane` opens in — nothing more. An
    // existing reviewer keeps the pane in the repo's tab; else the
    // caller's own pane does. A wrong guess here used to also decide
    // what got stacked, which is exactly what it no longer does.
    let anchor_cmds: Vec<String> = other_reviewers
        .iter()
        .map(|l| agent_start_command(l, &repo_str))
        .collect();
    let departing_cmds: Vec<String> = departing
        .iter()
        .map(|l| agent_start_command(l, &repo_str))
        .collect();
    let choice = select_anchor(panes, &anchor_cmds, &departing_cmds, caller_pane_id());
    let anchor = choice.report;
    if let Some(id) = &choice.focus {
        zellij_action(&["focus-pane-id", id]);
    }

    // Born in the stack rather than created loose and moved: the move
    // is a SECOND action, and zellij has already drawn the pane in its
    // default spot before it lands.
    let new_pane = new_pane_argv(label, &repo_str, choice.stackable && new_pane_can_stack());
    let refs: Vec<&str> = new_pane.iter().map(String::as_str).collect();
    let Some(out) = zellij_action(&refs) else {
        return ReviewerPaneAdd::default();
    };
    // `new-pane` prints the id it created, so the caller can stack the
    // fresh pane without paying for another listing.
    let created = String::from_utf8_lossy(&out).trim().to_string();
    if created.is_empty() {
        return ReviewerPaneAdd::default();
    }
    ReviewerPaneAdd {
        created: Some(created),
        anchor,
    }
}

/// Put this repo's reviewer panes — the ones already live plus any
/// `extra_ids` created this pass — into one stack.
///
/// Called ONCE per pass over the complete set, never per add. The
/// instrument pane is not in the set, so it cannot be swept in.
/// Returns whether the panes are stacked AFTERWARDS, read back rather
/// than assumed: `stack-panes` exits 0 on a stale id without doing
/// anything, so its status proves nothing (codex on 2d273c6).
pub(crate) fn stack_reviewer_panes(
    repo: &Path,
    reviewers: &[String],
    panes: &[ZellijPane],
    extra_ids: &[String],
) -> bool {
    let repo_str = repo.to_string_lossy();
    let mut ids: Vec<String> = panes
        .iter()
        .filter(|p| {
            agent_pane_label(p, &repo_str).is_some_and(|l| reviewers.iter().any(|r| r == l))
        })
        .map(ZellijPane::pane_id)
        .collect();
    for id in extra_ids {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    // `stack-panes` needs two ids to express anything. It used to
    // return `true` here, which reported SUCCESS for the single
    // reviewer stacked with the instrument pane — the first-reviewer
    // form of the reported bug — and the caller then cached that
    // broken tab as repaired (codex on cc4ae5e). Whether one pane is
    // correctly placed is a question only the read-back answers.
    if ids.len() >= 2 {
        let mut args: Vec<&str> = vec!["stack-panes", "--"];
        args.extend(ids.iter().map(String::as_str));
        zellij_action(&args);
    }
    match snapshot_panes() {
        Some(fresh) => reviewers_are_stacked(&fresh, repo, reviewers),
        // Cannot tell: report NOT placed so the pass stays
        // unconverged and retries, rather than caching a guess.
        None => false,
    }
}

/// Whether this repo's reviewer panes form ONE zellij stack that the
/// instrument pane is not in.
///
/// zellij reports a stack in TWO shapes, and both are accepted because
/// both have been measured:
///
/// - **Identical geometry** — every member reports the whole stack
///   area (the original observation this predicate was built on).
/// - **Expanded + collapsed** — one member holds the area and the rest
///   are one-row title bars, sharing a column span and tiling a
///   contiguous run. Captured live on 0.45.0; requiring only the first
///   shape made every correctly stacked tab read as broken, so the
///   reconcile pass repaired it on every refresh forever and stole
///   focus each time (placement-reads-zellij-stacks-correctly).
///
/// Accepting one shape and not the other is what caused the bug, so
/// neither is privileged.
///
/// `tab_id` equality is an explicit invariant: `list-panes` spans ALL
/// tabs, so panes in different tabs can share a column span and read
/// as contiguous by coordinate alone (codex on 5320c1d), and no
/// `stack-panes` call can join two tabs.
///
/// `list-panes --json` on 0.45.0 exposes no stack flag —
/// `is_floating` / `is_fullscreen` / `is_held` / `is_suppressed` and an
/// empty `index_in_pane_group` — so geometry is the only signal.
pub(crate) fn reviewers_are_stacked(
    panes: &[ZellijPane],
    repo: &Path,
    reviewers: &[String],
) -> bool {
    let repo_str = repo.to_string_lossy();
    let mine: Vec<&ZellijPane> = panes
        .iter()
        .filter(|p| {
            agent_pane_label(p, &repo_str).is_some_and(|l| reviewers.iter().any(|r| r == l))
        })
        .collect();
    let Some(first) = mine.first() else {
        return true;
    };
    let status_command = format!("clank status --repo {repo_str} --tui");
    let statuses: Vec<&ZellijPane> = panes
        .iter()
        .filter(|p| !p.is_plugin && p.terminal_command.as_deref() == Some(status_command.as_str()))
        .collect();
    // A lone reviewer forms no stack of its own; the only question is
    // whether the instrument pane shares one WITH it — the reported
    // bug's first-reviewer form, in either shape.
    if mine.len() == 1 {
        return !statuses.iter().any(|st| same_stack(st, first));
    }
    let Some(span) = reviewer_stack_span(&mine) else {
        return false;
    };
    // The instrument pane must not be a MEMBER of that stack.
    !statuses.iter().any(|st| inside_stack(st, first, &span))
}

/// Best-effort: close the panes of removed reviewers, matched by exact
/// launch command in the pass's shared listing. `labels` is a MULTISET
/// — duplicate entries close DISTINCT panes (the duplicate-pane plan
/// entries; codex 5d498d0). Closing kills each pane's process tree —
/// that is the agent-exit guarantee. Focus restoration is the worker's
/// pass-level transaction.
pub(crate) fn remove_reviewer_panes(repo: &Path, labels: &[String], panes: &[ZellijPane]) {
    for id in remove_target_ids(labels, panes, &repo.to_string_lossy()) {
        zellij_action(&["close-pane", "--pane-id", &id]);
    }
}

/// The DISTINCT pane ids the remove multiset resolves to: each label
/// occurrence consumes one matching pane, so duplicate labels map to
/// different panes (codex 4df5f0c). Pure.
fn remove_target_ids(labels: &[String], panes: &[ZellijPane], repo_path: &str) -> Vec<String> {
    let mut budget: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for l in labels {
        *budget.entry(l.as_str()).or_default() += 1;
    }
    let mut ids = Vec::new();
    // EXITED copies first. When a label has both a corpse and a live
    // pane, the excess to close is the corpse — listing order can put
    // the live one first, closing the working agent and leaving the
    // corpse to be counted as an excess again next pass, forever.
    //
    // Only the CHOICE changes: when every pane of a label is being
    // closed the budget covers them all and this just reorders ids.
    for exited_first in [true, false] {
        for pane in panes.iter().filter(|p| p.exited == exited_first) {
            let Some(label) = agent_pane_label(pane, repo_path) else {
                continue;
            };
            let Some(n) = budget.get_mut(label) else {
                continue;
            };
            if *n == 0 {
                continue;
            }
            *n -= 1;
            ids.push(pane.pane_id());
        }
    }
    ids
}

/// Outcome of [`compose_promote_layout`]: a ready-to-apply layout KDL, or
/// a deliberate no-op (the skip-on-unrecognized safety rule — see the
/// `agent-promote-zellij-relocation` plan).
#[derive(Debug, PartialEq)]
enum PromoteRelayout {
    Apply(String),
    Skip,
}

/// PURE core of the promote relocation: project the config role flip onto
/// the live zellij panes by composing a fresh stage(new master 65%) + stack
/// (other agents) + status layout, reusing the SAME composition `clank open`
/// emits so every slot's command byte-matches the live `invoked_with`.
///
/// `override-layout` re-flows the WHOLE active tab with no positional
/// fallback (a slot whose command is off by one byte spawns a new pane AND
/// lets the unmatched original get closed), so classification is
/// all-or-nothing, decided here before anything is applied:
/// - Find the caller pane by `pane_id == caller_pane_ref`; its `tab_id` is
///   the active tab. Absent → [`PromoteRelayout::Skip`] (no anchor for the
///   active-tab filter, and declarative focus would have nowhere to land).
/// - Every live terminal (non-plugin) pane in the active tab must classify
///   as a roster-agent pane (command byte-equals `agent_start_command`) or
///   the status pane. ANY unclassified live pane → Skip.
/// - The new master must have a live pane in the active tab, else → Skip.
/// - reviewers = live agent labels minus the new master, in on-screen
///   (list-panes) order. A roster agent with no live pane is simply omitted
///   (omit ≠ skip) — relocation still proceeds.
///
/// The composed KDL marks the caller's own pane `focus=true` so the
/// override doesn't yank focus elsewhere.
fn compose_promote_layout(
    panes: &[ZellijPane],
    caller_pane_ref: Option<&str>,
    roster_labels: &[String],
    new_master: &str,
    repo_path: &str,
    term: (u16, u16),
) -> PromoteRelayout {
    // The caller pane anchors the active-tab filter AND declarative focus.
    let Some(caller_ref) = caller_pane_ref else {
        return PromoteRelayout::Skip;
    };
    let Some(caller) = panes.iter().find(|p| p.pane_id() == caller_ref) else {
        return PromoteRelayout::Skip;
    };
    let active_tab = caller.tab_id;
    let caller_command = caller.terminal_command.clone();

    // Pre-compute each roster label's launch command once for matching.
    let agent_commands: Vec<(String, &String)> = roster_labels
        .iter()
        .map(|l| (agent_start_command(l, repo_path), l))
        .collect();
    let status_command = format!("clank status --repo {repo_path} --tui");

    // Classify every live terminal pane in the active tab, preserving the
    // on-screen order so the stack keeps its current arrangement.
    let mut live_agents: Vec<String> = Vec::new();
    for pane in panes
        .iter()
        .filter(|p| !p.is_plugin && p.tab_id == active_tab)
    {
        let cmd = pane.terminal_command.as_deref();
        if cmd == Some(status_command.as_str()) {
            continue;
        }
        match cmd.and_then(|c| {
            agent_commands
                .iter()
                .find(|(start, _)| start == c)
                .map(|(_, label)| (*label).clone())
        }) {
            Some(label) => live_agents.push(label),
            // An unclassified live terminal pane (manual shell/editor, null
            // command, or an agent pane whose command doesn't byte-match) —
            // the whole relocation is unsafe, skip it.
            None => return PromoteRelayout::Skip,
        }
    }

    // The new master must actually have a live pane to reposition.
    if !live_agents.iter().any(|l| l == new_master) {
        return PromoteRelayout::Skip;
    }

    let reviewers: Vec<String> = live_agents
        .into_iter()
        .filter(|l| l != new_master)
        .collect();

    let kdl = match compose_kdl(
        &caller_tab_name(repo_path),
        repo_path,
        new_master,
        &reviewers,
        None,
        term,
    ) {
        Ok(k) => k,
        Err(_) => return PromoteRelayout::Skip,
    };

    // Declarative focus: mark the caller's own pane focus=true so the
    // override keeps focus where the operator ran promote. Matched by the
    // caller pane's command (the byte-identical slot in the composed KDL).
    match inject_focus_on_command(&kdl, caller_command.as_deref()) {
        Ok(k) => PromoteRelayout::Apply(k),
        Err(_) => PromoteRelayout::Skip,
    }
}

/// Tab name for the composed promote layout. `override-layout
/// --apply-only-to-active-tab` re-flows the existing tab in place, so the
/// name is cosmetic; derive it from the repo basename to match `clank open`.
fn caller_tab_name(repo_path: &str) -> String {
    Path::new(repo_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_path.to_string())
}

/// Mark the agent/status pane whose `command "clank"` + `args …` reproduce
/// `caller_command` with `focus=true`, editing the parsed KDL tree (never
/// string-spliced). A no-match is fine — focus just isn't pinned — but a
/// `None` caller command (the caller pane has no `invoked_with`) can't be
/// matched, so callers that need focus should treat that as a skip upstream.
fn inject_focus_on_command(kdl: &str, caller_command: Option<&str>) -> anyhow::Result<String> {
    let Some(caller_command) = caller_command else {
        return Ok(kdl.to_string());
    };
    let mut doc: kdl::KdlDocument = kdl.parse().map_err(|e: kdl::KdlError| {
        anyhow::anyhow!("composed promote layout is not valid KDL: {e}")
    })?;
    mark_focus_in_doc(&mut doc, caller_command);
    Ok(doc.to_string())
}

/// Depth-first: set `focus=true` on the first `pane` node whose
/// `command`/`args` children reproduce `caller_command`. Returns whether a
/// match was set so recursion can stop after the first hit.
fn mark_focus_in_doc(doc: &mut kdl::KdlDocument, caller_command: &str) -> bool {
    for node in doc.nodes_mut() {
        if node.name().value() == "pane" && pane_command(node).as_deref() == Some(caller_command) {
            node.entries_mut()
                .retain(|e| e.name().map(|n| n.value()) != Some("focus"));
            node.push(kdl::KdlEntry::new_prop("focus", true));
            return true;
        }
        if let Some(children) = node.children_mut()
            && mark_focus_in_doc(children, caller_command)
        {
            return true;
        }
    }
    false
}

/// Reconstruct the `clank …` command string a composed agent pane launches,
/// from its `command "clank"` + `args "…" "…"` child nodes — the form
/// zellij reports as that pane's `terminal_command` once live.
fn pane_command(node: &kdl::KdlNode) -> Option<String> {
    let children = node.children()?;
    let command = children
        .nodes()
        .iter()
        .find(|n| n.name().value() == "command")?
        .entries()
        .first()?
        .value()
        .as_string()?
        .to_string();
    let args_node = children.nodes().iter().find(|n| n.name().value() == "args");
    let mut parts = vec![command];
    if let Some(args_node) = args_node {
        for entry in args_node.entries() {
            if entry.name().is_none()
                && let Some(s) = entry.value().as_string()
            {
                parts.push(s.to_string());
            }
        }
    }
    Some(parts.join(" "))
}

/// Best-effort: inside a zellij session, project a just-applied master
/// promotion onto the live layout — the new master becomes the 65% STAGE
/// and the demoted old master joins the reviewer STACK, reusing the running
/// panes (no respawn — `override-layout` matches on `invoked_with`).
///
/// No-op outside zellij, on any zellij failure, or when
/// [`compose_promote_layout`] declines (the skip-on-unrecognized rule). The
/// config role change is the source of truth and stands regardless.
/// `roster_labels` is the full post-flip roster (master + reviewers);
/// `old_master` titles the demoted pane.
pub(crate) fn relocate_for_promote(
    repo: &Path,
    new_master: &str,
    old_master: Option<&str>,
    roster_labels: &[String],
    panes: &[ZellijPane],
) {
    let repo_str = repo.to_string_lossy().into_owned();
    // Orientation comes from the caller tab's full extent (what `clank open`
    // sees), not `term_size()` (the caller's own pane = the 65% stage, which
    // mis-reads a portrait tab as landscape). Fall back to `term_size` when
    // the caller pane, its tab, or the geometry is unavailable / degenerate.
    let caller_ref = caller_pane_id();
    let term = caller_ref
        .as_deref()
        .and_then(|r| panes.iter().find(|p| p.pane_id() == r).map(|p| p.tab_id))
        .map(|tab_id| tab_dims(panes, tab_id))
        .filter(|(cols, rows)| *cols != 0 && *rows != 0)
        .unwrap_or_else(|| {
            let (rows, cols) = crate::cli::term::term_size();
            (cols, rows)
        });
    let kdl = match compose_promote_layout(
        panes,
        caller_ref.as_deref(),
        roster_labels,
        new_master,
        &repo_str,
        term,
    ) {
        PromoteRelayout::Apply(kdl) => kdl,
        PromoteRelayout::Skip => return,
    };

    let Ok(path) = write_layout_file(repo, &kdl) else {
        return;
    };
    let path_str = path.display().to_string();
    // No-op-on-failure: only stamp titles if the override actually applied,
    // so a failed override leaves the layout untouched rather than a partial
    // mutation (titles changed but panes not relocated).
    if zellij_action(&[
        "override-layout",
        &path_str,
        "--apply-only-to-active-tab",
        "--retain-existing-plugin-panes",
    ])
    .is_none()
    {
        return;
    }

    // Stamp the new roles into the two changed panes' titles. This is
    // REQUIRED, not cosmetic: `override-layout` keeps matched panes' existing
    // titles, and the status-TUI retitle loop derives each agent's role by
    // PARSING its pane title (`parse_agent_panes`) — so a stale title makes
    // the TUI re-affirm the OLD role forever. `rename-pane --pane-id` targets
    // by id (no focus change); the TUI re-adds the status emoji on its next
    // refresh. Best-effort.
    rename_agent_pane(panes, new_master, &repo_str, "master");
    // No old master (nothing was master-titled — e.g. a whole-team
    // replacement converging at startup): stage the new one, demote
    // nobody.
    if let Some(old) = old_master {
        rename_agent_pane(panes, old, &repo_str, "reviewer");
    }
    // Focus restoration is the worker's pass-level transaction
    // (zellij-one-listing-per-pass).
}

/// Best-effort `rename-pane --pane-id` of the pane running
/// `agent_start_command(label, repo)` to `agent_pane_title(label, role)`.
/// By-id, so no focus change; no-op if no such pane is live.
fn rename_agent_pane(panes: &[ZellijPane], label: &str, repo_path: &str, role: &str) {
    if let Some(pane) = find_pane_by_command(panes, &agent_start_command(label, repo_path)) {
        zellij_action(&[
            "rename-pane",
            "--pane-id",
            &pane.pane_id(),
            &agent_pane_title(label, role),
        ]);
    }
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
/// Max generated session-name length. Zellij embeds the name in its
/// UNIX socket path (`$TMPDIR/zellij-<uid>/<version>/<name>`) and
/// refuses names past the budget at arg-parse time; macOS's long
/// `$TMPDIR` leaves exactly 24 chars (probed on zellij 0.44: 24 OK,
/// 25 refused — the error's "less than 0 characters" number is
/// zellij's display bug). Linux runtime dirs are shorter, so the
/// macOS bound is the binding one (zellij-session-name-budget).
const SESSION_NAME_MAX: usize = 24;

/// Deterministic per-repo session name (`zellij-session-dedup`):
/// zellij enforces NAME UNIQUENESS, so a stable name makes
/// duplicate clank sessions unrepresentable — re-running
/// `clank open zellij` attaches instead of minting another
/// randomly-named session full of fresh agent instances.
///
/// Long basenames are CAPPED to [`SESSION_NAME_MAX`]: truncated, with a
/// short hash of the FULL basename appended so distinct long repos
/// sharing a prefix never collide — and still deterministic, which
/// reconciliation (find-session-by-name, add missing tabs) depends on.
/// Short names are byte-identical to before, so existing sessions keep
/// matching.
fn session_name(basename: &str) -> String {
    let full = format!("clank-{basename}");
    if full.len() <= SESSION_NAME_MAX {
        return full;
    }
    let hash = fnv1a_short(basename.as_bytes());
    // "clank-" + truncated stem + "-" + 4 hex chars == SESSION_NAME_MAX.
    let keep = SESSION_NAME_MAX - "clank-".len() - 1 - 4;
    let mut stem = String::new();
    for c in basename.chars() {
        if stem.len() + c.len_utf8() > keep {
            break;
        }
        stem.push(c);
    }
    format!("clank-{stem}-{hash}")
}

/// 4-hex-char FNV-1a. Inlined (not `DefaultHasher`) because the value
/// must be stable across clank RELEASES — a session named by one build
/// must be findable by the next — and std's hasher makes no such
/// guarantee.
fn fnv1a_short(bytes: &[u8]) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x01000193);
    }
    format!("{:04x}", (h >> 16) ^ (h & 0xffff))
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
    #[test]
    fn status_pane_size_is_a_percentage_in_both_orientations() {
        // zellij-status-pane-resizable: an ABSOLUTE layout size makes
        // zellij treat the pane as FIXED (interactive resize refused),
        // so the instrument pane must be percentage-sized in BOTH
        // orientations. Every layout path (base, swap variants, fork,
        // promote relayout) composes through agent_group_kdl, so this
        // one assertion covers them all.
        for o in [Orientation::Landscape, Orientation::Portrait] {
            let kdl = agent_group_kdl(
                "/repo",
                "alice",
                ReviewerStack::Roster(&["bob".to_string()]),
                o,
            );
            let status_line = kdl
                .lines()
                .find(|l| l.contains("name=\"status\""))
                .expect("an instrument pane line");
            assert!(
                status_line.contains("size=\"30%\""),
                "{o:?}: percentage-sized, got: {status_line}"
            );
            // No absolute size anywhere in the group (chrome bars are
            // composed elsewhere and stay fixed on purpose).
            assert!(
                !kdl.lines()
                    .any(|l| l.contains("size=") && !l.contains("size=\"")),
                "{o:?}: no absolute pane sizes in the agent group:\n{kdl}"
            );
        }
    }

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

    // ── zellij-in-session-orientation: the client-tty picker ──
    //
    // ps shapes below are verbatim from a live machine (macOS ttysNNN;
    // one Linux pts/N case).

    const PS_LIVE: &str = "\
??        /usr/local/bin/zellij --server /tmp/zellij-501/0.44.3/clank-clank
ttys004   clank open zellij
ttys004   zellij attach clank-dark_skippy
ttys013   zellij --session clank-fsctl --new-session-with-layout /Users/llfourn/src/fsctl/.clank/zellij/layout.kdl options --session-serialization false
ttys030   -zsh
";

    #[test]
    fn picker_finds_the_attach_shape_client_by_session_name() {
        assert_eq!(
            pick_zellij_client_tty(PS_LIVE, Some("clank-dark_skippy")).as_deref(),
            Some("ttys004")
        );
    }

    #[test]
    fn picker_finds_the_session_flag_shape_client() {
        assert_eq!(
            pick_zellij_client_tty(PS_LIVE, Some("clank-fsctl")).as_deref(),
            Some("ttys013")
        );
    }

    #[test]
    fn picker_never_matches_a_layout_path_containing_the_name() {
        // `fsctl` appears inside ttys013's layout PATH but only
        // `clank-fsctl` is a standalone token — an unrelated session
        // name that's a substring of a path must not match; with two
        // clients present the ambiguous fallback is None.
        assert_eq!(pick_zellij_client_tty(PS_LIVE, Some("fsctl")), None);
    }

    #[test]
    fn picker_ignores_the_server_and_non_zellij_commands() {
        // The `??`-tty server line and `clank open zellij` (prog !=
        // zellij) are not clients; with the two real clients left and
        // no name match → ambiguous → None.
        assert_eq!(pick_zellij_client_tty(PS_LIVE, Some("nope")), None);
        assert_eq!(pick_zellij_client_tty(PS_LIVE, None), None);
    }

    #[test]
    fn picker_uses_a_sole_client_when_the_name_does_not_match() {
        let ps = "ttys002   zellij\n??   /usr/local/bin/zellij --server /x\n";
        assert_eq!(
            pick_zellij_client_tty(ps, Some("anything")).as_deref(),
            Some("ttys002"),
            "a bare unnamed client is unambiguous"
        );
    }

    #[test]
    fn picker_excludes_transient_action_invocations() {
        // clank's own `zellij -s <name> action ...` queries run
        // concurrently on a tty; they are not the attached client.
        let ps = "\
ttys009   zellij -s clank-foo action query-tab-names
ttys004   zellij attach clank-foo
";
        assert_eq!(
            pick_zellij_client_tty(ps, Some("clank-foo")).as_deref(),
            Some("ttys004")
        );
    }

    #[test]
    fn picker_handles_linux_pts_ttys() {
        let ps = "pts/4   zellij attach clank-foo\n";
        assert_eq!(
            pick_zellij_client_tty(ps, Some("clank-foo")).as_deref(),
            Some("pts/4"),
            "caller prepends /dev/ → /dev/pts/4"
        );
    }

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
        assert_eq!(
            agent_start_command("bob", "/repo"),
            "clank agent start bob --repo /repo"
        );
    }

    #[test]
    fn remove_target_ids_resolve_duplicates_to_distinct_panes() {
        // codex 4df5f0c: the remove MULTISET maps each occurrence to a
        // DIFFERENT pane id; unrelated panes and over-budget matches
        // are untouched.
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":0},
              {"id":2,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":0},
              {"id":3,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":0},
              {"id":4,"is_plugin":false,"title":"claude (master)","terminal_command":"clank agent start claude --repo /a","tab_id":0}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            remove_target_ids(&["codex".into(), "codex".into()], &panes, "/a"),
            vec!["terminal_1".to_string(), "terminal_2".to_string()],
            "two entries, two DISTINCT panes; the third codex pane and claude survive"
        );
    }

    #[test]
    fn active_tab_id_is_read_from_current_tab_info() {
        // Real shape: `name:` / `id:` / `position:` lines. `id` is the
        // STABLE tab id `list-panes` reports as `tab_id`, not the
        // position — they differ on every session with a closed tab.
        let text = "name: \u{1f4a4} self-spend-prompt\nid: 20\nposition: 7\n";
        assert_eq!(parse_active_tab_id(text), Some(20));
        assert_eq!(parse_active_tab_id("position: 7\n"), None);
        assert_eq!(parse_active_tab_id("id: not-a-number\n"), None);
    }

    #[test]
    fn focused_pane_is_the_one_in_the_active_tab() {
        // Every tab reports a focused pane — measured live: 13 tabs,
        // 13 `is_focused` panes. Without the active tab there is no
        // answer, and guessing MOVES the user's focus rather than
        // restoring it.
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"is_plugin":false,"is_focused":true,"tab_id":0,"title":"a"},
              {"id":2,"is_plugin":false,"is_focused":false,"tab_id":0,"title":"b"},
              {"id":3,"is_plugin":false,"is_focused":true,"tab_id":7,"title":"c"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            focused_pane_in(&panes, Some(7)).as_deref(),
            Some("terminal_3")
        );
        assert_eq!(
            focused_pane_in(&panes, Some(0)).as_deref(),
            Some("terminal_1")
        );
        // No active tab, and a tab with no focused pane: no answer.
        assert_eq!(focused_pane_in(&panes, None), None);
        assert_eq!(focused_pane_in(&panes, Some(99)), None);
    }

    #[test]
    fn pass_focus_falls_back_to_the_caller_pane() {
        // codex 5d498d0: when list-clients fails the pass restore must
        // fall back to the caller's own (authoritative) pane, as the
        // old per-op restores did.
        assert_eq!(
            focus_target_from(Some("terminal_3".into()), Some("terminal_9".into())).as_deref(),
            Some("terminal_3"),
            "client focus wins when available"
        );
        assert_eq!(
            focus_target_from(None, Some("terminal_9".into())).as_deref(),
            Some("terminal_9")
        );
        assert_eq!(focus_target_from(None, None), None);
    }

    #[test]
    fn live_zellij_stacks_read_as_placed() {
        // THE regression, on geometry captured live from zellij 0.45.0
        // (session `clank-fsctl`, 2026-08-18). A stack renders as one
        // EXPANDED member plus collapsed one-row title bars — members
        // do NOT share identical geometry, which the old predicate
        // required. Every one of these tabs was correctly stacked and
        // read as broken, so the reconcile pass repaired them on every
        // refresh forever and stole the user's focus each time.
        let pane = |cmd: &str, tab: u32, x: u16, y: u16, w: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: tab,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: x,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let cx = "clank agent start codex --repo /r";
        let ru = "clank agent start ruthless --repo /r";

        // (name, codex geom, ruthless geom) — verbatim captures.
        let live: [(&str, (u16, u16, u16, u16), (u16, u16, u16, u16)); 5] = [
            (
                "nonce-aware-coin-selection",
                (116, 1, 62, 1),
                (116, 2, 62, 93),
            ),
            ("firmware-upgrade-nudge", (116, 1, 62, 93), (116, 94, 62, 1)),
            ("sign-task-path-bounds", (116, 1, 62, 1), (116, 2, 62, 93)),
            ("change-index-leak-demo", (0, 88, 125, 1), (0, 89, 125, 46)),
            ("fix-anchor-above-tip", (116, 1, 62, 1), (116, 2, 62, 93)),
        ];
        for (name, c, r) in live {
            let panes = vec![
                pane(cx, 1, c.0, c.1, c.2, c.3),
                pane(ru, 1, r.0, r.1, r.2, r.3),
            ];
            assert!(
                reviewers_are_stacked(&panes, repo, &two),
                "{name}: a real zellij stack must read as PLACED"
            );
        }
    }

    #[test]
    fn a_genuine_split_is_not_a_stack() {
        // Side by side in different column spans — the shape a stack
        // repair should still act on.
        let pane = |cmd: &str, x: u16, y: u16, w: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: 1,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: x,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let panes = vec![
            pane("clank agent start codex --repo /r", 0, 1, 60, 40),
            pane("clank agent start ruthless --repo /r", 60, 1, 60, 40),
        ];
        assert!(!reviewers_are_stacked(&panes, repo, &two));
    }

    #[test]
    fn unmeasurable_geometry_fails_closed_in_both_shapes() {
        // Every geometry field is `#[serde(default)]`, so an absent one
        // reads as 0. All-zero panes compare IDENTICAL, and a zero-row
        // pane also satisfies the contiguity equation — both would
        // report a stack nobody observed and CACHE it, leaving a real
        // misplacement unrepaired (codex on 7b3536c). A listing we
        // cannot measure must never read as placed.
        let bare = |cmd: &str, y: u16, w: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: 1,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: 116,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let cx = "clank agent start codex --repo /r";
        let ru = "clank agent start ruthless --repo /r";

        // Identical shape, geometry entirely absent.
        let zeroed = vec![bare(cx, 0, 0, 0), bare(ru, 0, 0, 0)];
        assert!(
            !reviewers_are_stacked(&zeroed, repo, &two),
            "all-zero geometry must not read as an identical-shape stack"
        );

        // Run shape, with a ZERO-row member that satisfies contiguity
        // (y + 0 == y) but is not a collapsed title bar.
        let zero_member = vec![bare(cx, 1, 62, 0), bare(ru, 1, 62, 93)];
        assert!(
            !reviewers_are_stacked(&zero_member, repo, &two),
            "a zero-row pane is not a collapsed member"
        );

        // The real collapsed shape still passes, so the guard did not
        // simply reject everything.
        let real = vec![bare(cx, 1, 62, 1), bare(ru, 2, 62, 93)];
        assert!(reviewers_are_stacked(&real, repo, &two));
    }

    #[test]
    fn identical_geometry_in_different_tabs_is_not_one_stack() {
        // The hole codex found: the identical-shape branch compared
        // only x/y/columns/rows, so two panes at the SAME coordinates
        // in DIFFERENT tabs read as one stack. `list-panes` spans all
        // tabs, so that pair is ordinary — every tab has a pane at
        // (116,1). The previous different-tab test used different
        // y/rows and therefore only exercised the run-shaped branch.
        let pane = |cmd: &str, tab: u32| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: tab,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: 116,
            pane_y: 1,
            pane_columns: 62,
            pane_rows: 40,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let panes = vec![
            pane("clank agent start codex --repo /r", 1),
            pane("clank agent start ruthless --repo /r", 9),
        ];
        assert!(
            !reviewers_are_stacked(&panes, repo, &two),
            "identical coordinates in different tabs are two panes, not a stack"
        );
    }

    #[test]
    fn reviewers_in_different_tabs_are_never_stacked() {
        // `list-panes` spans ALL tabs, so identical column spans can
        // collide across tabs and read as contiguous by coordinate
        // alone. Same-tab is an invariant, not a coincidence (codex on
        // 5320c1d) — and no `stack-panes` call can join two tabs.
        let pane = |cmd: &str, tab: u32, y: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: tab,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: 116,
            pane_y: y,
            pane_columns: 62,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let panes = vec![
            pane("clank agent start codex --repo /r", 1, 1, 1),
            pane("clank agent start ruthless --repo /r", 7, 2, 93),
        ];
        assert!(
            !reviewers_are_stacked(&panes, repo, &two),
            "different tabs cannot be one stack"
        );
    }

    #[test]
    fn the_instrument_pane_inside_the_stack_still_fails() {
        // The ORIGINAL bug must stay caught under the new model: a
        // status pane that is a member of the reviewers' run.
        let pane = |cmd: &str, y: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: 1,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: 116,
            pane_y: y,
            pane_columns: 62,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/r");
        let two = vec!["codex".to_string(), "ruthless".to_string()];
        let panes = vec![
            pane("clank agent start codex --repo /r", 1, 1),
            pane("clank agent start ruthless --repo /r", 3, 92),
            // Wedged BETWEEN the reviewers — a stack member, which is
            // the reported bug. Contrast the healthy layout, where the
            // instrument pane sits directly BELOW the run.
            pane("clank status --repo /r --tui", 2, 1),
        ];
        assert!(
            !reviewers_are_stacked(&panes, repo, &two),
            "the instrument pane sharing the run is the reported bug"
        );
    }

    #[test]
    fn a_lone_reviewer_stacked_with_status_is_not_placed() {
        // The FIRST-reviewer form of the reported bug, and the shape
        // of the tab that prompted this work: one reviewer sharing a
        // stack with the instrument pane. A two-reviewer fixture
        // misses it because the reviewers-agree check passes
        // vacuously on a single pane (codex on cc4ae5e).
        let pane = |cmd: &str, x: u16, y: u16, w: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: 0,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: x,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/repo");
        let one = vec!["kimi".to_string()];
        let rev = agent_start_command("kimi", "/repo");
        let status = "clank status --repo /repo --tui";

        assert!(
            !reviewers_are_stacked(
                &[pane(&rev, 125, 1, 53, 133), pane(status, 125, 1, 53, 133)],
                repo,
                &one
            ),
            "one reviewer sharing the instrument pane's geometry is NOT placed"
        );
        assert!(
            reviewers_are_stacked(
                &[pane(&rev, 125, 1, 53, 100), pane(status, 125, 101, 53, 33)],
                repo,
                &one
            ),
            "beside the instrument pane IS placed"
        );
    }

    #[test]
    fn status_sharing_the_reviewer_stack_is_not_placed() {
        // THE reported bug, as a fixture: adding an agent put it in a
        // stack WITH the status pane. Every reviewer geometry agrees
        // there, so a reviewers-only check calls it placed and caches
        // the broken tab forever (codex on d5121e1). The instrument
        // pane sharing the stack's geometry is what makes it wrong.
        let pane = |cmd: &str, x: u16, y: u16, w: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: String::new(),
            tab_id: 0,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: x,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/repo");
        let reviewers = vec!["r1".to_string(), "r2".to_string()];
        let r1 = agent_start_command("r1", "/repo");
        let r2 = agent_start_command("r2", "/repo");
        let status = "clank status --repo /repo --tui";

        // BROKEN: both reviewers AND status at the stack geometry.
        let broken = vec![
            pane(&r1, 40, 0, 40, 16),
            pane(&r2, 40, 0, 40, 16),
            pane(status, 40, 0, 40, 16),
        ];
        assert!(
            !reviewers_are_stacked(&broken, repo, &reviewers),
            "status inside the reviewer stack is NOT placed"
        );

        // GOOD: reviewers stacked, status beside them.
        let good = vec![
            pane(&r1, 40, 0, 40, 16),
            pane(&r2, 40, 0, 40, 16),
            pane(status, 40, 16, 40, 8),
        ];
        assert!(reviewers_are_stacked(&good, repo, &reviewers));

        // Reviewers not stacked at all is also not placed.
        let split = vec![pane(&r1, 40, 0, 40, 8), pane(&r2, 40, 8, 40, 8)];
        assert!(!reviewers_are_stacked(&split, repo, &reviewers));

        // A repo with no status pane still resolves on the reviewers.
        let no_status = vec![pane(&r1, 40, 0, 40, 16), pane(&r2, 40, 0, 40, 16)];
        assert!(reviewers_are_stacked(&no_status, repo, &reviewers));
    }

    #[test]
    fn agent_pane_label_parses_only_this_repos_agent_panes() {
        // tui-zellij-pane-reconcile: the reconciler classifies live
        // panes by the EXACT command agent_start_command composes —
        // other repos' agents, plugins, and arbitrary commands with
        // similar prefixes must all be invisible to it.
        let pane = |cmd: Option<&str>, is_plugin: bool| ZellijPane {
            id: 1,
            is_plugin,
            terminal_command: cmd.map(str::to_string),
            title: String::new(),
            tab_id: 0,
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            pane_x: 0,
            pane_y: 0,
            pane_columns: 0,
            pane_rows: 0,
        };
        let ours = pane(Some("clank agent start bob --repo /repo"), false);
        assert_eq!(agent_pane_label(&ours, "/repo"), Some("bob"));
        // Same command shape, different repo → not ours.
        assert_eq!(agent_pane_label(&ours, "/other"), None);
        // Extra trailing args break the exact match (deliberate: only
        // panes we composed count).
        let extra = pane(Some("clank agent start bob --repo /repo --x"), false);
        assert_eq!(agent_pane_label(&extra, "/repo"), None);
        // Plugins and unrelated commands are invisible.
        assert_eq!(
            agent_pane_label(
                &pane(Some("clank agent start bob --repo /repo"), true),
                "/repo"
            ),
            None
        );
        assert_eq!(agent_pane_label(&pane(Some("zsh"), false), "/repo"), None);
        assert_eq!(agent_pane_label(&pane(None, false), "/repo"), None);

        // The FULL legal label domain round-trips. `AgentLabel`
        // forbids only empty, dot-segments and `/`, so spaces and
        // emoji are legal names a user can really create — and an
        // agent this parser cannot read is an agent reconciliation
        // can never converge on, because it drives both the initial
        // classification and the verify.
        for label in ["two words", "🔥bot", "a b c", "with-dash", "under_score"] {
            let cmd = agent_start_command(label, "/repo");
            assert_eq!(
                agent_pane_label(&pane(Some(&cmd), false), "/repo"),
                Some(label),
                "the command composed for {label:?} must parse back to it"
            );
        }
        // A label that itself looks like the suffix still round-trips:
        // the LAST occurrence is the real one.
        let tricky = "x --repo /repo";
        let cmd = agent_start_command(tricky, "/repo");
        assert_eq!(
            agent_pane_label(&pane(Some(&cmd), false), "/repo"),
            Some(tricky)
        );
        // An empty label is not a pane we composed.
        assert_eq!(
            agent_pane_label(
                &pane(Some("clank agent start  --repo /repo"), false),
                "/repo"
            ),
            None
        );
    }

    // A realistic `list-panes --json --command` (subset of fields per
    // pane; serde must ignore the rest). Two `codex (reviewer)` panes in
    // DIFFERENT repos prove command-identity disambiguates labels that
    // repeat across worktree tabs.
    const LIST_PANES_JSON: &str = r#"[
      {"id":0,"is_plugin":true,"is_focused":false,"title":"(.) - zellij:link","terminal_command":null,"plugin_url":"zellij:link","tab_id":0},
      {"id":0,"is_plugin":false,"is_focused":true,"title":"claude (master)","terminal_command":"clank agent start claude --repo /a","pane_x":0,"tab_id":0},
      {"id":1,"is_plugin":false,"is_focused":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":0},
      {"id":2,"is_plugin":false,"is_focused":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":0},
      {"id":7,"is_plugin":false,"is_focused":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /b","tab_id":1}
    ]"#;

    fn parse_panes() -> Vec<ZellijPane> {
        serde_json::from_str(LIST_PANES_JSON).expect("real list-panes shape parses")
    }

    #[test]
    fn pane_id_targets_terminal_vs_plugin() {
        let panes = parse_panes();
        assert_eq!(panes[0].pane_id(), "plugin_0");
        assert_eq!(panes[1].pane_id(), "terminal_0");
    }

    #[test]
    fn find_pane_by_command_disambiguates_same_label_across_repos() {
        let panes = parse_panes();
        assert_eq!(
            find_pane_by_command(&panes, &agent_start_command("codex", "/a"))
                .unwrap()
                .pane_id(),
            "terminal_1"
        );
        assert_eq!(
            find_pane_by_command(&panes, &agent_start_command("codex", "/b"))
                .unwrap()
                .pane_id(),
            "terminal_7"
        );
        assert!(find_pane_by_command(&panes, &agent_start_command("ghost", "/a")).is_none());
    }

    #[test]
    fn find_anchor_pane_picks_an_existing_reviewer_for_this_repo() {
        let panes = parse_panes();
        let anchor = find_anchor_pane(&panes, &[agent_start_command("codex", "/a")]);
        assert_eq!(anchor.unwrap().pane_id(), "terminal_1");
        // No reviewer of this repo present → no anchor (first reviewer case).
        assert!(find_anchor_pane(&panes, &[agent_start_command("codex", "/c")]).is_none());
    }

    // Both title shapes an anchor search must accept. The status TUI
    // stamps a status glyph onto every agent pane it classifies
    // (`status_tui/zellij.rs`), so matching `agent_pane_title`'s bare
    // output alone would miss them all; a pane the TUI has not
    // classified keeps the bare form. The command-less pane is what
    // anything not launched by clank looks like.
    const TITLE_SHAPES_JSON: &str = r#"[
      {"id":138,"is_plugin":false,"title":"🔨 claude (master)","terminal_command":"clank agent start claude --repo /a","tab_id":18},
      {"id":140,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":18},
      {"id":201,"is_plugin":false,"title":"kimi (reviewer)","terminal_command":null,"tab_id":18},
      {"id":111,"is_plugin":false,"title":"👀 codex (reviewer)","terminal_command":null,"tab_id":19},
      {"id":113,"is_plugin":false,"title":"💤 ruthless (reviewer)","terminal_command":null,"tab_id":19},
      {"id":150,"is_plugin":false,"title":"🔨 claude (master)","terminal_command":"clank agent start claude --repo /c","tab_id":20},
      {"id":151,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /c --tui","tab_id":20},
      {"id":152,"is_plugin":true,"title":"ghost (reviewer)","terminal_command":null,"tab_id":20}
    ]"#;

    fn title_shape_panes() -> Vec<ZellijPane> {
        serde_json::from_str(TITLE_SHAPES_JSON).expect("list-panes shape parses")
    }

    #[test]
    fn remove_closes_the_exited_copy_not_the_live_one() {
        // The reported shape: a duplicate pane whose tool refused the
        // session (`already has an active writer`) and exited 1, while
        // the first pane kept working. Multiplicity says close ONE.
        //
        // Listing order alone would close whichever came first — and
        // if that is the live agent, the corpse survives, is counted
        // as an excess again next pass, and the tab never converges
        // while the working agent is repeatedly killed.
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"is_plugin":false,"terminal_command":"clank agent start codex --repo /a","tab_id":0,"title":"codex (reviewer)","exited":false},
              {"id":2,"is_plugin":false,"terminal_command":"clank agent start codex --repo /a","tab_id":0,"title":"codex (reviewer)","exited":true},
              {"id":3,"is_plugin":false,"terminal_command":"clank agent start claude --repo /a","tab_id":0,"title":"claude (master)","exited":false}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            remove_target_ids(&["codex".to_string()], &panes, "/a"),
            vec!["terminal_2".to_string()],
            "the corpse is the excess copy, whatever order the listing reports"
        );
        // A label leaving the roster entirely still loses BOTH panes:
        // the preference picks WHICH, never HOW MANY.
        assert_eq!(
            remove_target_ids(&["codex".to_string(), "codex".to_string()], &panes, "/a"),
            vec!["terminal_2".to_string(), "terminal_1".to_string()]
        );
    }

    #[test]
    fn an_exited_pane_still_counts_as_its_label_s_pane() {
        // One rule for every reader: exit state never changes WHO a
        // pane belongs to. So a crashed agent is not silently replaced
        // — respawn is manual by design, and auto-respawn would
        // relaunch a tool straight back into the error that killed it.
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"is_plugin":false,"terminal_command":"clank agent start codex --repo /a","tab_id":0,"title":"codex (reviewer)","exited":true}
            ]"#,
        )
        .unwrap();
        assert_eq!(agent_pane_label(&panes[0], "/a"), Some("codex"));
        assert_eq!(
            agent_pane_pairs(&panes, Path::new("/a")),
            vec![("codex".to_string(), false)]
        );
        // The idempotence guard sees it too, so no replacement is made.
        assert!(
            find_pane_by_command(&panes, &agent_start_command("codex", "/a")).is_some(),
            "a corpse must still satisfy the guard, or the pass respawns it"
        );
    }

    #[test]
    fn anchor_prefers_a_current_reviewer_and_does_not_report_it() {
        let panes = parse_panes();
        let c = select_anchor(
            &panes,
            &[agent_start_command("codex", "/a")],
            &[agent_start_command("gone", "/a")],
            Some("terminal_0".to_string()),
        );
        assert_eq!(c.focus.as_deref(), Some("terminal_1"));
        // `stack` names members by command against the current roster,
        // so it finds this one on its own.
        assert_eq!(c.report, None);
        // This fixture carries no geometry, so the column cannot be
        // PROVEN clean and the fast path is refused. Unverifiable is
        // not the same as safe.
        assert!(
            !c.stackable,
            "an unverifiable column takes the recoverable path"
        );
    }

    #[test]
    fn anchor_falls_back_to_a_departing_reviewer_and_reports_it() {
        // The swap case: the arriving reviewer has no peer on the new
        // roster, and the pane that knows the tab belongs to the agent
        // leaving this pass. Its pane is still live because removes run
        // after the layout.
        let panes = parse_panes();
        let c = select_anchor(
            &panes,
            &[agent_start_command("nobody", "/a")],
            &[agent_start_command("codex", "/a")],
            Some("terminal_0".to_string()),
        );
        assert_eq!(c.focus.as_deref(), Some("terminal_1"));
        // Not on the roster `stack` is given, so it must be carried.
        assert_eq!(c.report.as_deref(), Some("terminal_1"));
    }

    #[test]
    fn a_departing_anchor_is_scoped_to_this_repo() {
        // `codex` runs in BOTH /a and /b. Command identity carries the
        // repo path, so a departing `codex` in /a lands on /a's pane —
        // being pulled into another repo's tab is the failure zellij
        // 0.45.0 cannot undo.
        let panes = parse_panes();
        let c = select_anchor(
            &panes,
            &[agent_start_command("nobody", "/a")],
            &[agent_start_command("codex", "/a")],
            None,
        );
        assert_eq!(c.report.as_deref(), Some("terminal_1"));
        // A departing label with no pane in THIS repo anchors nothing.
        let c = select_anchor(
            &panes,
            &[agent_start_command("nobody", "/a")],
            &[agent_start_command("codex", "/c")],
            None,
        );
        assert_eq!(c.report, None);
    }

    #[test]
    fn anchor_falls_back_to_the_caller_pane_and_reports_nothing() {
        // Caller sits in tab 20, which holds a master, the instrument
        // pane and a plugin — no reviewer to anchor on.
        let panes = title_shape_panes();
        let c = select_anchor(
            &panes,
            &[agent_start_command("nobody", "/c")],
            &[],
            Some("terminal_151".to_string()),
        );
        // The caller pane picks a TAB and nothing more: it must never
        // enter the stack set.
        assert_eq!(c.focus.as_deref(), Some("terminal_151"));
        assert_eq!(c.report, None);
        assert!(
            !c.stackable,
            "focusing the caller is a tab hint, not a stack — `--stacked` here \
             would put a reviewer into the status pane's stack"
        );
    }

    /// Negative eligibility: a reported anchor is a reviewer by SHAPE,
    /// which is not the same as a stack that can be safely joined.
    ///
    /// A departing reviewer is on its way out and a title match admits
    /// hand-titled false positives. Their old cost was one misplaced
    /// pane, recoverable by `stack-panes`; joining them with
    /// `--stacked` is not recoverable at all.
    #[test]
    fn a_reported_anchor_is_never_joined_at_creation() {
        let panes = parse_panes();
        let c = select_anchor(
            &panes,
            &[agent_start_command("nobody", "/a")],
            &[agent_start_command("gone", "/a")],
            Some("terminal_0".to_string()),
        );
        assert!(
            c.report.is_some(),
            "the fixture must yield a departing anchor"
        );
        assert!(
            !c.stackable,
            "a departing anchor is not a proven-clean stack"
        );
    }

    /// The HEALTHY LANDSCAPE shape: the reviewer region and the
    /// instrument pane share the 35% column, with status as a
    /// full-height tile directly below. This is `agent_group_kdl`'s
    /// normal output, and it must be joinable — reading the column as
    /// the stack made exactly this case look contaminated.
    const LANDSCAPE_HEALTHY_JSON: &str = r#"[
      {"id":40,"is_plugin":false,"title":"claude (master)","terminal_command":"clank agent start claude --repo /a","tab_id":6,"pane_x":0,"pane_columns":100,"pane_y":0,"pane_rows":40},
      {"id":41,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":6,"pane_x":100,"pane_columns":60,"pane_y":0,"pane_rows":20},
      {"id":42,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":6,"pane_x":100,"pane_columns":60,"pane_y":20,"pane_rows":20}
    ]"#;

    /// The PORTRAIT shape: status sits beside the reviewers, in its
    /// own column.
    const PORTRAIT_HEALTHY_JSON: &str = r#"[
      {"id":10,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":3,"pane_x":0,"pane_columns":40,"pane_y":20,"pane_rows":20},
      {"id":11,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":3,"pane_x":40,"pane_columns":60,"pane_y":20,"pane_rows":20}
    ]"#;

    /// The known-broken geometry: status is an actual stack SIBLING,
    /// collapsed to a title row and abutting the reviewer. Not merely
    /// another tile in the column — that is the healthy shape above.
    const CONTAMINATED_STACK_JSON: &str = r#"[
      {"id":20,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":4,"pane_x":40,"pane_columns":60,"pane_y":0,"pane_rows":19},
      {"id":21,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":4,"pane_x":40,"pane_columns":60,"pane_y":19,"pane_rows":1}
    ]"#;

    /// The MULTI-REVIEWER landscape, which is the normal shape once a
    /// second reviewer exists: one expanded, the rest collapsed to
    /// title rows, and the full-height instrument pane starting
    /// exactly at the run's bottom.
    ///
    /// The last collapsed reviewer ABUTS status, so any adjacency-based
    /// membership test walks into it and refuses the fast path here.
    const LANDSCAPE_MULTI_HEALTHY_JSON: &str = r#"[
      {"id":60,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":8,"pane_x":100,"pane_columns":60,"pane_y":0,"pane_rows":18},
      {"id":61,"is_plugin":false,"title":"ruthless (reviewer)","terminal_command":"clank agent start ruthless --repo /a","tab_id":8,"pane_x":100,"pane_columns":60,"pane_y":18,"pane_rows":1},
      {"id":62,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":8,"pane_x":100,"pane_columns":60,"pane_y":19,"pane_rows":21}
    ]"#;

    /// The IDENTICAL-geometry stack shape with the instrument pane as
    /// a member — every pane reports the whole stack area.
    const IDENTICAL_CONTAMINATED_JSON: &str = r#"[
      {"id":70,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":9,"pane_x":40,"pane_columns":60,"pane_y":0,"pane_rows":20},
      {"id":71,"is_plugin":false,"title":"ruthless (reviewer)","terminal_command":"clank agent start ruthless --repo /a","tab_id":9,"pane_x":40,"pane_columns":60,"pane_y":0,"pane_rows":20},
      {"id":72,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":9,"pane_x":40,"pane_columns":60,"pane_y":0,"pane_rows":20}
    ]"#;

    /// Status INTERLEAVED between two reviewers, which shows up as the
    /// reviewer rows no longer tiling contiguously.
    const INTERLEAVED_CONTAMINATED_JSON: &str = r#"[
      {"id":80,"is_plugin":false,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":10,"pane_x":40,"pane_columns":60,"pane_y":0,"pane_rows":1},
      {"id":81,"is_plugin":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":10,"pane_x":40,"pane_columns":60,"pane_y":1,"pane_rows":1},
      {"id":82,"is_plugin":false,"title":"ruthless (reviewer)","terminal_command":"clank agent start ruthless --repo /a","tab_id":10,"pane_x":40,"pane_columns":60,"pane_y":2,"pane_rows":18}
    ]"#;

    fn codex_and_ruthless() -> [String; 2] {
        [
            agent_start_command("codex", "/a"),
            agent_start_command("ruthless", "/a"),
        ]
    }

    #[test]
    fn the_multi_reviewer_landscape_stack_may_be_joined_at_creation() {
        let panes: Vec<ZellijPane> = serde_json::from_str(LANDSCAPE_MULTI_HEALTHY_JSON).unwrap();
        let c = select_anchor(&panes, &codex_and_ruthless(), &[], None);
        assert_eq!(c.focus.as_deref(), Some("terminal_60"));
        // The premise: the last collapsed reviewer really does abut
        // status, so this only passes because membership is decided by
        // the proven run and not by adjacency.
        assert!(
            same_stack(&panes[2], &panes[1]),
            "the fixture must abut status to the last collapsed reviewer"
        );
        assert!(
            c.stackable,
            "status begins AT the run's bottom, which is outside it — this is every landscape \
             tab with two reviewers"
        );
    }

    #[test]
    fn an_instrument_pane_sharing_the_stack_area_forbids_the_fast_path() {
        let panes: Vec<ZellijPane> = serde_json::from_str(IDENTICAL_CONTAMINATED_JSON).unwrap();
        let c = select_anchor(&panes, &codex_and_ruthless(), &[], None);
        assert_eq!(c.focus.as_deref(), Some("terminal_70"));
        assert!(
            !c.stackable,
            "identical geometry IS the stack, and status reports it too"
        );
    }

    #[test]
    fn an_interleaved_instrument_pane_forbids_the_fast_path() {
        let panes: Vec<ZellijPane> = serde_json::from_str(INTERLEAVED_CONTAMINATED_JSON).unwrap();
        let c = select_anchor(&panes, &codex_and_ruthless(), &[], None);
        assert_eq!(c.focus.as_deref(), Some("terminal_80"));
        assert!(
            !c.stackable,
            "something sits between the reviewers, so their rows no longer tile and no span can \
             be proven"
        );
    }

    #[test]
    fn the_healthy_landscape_stack_may_be_joined_at_creation() {
        let panes: Vec<ZellijPane> = serde_json::from_str(LANDSCAPE_HEALTHY_JSON).unwrap();
        let c = select_anchor(&panes, &[agent_start_command("codex", "/a")], &[], None);
        assert_eq!(c.focus.as_deref(), Some("terminal_41"));
        assert!(
            c.stackable,
            "status ABUTS the reviewer region in every landscape tab — abutting is the layout, \
             not a stack, and refusing here disables the fix where the jump actually shows"
        );
    }

    #[test]
    fn the_healthy_portrait_stack_may_be_joined_at_creation() {
        let panes: Vec<ZellijPane> = serde_json::from_str(PORTRAIT_HEALTHY_JSON).unwrap();
        let c = select_anchor(&panes, &[agent_start_command("codex", "/a")], &[], None);
        assert_eq!(c.focus.as_deref(), Some("terminal_11"));
        assert!(c.stackable, "status in its own column is no sibling at all");
    }

    /// The exact-command anchor is NOT sufficient on its own.
    #[test]
    fn an_instrument_pane_inside_the_stack_forbids_the_fast_path() {
        let panes: Vec<ZellijPane> = serde_json::from_str(CONTAMINATED_STACK_JSON).unwrap();
        let c = select_anchor(&panes, &[agent_start_command("codex", "/a")], &[], None);
        assert_eq!(
            c.focus.as_deref(),
            Some("terminal_20"),
            "still the anchor for the tab"
        );
        assert!(
            !c.stackable,
            "status collapsed to a title row and abutting IS a stack member, and joining it \
             cannot be undone"
        );
    }

    /// The pane is born in the stack instead of being created loose and
    /// moved — the move is a second action, and zellij draws the pane
    /// in its default spot before it lands.
    #[test]
    fn new_pane_is_born_stacked_only_when_there_is_a_stack() {
        let with = new_pane_argv("ruthless", "/repo", true);
        assert!(
            with.iter().any(|a| a == "--stacked"),
            "an anchored pane joins the stack at creation: {with:?}"
        );
        // Order matters to clap: flags precede the `--` separator.
        let sep = with.iter().position(|a| a == "--").unwrap();
        let flag = with.iter().position(|a| a == "--stacked").unwrap();
        assert!(flag < sep, "the flag must sit before `--`: {with:?}");

        let without = new_pane_argv("ruthless", "/repo", false);
        assert!(
            !without.iter().any(|a| a == "--stacked"),
            "with no stack to join, the flag is a guess about layout: {without:?}"
        );
        // The unstacked form is EXACTLY what shipped before, so an old
        // client falls back to today's behaviour rather than to a
        // `new-pane` its parser rejects.
        assert_eq!(without[0], "new-pane");
        assert_eq!(without[1], "--name");
    }

    /// No zellij → one message that says what to install, and an
    /// error (non-zero exit), not a raw OS error from a later spawn.
    #[test]
    fn open_without_zellij_says_to_install_it() {
        let err = require_zellij_given(PlacementCapability::NoZellij).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not installed"), "names the problem: {msg}");
        assert!(msg.contains("zellij.dev"), "says where to get it: {msg}");
        assert!(
            msg.contains("clank open"),
            "names the command that needs it: {msg}"
        );
    }

    /// Every spawn shape names what clank was DOING, in the user's
    /// terms, so a failure reads as "could not attach" rather than a
    /// bare exit code. The argv shapes are the three `open` produces
    /// plus the fallback.
    #[test]
    fn spawn_failures_say_what_was_being_attempted() {
        let s = |v: &[&str]| spawn_kind(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(
            s(&["zellij", "attach", "clank-foo"]),
            "attach to the repo's session"
        );
        assert_eq!(
            s(&["zellij", "--layout", "l.kdl"]),
            "start the repo's session"
        );
        assert_eq!(
            s(&["zellij", "action", "new-tab", "--layout", "l.kdl"]),
            "add a tab to the current session"
        );
        assert_eq!(s(&["zellij"]), "open the workspace");
    }

    /// A spawn that cannot start at all (binary vanished between the
    /// preflight and the spawn, or exists but is not executable) points
    /// at `doctor` rather than surfacing a raw OS error.
    #[test]
    fn an_unstartable_spawn_points_at_doctor() {
        let err =
            spawn_zellij(&["/nonexistent/zellij".to_string(), "attach".to_string()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("could not start zellij"), "{msg}");
        assert!(
            msg.contains("attach to the repo's session"),
            "names the attempt: {msg}"
        );
        assert!(msg.contains("clank doctor"), "says where to look: {msg}");
    }

    /// An OLD client is not a missing one. `open` still works — it
    /// just cannot stack panes, which `doctor` reports on its own.
    /// Refusing to open on an old client would turn a cosmetic gap
    /// into a hard stop.
    #[test]
    fn open_proceeds_on_an_old_client() {
        assert!(require_zellij_given(PlacementCapability::ClientTooOld).is_ok());
        assert!(require_zellij_given(PlacementCapability::ClientSupports).is_ok());
    }

    #[test]
    fn anchor_with_nothing_to_go_on_is_empty() {
        let panes = parse_panes();
        let c = select_anchor(&panes, &[agent_start_command("nobody", "/a")], &[], None);
        assert_eq!(c, AnchorChoice::default());
    }

    #[test]
    fn title_anchor_finds_the_reviewer_the_command_match_cannot_see() {
        let panes = title_shape_panes();
        // A reviewer pane the exact-command lookup cannot name, which
        // is the whole reason the fallback exists.
        assert!(find_anchor_pane(&panes, &[agent_start_command("kimi", "/a")]).is_none());
        assert_eq!(
            find_reviewer_pane_by_title(&panes, 18).map(ZellijPane::pane_id),
            Some("terminal_201".to_string())
        );
    }

    #[test]
    fn title_anchor_accepts_the_glyph_stamped_shape() {
        let panes = title_shape_panes();
        assert_eq!(
            find_reviewer_pane_by_title(&panes, 19).map(ZellijPane::pane_id),
            Some("terminal_111".to_string())
        );
    }

    #[test]
    fn title_anchor_never_picks_master_status_or_a_plugin() {
        let panes = title_shape_panes();
        // Tab 20 holds a master, the instrument pane, and a PLUGIN
        // whose title would otherwise match — the instrument pane
        // getting stacked is the original bug this must not revive.
        assert!(find_reviewer_pane_by_title(&panes, 20).is_none());
    }

    #[test]
    fn title_anchor_is_scoped_to_one_tab() {
        let panes = title_shape_panes();
        // `list-panes` spans ALL tabs and `(reviewer)` is in nearly
        // every one, so an unscoped match would anchor onto a foreign
        // tab — which zellij 0.45.0 offers no way to undo.
        for (tab, want) in [(18u32, "terminal_201"), (19, "terminal_111")] {
            assert_eq!(
                find_reviewer_pane_by_title(&panes, tab).map(ZellijPane::pane_id),
                Some(want.to_string())
            );
        }
        assert!(find_reviewer_pane_by_title(&panes, 99).is_none());
    }

    #[test]
    fn a_title_matched_pane_is_still_not_a_roster_member() {
        let panes = title_shape_panes();
        // Anchoring accepts it; CLASSIFICATION must not. A title
        // carries no ownership marker, so letting it name an agent
        // fails open (zellij-pane-placement-and-cost).
        let anchor = find_reviewer_pane_by_title(&panes, 18).expect("anchor found");
        assert_eq!(agent_pane_label(anchor, "/a"), None);
        assert!(
            agent_pane_pairs(&panes, Path::new("/a"))
                .iter()
                .all(|(l, _)| l != "kimi")
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
        let doc: kdl::KdlDocument = kdl.parse().expect("valid KDL");
        assert!(
            stacked_panes(layout_child(&doc, "tab")).is_empty(),
            "no empty stack region in the tab:\n{kdl}"
        );
        assert!(
            kdl.contains("\"--tui\""),
            "instrument pane still ships:\n{kdl}"
        );
        // The variants still carry the slot: the first reviewer added
        // after open needs somewhere to land on alt+[.
        for swap in swap_variants(&doc) {
            assert_children_stack(swap);
        }
    }

    /// Every `pane stacked=true` node under `node`, depth-first.
    fn stacked_panes(node: &kdl::KdlNode) -> Vec<&kdl::KdlNode> {
        let mut out = Vec::new();
        for child in node.children().map(|c| c.nodes()).unwrap_or_default() {
            if child.name().value() == "pane"
                && child.get("stacked").and_then(|e| e.value().as_bool()) == Some(true)
            {
                out.push(child);
            }
            out.extend(stacked_panes(child));
        }
        out
    }

    fn layout_child<'a>(doc: &'a kdl::KdlDocument, name: &str) -> &'a kdl::KdlNode {
        doc.get("layout")
            .and_then(|l| l.children())
            .and_then(|c| c.nodes().iter().find(|n| n.name().value() == name))
            .unwrap_or_else(|| panic!("layout has a `{name}` node"))
    }

    fn swap_variants(doc: &kdl::KdlDocument) -> Vec<&kdl::KdlNode> {
        let swaps: Vec<_> = doc
            .get("layout")
            .and_then(|l| l.children())
            .map(|c| {
                c.nodes()
                    .iter()
                    .filter(|n| n.name().value() == "swap_tiled_layout")
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(swaps.len(), 2, "one variant per orientation");
        swaps
    }

    /// The variant's one stack is zellij's "whatever panes are here"
    /// slot: a stacked pane whose only child is a `children` node.
    fn assert_children_stack(swap: &kdl::KdlNode) {
        let stacks = stacked_panes(swap);
        assert_eq!(stacks.len(), 1, "one stack per variant:\n{swap}");
        let slots: Vec<&str> = stacks[0]
            .children()
            .map(|c| c.nodes().iter().map(|n| n.name().value()).collect())
            .unwrap_or_default();
        assert_eq!(slots, ["children"], "the stack is a children slot:\n{swap}");
    }

    #[test]
    fn swap_variants_take_any_reviewer_pane_not_the_opening_roster() {
        // A variant is applied to the panes that EXIST. One that named
        // the opening roster had a slot per reviewer and nothing for a
        // reviewer added later, so alt+[ split that pane off the stage
        // (a-swap-layout-describes-a-shape-not-a-roster).
        let kdl = compose_kdl(
            TEST_TAB,
            TEST_REPO,
            "alice",
            &reviewers(&["bob", "carol"]),
            None,
            LANDSCAPE,
        )
        .unwrap();
        let doc: kdl::KdlDocument = kdl.parse().expect("valid KDL");
        let tab = layout_child(&doc, "tab").to_string();
        assert!(
            tab.contains("args \"agent\" \"start\" \"bob\"")
                && tab.contains("args \"agent\" \"start\" \"carol\""),
            "the tab launches every reviewer:\n{tab}"
        );
        for swap in swap_variants(&doc) {
            assert_children_stack(swap);
            let text = swap.to_string();
            assert!(
                !text.contains("\"bob\"") && !text.contains("\"carol\""),
                "a variant names no reviewer:\n{text}"
            );
            // Master and status keep command-bearing slots — matching
            // on the command is what holds them in place while the
            // reviewers fill the slot.
            assert!(
                text.contains("args \"agent\" \"start\" \"alice\""),
                "master slot keeps its command:\n{text}"
            );
            assert!(
                text.contains("\"--tui\""),
                "status slot keeps its command:\n{text}"
            );
        }
    }

    // ── session dedup (zellij-session-dedup) ──

    #[test]
    fn live_session_names_skip_the_dead_and_keep_the_current() {
        let listing = "\
clank-clank [Created 31m 15s ago] (current)
other-repo [Created 2h ago]
dead-one [Created 10h ago] (EXITED - attach to resurrect)
";
        assert_eq!(live_session_names(listing), ["clank-clank", "other-repo"]);
        assert!(live_session_names("").is_empty());
    }

    #[test]
    fn a_pane_runs_a_command_only_while_its_process_lives() {
        let json = |exited: bool, plugin: bool| {
            format!(
                r#"{{"id":4,"is_plugin":{plugin},"exited":{exited},"title":"t",
                    "terminal_command":"clank agent start codex --repo /r","tab_name":"r"}}"#
            )
        };
        let pane = |exited, plugin| -> ZellijPane {
            serde_json::from_str(&json(exited, plugin)).expect("list-panes element")
        };
        let cmd = "clank agent start codex --repo /r";
        assert!(pane(false, false).runs(cmd));
        assert!(!pane(true, false).runs(cmd), "an exited pane holds nothing");
        assert!(!pane(false, true).runs(cmd), "a plugin pane runs no agent");
        assert!(!pane(false, false).runs("clank agent start codex --repo /other"));
        assert_eq!(pane(false, false).tab_name(), "r");
        // The identity match deliberately still sees the exited copy.
        assert!(find_pane_by_command(&[pane(true, false)], cmd).is_some());
    }

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
    fn fork_path_errors_when_absent_resolves_registered_worktrees() {
        // fork_path answers through the shared resolve_fork model:
        // a REGISTERED worktree on the branch resolves (wherever it
        // lives); a bare same-name directory is not a fork.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(src)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{:?}: {out:?}", args);
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "seed",
        ]);
        // Absent → error pointing at `clank fork`.
        let err = fork_path(src, "ghost").unwrap_err().to_string();
        assert!(err.contains("ghost") && err.contains("clank fork"), "{err}");
        // A bare unregistered dir in the default namespace is NOT a
        // fork (identity is the registry, not the path).
        std::fs::create_dir_all(src.join(".clank/worktrees/foo")).unwrap();
        assert!(fork_path(src, "foo").is_err());
        // A registered worktree resolves — including one `--path`
        // put OUTSIDE the default namespace.
        let elsewhere = dir.path().join("elsewhere-wt");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "bar",
            elsewhere.to_str().unwrap(),
        ]);
        let resolved = fork_path(src, "bar").unwrap();
        assert_eq!(
            dunce::canonicalize(&resolved).unwrap(),
            dunce::canonicalize(&elsewhere).unwrap()
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

    #[test]
    fn session_name_caps_long_basenames_within_the_socket_budget() {
        // The live failure (zellij-session-name-budget): macOS's TMPDIR
        // leaves a 24-char budget; `clank-frostsnap_nostr-taipei` is 28.
        let name = session_name("frostsnap_nostr-taipei");
        assert!(
            name.len() <= SESSION_NAME_MAX,
            "capped within the probed budget: {name}"
        );
        assert!(name.starts_with("clank-"), "{name}");
        // Deterministic (reconciliation finds the session by name across
        // invocations AND releases — the hash is inlined FNV, not std's).
        assert_eq!(name, session_name("frostsnap_nostr-taipei"));
        // Distinct long repos sharing the truncated prefix don't collide.
        let other = session_name("frostsnap_nostr-tokyo!");
        assert_ne!(name, other, "hash disambiguates shared prefixes");
        assert!(other.len() <= SESSION_NAME_MAX);
        // Short names stay byte-identical (existing sessions keep
        // matching), right up to the cap.
        let at_cap = "x".repeat(SESSION_NAME_MAX - "clank-".len());
        assert_eq!(session_name(&at_cap), format!("clank-{at_cap}"));
    }

    #[test]
    fn compose_relocation_requires_the_departing_old_master_in_the_set() {
        // codex ae6338a: when the old master LEAVES the roster its pane
        // is still live during the relocation (removes run after the
        // layout). compose classifies panes against the supplied label
        // set and skips on any unclassified agent pane — so the caller
        // must include the departing source, or the relocation
        // silently no-ops exactly when a team replacement needs it.
        let panes = panes_from(&[
            agent_pane(0, "old", false),
            agent_pane(1, "codex", false),
            status_pane(2),
        ]);
        let without_departing = compose_promote_layout(
            &panes,
            Some("terminal_2"),
            &labels(&["codex"]),
            "codex",
            "/a",
            (200, 50),
        );
        assert_eq!(
            without_departing,
            PromoteRelayout::Skip,
            "unclassified live pane → skip (safety rule)"
        );
        let with_departing = compose_promote_layout(
            &panes,
            Some("terminal_2"),
            &labels(&["codex", "old"]),
            "codex",
            "/a",
            (200, 50),
        );
        assert!(
            matches!(with_departing, PromoteRelayout::Apply(_)),
            "departing source classified → relocation applies"
        );
    }

    #[test]
    fn compose_relocation_classifies_departing_reviewers_too() {
        // codex 8c4906d: a team replacement can have a departing
        // REVIEWER live during the relocation (removes run after the
        // layout) alongside a master swap — the classification set
        // must be roster ∪ every live agent label or compose skips.
        let panes = panes_from(&[
            agent_pane(0, "old-master", false),
            agent_pane(1, "leaving-rev", false),
            agent_pane(2, "codex", false),
            status_pane(3),
        ]);
        // Roster after the change: codex master, old-master demoted...
        // without the departing reviewer in the set → Skip.
        let missing = compose_promote_layout(
            &panes,
            Some("terminal_3"),
            &labels(&["codex", "old-master"]),
            "codex",
            "/a",
            (200, 50),
        );
        assert_eq!(missing, PromoteRelayout::Skip);
        let full = compose_promote_layout(
            &panes,
            Some("terminal_3"),
            &labels(&["codex", "old-master", "leaving-rev"]),
            "codex",
            "/a",
            (200, 50),
        );
        assert!(matches!(full, PromoteRelayout::Apply(_)));
    }

    // ── agent-promote-zellij-relocation: compose_promote_layout ──

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    /// A live agent pane in `/a`, tab 0. `focused` marks the caller pane.
    fn agent_pane(id: u32, label: &str, focused: bool) -> String {
        format!(
            r#"{{"id":{id},"is_plugin":false,"is_focused":{focused},"title":"{label} (x)","terminal_command":"clank agent start {label} --repo /a","tab_id":0}}"#
        )
    }

    fn status_pane(id: u32) -> String {
        format!(
            r#"{{"id":{id},"is_plugin":false,"is_focused":false,"title":"status","terminal_command":"clank status --repo /a --tui","tab_id":0}}"#
        )
    }

    fn panes_from(parts: &[String]) -> Vec<ZellijPane> {
        let json = format!("[{}]", parts.join(","));
        serde_json::from_str(&json).expect("fixture parses")
    }

    #[test]
    fn tab_dims_reads_tab_extent_for_orientation() {
        // A portrait tab: two stacked panes spanning 80 cols × (68 + 69) rows.
        // `term_size` (the caller's stage pane alone, 80×68) would read
        // landscape (80 >= 2*68? no — but the real flip case is a wide stage
        // in a portrait tab); the TAB extent (80×137) reads portrait.
        let portrait: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":false,"is_focused":true,"terminal_command":"clank agent start claude --repo /a","tab_id":0,"pane_x":0,"pane_y":0,"pane_columns":80,"pane_rows":68},
              {"id":1,"is_plugin":false,"is_focused":false,"terminal_command":"clank agent start codex --repo /a","tab_id":0,"pane_x":0,"pane_y":68,"pane_columns":80,"pane_rows":69}
            ]"#,
        )
        .unwrap();
        let (cols, rows) = tab_dims(&portrait, 0);
        assert_eq!((cols, rows), (80, 137));
        assert_eq!(Orientation::detect((cols, rows)), Orientation::Portrait);

        // A landscape tab.
        let landscape: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":false,"is_focused":true,"terminal_command":"x","tab_id":0,"pane_x":0,"pane_y":0,"pane_columns":200,"pane_rows":50}
            ]"#,
        )
        .unwrap();
        let (cols, rows) = tab_dims(&landscape, 0);
        assert_eq!((cols, rows), (200, 50));
        assert_eq!(Orientation::detect((cols, rows)), Orientation::Landscape);

        // Scoped to tab_id: a pane in ANOTHER tab is ignored.
        let mixed: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":false,"is_focused":true,"terminal_command":"x","tab_id":0,"pane_x":0,"pane_y":0,"pane_columns":80,"pane_rows":137},
              {"id":1,"is_plugin":false,"is_focused":false,"terminal_command":"y","tab_id":1,"pane_x":0,"pane_y":0,"pane_columns":300,"pane_rows":300}
            ]"#,
        )
        .unwrap();
        assert_eq!(tab_dims(&mixed, 0), (80, 137));

        // A plugin pane (tab bar, full width) pins the true extent; included.
        let with_plugin: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":true,"is_focused":false,"terminal_command":null,"tab_id":0,"pane_x":0,"pane_y":0,"pane_columns":178,"pane_rows":1},
              {"id":1,"is_plugin":false,"is_focused":true,"terminal_command":"x","tab_id":0,"pane_x":0,"pane_y":1,"pane_columns":178,"pane_rows":136}
            ]"#,
        )
        .unwrap();
        assert_eq!(tab_dims(&with_plugin, 0), (178, 137));

        // Geometry absent (older/odd list-panes) → (0,0); relocate_for_promote
        // falls back to term_size rather than mis-orienting.
        let no_geo: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":false,"is_focused":true,"terminal_command":"x","tab_id":0}
            ]"#,
        )
        .unwrap();
        assert_eq!(tab_dims(&no_geo, 0), (0, 0));
    }

    #[test]
    fn compose_promote_all_classified_makes_new_master_the_stage() {
        // claude (current master, the caller pane) + codex reviewer + status.
        // Promote codex → codex becomes the 65% stage, claude joins the
        // stack; both panes are reused (slot commands byte-match the live
        // terminal_commands).
        let panes = panes_from(&[
            agent_pane(0, "claude", true),
            agent_pane(1, "codex", false),
            status_pane(2),
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        let PromoteRelayout::Apply(kdl) = out else {
            panic!("expected Apply, got {out:?}");
        };
        // codex is the 65% stage.
        assert!(
            kdl.contains("pane size=\"65%\" name=\"codex (master)\""),
            "codex must be the stage:\n{kdl}"
        );
        // claude is stacked (a reviewer slot).
        assert!(kdl.contains("pane stacked=true"), "stack region:\n{kdl}");
        assert!(
            kdl.contains("name=\"claude (reviewer)\""),
            "claude must join the stack:\n{kdl}"
        );
        // status pane present.
        assert!(kdl.contains("name=\"status\""), "status pane:\n{kdl}");
        // Slot commands byte-match the live terminal_commands.
        assert!(kdl.contains("args \"agent\" \"start\" \"codex\" \"--repo\" \"/a\""));
        assert!(kdl.contains("args \"agent\" \"start\" \"claude\" \"--repo\" \"/a\""));
        // Multi-line KDL (zellij's parser rejects the compact form).
        assert!(kdl.lines().count() > 5, "multi-line KDL:\n{kdl}");
        // Caller's pane (claude) carries focus=true.
        assert!(
            kdl.contains("name=\"claude (reviewer)\" cwd=\"/a\" focus=true")
                || focus_on_claude(&kdl),
            "caller pane must have focus=true:\n{kdl}"
        );
        let _: kdl::KdlDocument = kdl.parse().expect("composed promote layout is valid KDL");
    }

    /// `focus=true` lands on the (claude) pane node regardless of attribute
    /// order in the serialized KDL.
    fn focus_on_claude(kdl: &str) -> bool {
        let doc: kdl::KdlDocument = kdl.parse().unwrap();
        fn walk(doc: &kdl::KdlDocument) -> bool {
            for n in doc.nodes() {
                if n.name().value() == "pane"
                    && pane_command(n).as_deref() == Some("clank agent start claude --repo /a")
                    && n.entries().iter().any(|e| {
                        e.name().map(|x| x.value()) == Some("focus")
                            && e.value().as_bool() == Some(true)
                    })
                {
                    return true;
                }
                if let Some(c) = n.children() {
                    if walk(c) {
                        return true;
                    }
                }
            }
            false
        }
        walk(&doc)
    }

    #[test]
    fn compose_promote_omits_roster_agent_with_no_live_pane() {
        // glm is on the roster but has no live pane — it's omitted from the
        // composed layout, and relocation still proceeds (omit ≠ skip).
        let panes = panes_from(&[
            agent_pane(0, "claude", true),
            agent_pane(1, "codex", false),
            status_pane(2),
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex", "glm"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        let PromoteRelayout::Apply(kdl) = out else {
            panic!("expected Apply, got {out:?}");
        };
        assert!(
            !kdl.contains("\"glm\""),
            "absent roster agent must be omitted:\n{kdl}"
        );
        assert!(kdl.contains("name=\"codex (master)\""));
    }

    #[test]
    fn compose_promote_skips_on_unclassified_live_pane() {
        // A manual shell pane (null terminal_command) in the active tab →
        // Skip the whole relocation (config role change still stands).
        let shell = r#"{"id":3,"is_plugin":false,"is_focused":false,"title":"zsh","terminal_command":null,"tab_id":0}"#.to_string();
        let panes = panes_from(&[
            agent_pane(0, "claude", true),
            agent_pane(1, "codex", false),
            status_pane(2),
            shell,
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        assert_eq!(out, PromoteRelayout::Skip);
    }

    #[test]
    fn compose_promote_skips_on_command_byte_mismatch() {
        // An agent pane whose command doesn't byte-match agent_start_command
        // (here it carries the running foreground process, NOT invoked_with)
        // → Skip (it would be unclassified, and override would close it).
        let mismatch = r#"{"id":1,"is_plugin":false,"is_focused":false,"title":"codex (reviewer)","terminal_command":"codex resume abc-123","tab_id":0}"#.to_string();
        let panes = panes_from(&[agent_pane(0, "claude", true), mismatch, status_pane(2)]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        assert_eq!(out, PromoteRelayout::Skip);
    }

    #[test]
    fn compose_promote_ignores_panes_in_other_tabs() {
        // A pane in a DIFFERENT tab (tab 1) — even an unclassified one —
        // must be ignored by the active-tab filter; relocation proceeds on
        // the caller's tab (tab 0).
        let other_tab_shell = r#"{"id":9,"is_plugin":false,"is_focused":true,"title":"shell","terminal_command":null,"tab_id":1}"#.to_string();
        let panes = panes_from(&[
            agent_pane(0, "claude", true),
            agent_pane(1, "codex", false),
            status_pane(2),
            other_tab_shell,
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        let PromoteRelayout::Apply(kdl) = out else {
            panic!("expected Apply (other-tab pane ignored), got {out:?}");
        };
        assert!(kdl.contains("name=\"codex (master)\""));
    }

    #[test]
    fn compose_promote_skips_when_new_master_has_no_live_pane() {
        // Promote `glm`, which has no live pane → Skip (nothing to make the
        // stage).
        let panes = panes_from(&[
            agent_pane(0, "claude", true),
            agent_pane(1, "codex", false),
            status_pane(2),
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_0"),
            &labels(&["claude", "codex", "glm"]),
            "glm",
            "/a",
            LANDSCAPE,
        );
        assert_eq!(out, PromoteRelayout::Skip);
    }

    #[test]
    fn compose_promote_skips_when_caller_pane_not_found() {
        // The caller pane id isn't among the live panes (no active-tab
        // anchor, no focus target) → Skip.
        let panes = panes_from(&[
            agent_pane(0, "claude", false),
            agent_pane(1, "codex", false),
            status_pane(2),
        ]);
        let out = compose_promote_layout(
            &panes,
            Some("terminal_99"),
            &labels(&["claude", "codex"]),
            "codex",
            "/a",
            LANDSCAPE,
        );
        assert_eq!(out, PromoteRelayout::Skip);
        // Same for an absent caller ref (outside-zellij-ish).
        assert_eq!(
            compose_promote_layout(
                &panes,
                None,
                &labels(&["claude", "codex"]),
                "codex",
                "/a",
                LANDSCAPE,
            ),
            PromoteRelayout::Skip
        );
    }
}
