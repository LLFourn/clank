//! `clank open zellij` — auto-generate a zellij layout (KDL)
//! at `<repo>/.clank/zellij/layout.kdl` and spawn
//! `zellij --layout <path>`. The pane commands pin
//! `--repo <abs-path>` so the spawned session's cwd doesn't
//! affect the resolved repo (codex 361b104 catch).

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::OpenZellijArgs;
use super::{repo_basename, resolve_repo};

/// Whether the environment indicates a Zellij session. This is not a
/// reachability check: only an answered query establishes the live tab state.
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
    // Capture the route once. Both lookup and creation use the current session
    // implicitly, or the same deterministic named session outside Zellij.
    let current = in_session().then(|| SessionTarget::Current {
        name: std::env::var("ZELLIJ_SESSION_NAME").ok(),
    });
    run_with(&mut RealOpenIo, &args, current).await
}

/// [`run`] with its I/O and the caller's session supplied.
///
/// Whether a target is already open is decided by one scan of every
/// session on the machine, by the repo path its panes name: never by a tab
/// or session name, which only place what is opened. A target open
/// anywhere is reported where it is and not opened again.
async fn run_with(
    io: &mut impl OpenIo,
    args: &OpenZellijArgs,
    current: Option<SessionTarget>,
) -> anyhow::Result<()> {
    let source = resolve_repo(args.repo.as_deref())?;
    // --all = the pwd repo AND all its worktrees (`git worktree list`),
    // regardless of which session/worktree we're in. Otherwise one target:
    // --fork/--pr name an existing worktree, bare = the source repo.
    let targets = if args.all {
        worktree_paths(&source)?
    } else if let Some(name) = args.fork.as_deref() {
        vec![fork_path(&source, name)?]
    } else if let Some(pr) = args.pr {
        vec![fork_path(&source, &format!("pr-{pr}"))?]
    } else {
        vec![source.clone()]
    };
    // --print is a layout preview, not a query against the user's server.
    let (targets, hosts) = if args.print {
        (targets, Vec::new())
    } else {
        let scan = io.scan().await;
        require_complete(&scan)?;
        let sorted = sort_targets(&scan, targets, current.as_ref());
        for report in &sorted.reports {
            io.report(report);
        }
        (sorted.missing, sorted.hosts)
    };
    if targets.is_empty() {
        return Ok(());
    }
    if args.all {
        if let Some(current) = &current {
            // Inside a session: add each target's tab to the CURRENT
            // session. `open_one`'s in-session tab spawn is non-blocking,
            // so the loop runs to completion (unlike an outside
            // attach/create per target, which would block on the first —
            // codex 03be1fb).
            open_current_targets(io, current, &targets, args.print).await?;
        } else {
            open_all(io, &source, &targets, &hosts, args.print).await?;
        }
        return Ok(());
    }
    let target = &targets[0];
    let session = current.unwrap_or(SessionTarget::Named(session_name(&repo_basename(target)?)));
    open_one(io, &session, target, args.print, None).await
}

/// `Current` deliberately retains Zellij's implicit in-session routing for
/// both query and creation. The optional name is diagnostic context only; it
/// does not select a different server or authorize a fallback session.
#[derive(Debug)]
enum SessionTarget {
    Current { name: Option<String> },
    Named(String),
}

impl SessionTarget {
    fn description(&self) -> String {
        match self {
            Self::Current { name: Some(name) } => format!("the shell's current session `{name}`"),
            Self::Current { name: None } => "the shell's current session ($ZELLIJ is set)".into(),
            Self::Named(name) => format!("session `{name}`"),
        }
    }

    fn tab_query_argv(&self) -> Vec<String> {
        let mut argv = vec!["zellij".into()];
        if let Self::Named(name) = self {
            argv.extend(["-s".into(), name.clone()]);
        }
        argv.extend(["action".into(), "query-tab-names".into()]);
        argv
    }
}

#[derive(Debug)]
struct QueryOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// A narrow seam for the actual open path. Fixtures exercise lookup, layout
/// writes and spawn decisions together without starting a Zellij server.
trait OpenIo {
    async fn query(&mut self, argv: &[String]) -> anyhow::Result<QueryOutput>;
    fn spawn(&mut self, argv: &[String]) -> anyhow::Result<()>;
    /// Every pane of every zellij session on the machine.
    async fn scan(&mut self) -> SessionScan;
    /// Record that clank owns `repo`'s panes for `labels`.
    fn own(
        &mut self,
        repo: &Path,
        labels: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<()>;
    /// Tell the person where something already is.
    fn report(&mut self, text: &str) {
        println!("{text}");
    }
    fn user_template(&self) -> anyhow::Result<Option<String>> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        user_layout_template(home.as_deref())
    }
    fn term_size(&self) -> (u16, u16) {
        layout_term_size()
    }
}

struct RealOpenIo;

impl OpenIo for RealOpenIo {
    async fn query(&mut self, argv: &[String]) -> anyhow::Result<QueryOutput> {
        run_open_query(argv, std::time::Duration::from_secs(3)).await
    }

    fn spawn(&mut self, argv: &[String]) -> anyhow::Result<()> {
        spawn_zellij(argv)
    }

    async fn scan(&mut self) -> SessionScan {
        panes_in_all_sessions_async(std::time::Duration::from_secs(3)).await
    }

    fn own(
        &mut self,
        repo: &Path,
        labels: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<()> {
        crate::cli::pane_owners::update(repo, labels, &std::collections::BTreeSet::new()).map(drop)
    }
}

/// After a spawn that opened `repo`'s tab: its panes are clank's. A
/// failure to say so is reported, not fatal: the tab is open, and the
/// TUI adopts its roster's panes on its first pass.
fn own_opened(io: &mut impl OpenIo, repo: &Path, master: &str, reviewers: &[String]) {
    let labels = std::iter::once(master.to_string())
        .chain(reviewers.iter().cloned())
        .collect();
    if let Err(e) = io.own(repo, &labels) {
        io.report(&format!(
            "warning: could not record that clank owns {}'s panes: {e:#}",
            repo.display()
        ));
    }
}

fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| crate::shell_quote::shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Explicit-open queries are bounded in time and memory. Drain both streams
/// concurrently with wait, without reader tasks, threads or an idle worker.
/// An oversized stdout is unknown, never a truncated listing authorizing tabs.
async fn run_open_query(
    argv: &[String],
    budget: std::time::Duration,
) -> anyhow::Result<QueryOutput> {
    use tokio::io::AsyncReadExt;
    const STDOUT_CAP: usize = 1024 * 1024;
    const STDERR_CAP: usize = 4096;
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run `{}`", display_argv(argv)))?;
    let mut group = super::wait::GroupKill::over(child.id());
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut tail = super::wait::RingTail::new(STDERR_CAP);
    let result = tokio::time::timeout(budget, async {
        let out = async {
            let mut buf = [0; 8192];
            loop {
                let n = stdout.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let keep = n.min(STDOUT_CAP - bytes.len());
                bytes.extend_from_slice(&buf[..keep]);
                oversized |= keep != n;
            }
            Ok::<_, std::io::Error>(())
        };
        let err = async {
            let mut buf = [0; 8192];
            loop {
                let n = stderr.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                tail.push(&buf[..n]);
            }
            Ok::<_, std::io::Error>(())
        };
        tokio::try_join!(child.wait(), out, err).map(|(status, _, _)| status)
    })
    .await;
    // Includes descendants retaining pipe writers after their parent exits.
    group.terminate();
    if !matches!(result, Ok(Ok(_))) {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), child.wait()).await;
    }
    let stderr = tail.into_string();
    let status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => anyhow::bail!(
            "`{}` failed: {error}; stderr (last {STDERR_CAP} bytes): {stderr}",
            display_argv(argv)
        ),
        Err(_) => anyhow::bail!(
            "`{}` timed out after {budget:?}; stderr (last {STDERR_CAP} bytes): {stderr}",
            display_argv(argv)
        ),
    };
    if oversized {
        anyhow::bail!(
            "`{}` exited {status}, but stdout exceeded {STDOUT_CAP} bytes; listing is unknown; stderr (last {STDERR_CAP} bytes): {stderr}",
            display_argv(argv)
        );
    }
    let stdout = String::from_utf8(bytes).with_context(|| {
        format!(
            "`{}` returned non-UTF-8 stdout; listing is unknown; stderr: {stderr}",
            display_argv(argv)
        )
    })?;
    Ok(QueryOutput {
        status,
        stdout,
        stderr,
    })
}

fn lookup_context(target: &SessionTarget, subject: &str) -> String {
    format!(
        "could not determine {subject} for {}. No tab/session creation was requested after this failed lookup; check this shell's Zellij session/socket access",
        target.description()
    )
}

async fn query_tabs(io: &mut impl OpenIo, target: &SessionTarget) -> anyhow::Result<Vec<String>> {
    let argv = target.tab_query_argv();
    let query = async {
        let out = io.query(&argv).await?;
        if !out.status.success() {
            anyhow::bail!(
                "`{}` exited {}; stderr (last 4096 bytes): {}",
                display_argv(&argv),
                out.status,
                out.stderr
            );
        }
        Ok(out.stdout.lines().map(str::to_string).collect())
    }
    .await;
    query.with_context(|| lookup_context(target, "whether the tab already exists"))
}

/// The `list-sessions -n` listing, for deciding where to create.
async fn query_listing(io: &mut impl OpenIo, target: &SessionTarget) -> anyhow::Result<String> {
    let argv = vec!["zellij".into(), "list-sessions".into(), "-n".into()];
    let query: anyhow::Result<String> = async {
        let out = io.query(&argv).await?;
        interpret_listing(out.status.success(), &out.stdout, &out.stderr).with_context(|| {
            format!(
                "`{}` exited {}; stderr (last 4096 bytes): {}",
                display_argv(&argv),
                out.status,
                out.stderr
            )
        })
    }
    .await;
    query.with_context(|| lookup_context(target, "whether the session exists"))
}

async fn open_current_targets(
    io: &mut impl OpenIo,
    session: &SessionTarget,
    targets: &[PathBuf],
    print: bool,
) -> anyhow::Result<()> {
    let mut tabs = if print {
        Vec::new()
    } else {
        query_tabs(io, session).await?
    };
    for target in targets {
        let tab = open_tab(io, session, target, print, Some(&tabs)).await?;
        // A successfully requested tab is known locally. Reuse this one query
        // throughout --all rather than querying the same session per worktree.
        tabs.push(tab);
    }
    Ok(())
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

/// How `--all` outside a session places the targets that are not open
/// anywhere. Pure / testable.
#[derive(Clone, PartialEq, Debug)]
enum AllPlan {
    /// (Re)create a fresh session with every target tab. `delete_first`
    /// clears a dead same-named session first.
    CreateFresh { delete_first: bool },
    /// Add the tabs to a live session this repo already lives in.
    AddTabs,
}

/// Where `--all` outside a session puts `targets`: the conventional
/// `clank-<repo>` session only when this repo is already open in it (`hosts`
/// names the sessions that hold any of its targets), otherwise a session of
/// its own, never one that belongs to another repo.
fn plan_all(listing: &str, conventional: &str, hosts: &[String]) -> (bool, AllPlan) {
    match decide_session(listing, conventional) {
        SessionPlan::Attach if hosts.iter().any(|h| h == conventional) => (true, AllPlan::AddTabs),
        SessionPlan::Attach => (
            false,
            AllPlan::CreateFresh {
                delete_first: false,
            },
        ),
        SessionPlan::DeleteDeadThenCreate => (true, AllPlan::CreateFresh { delete_first: true }),
        SessionPlan::Create => (
            true,
            AllPlan::CreateFresh {
                delete_first: false,
            },
        ),
    }
}

/// `--all` outside a session: open a tab for every target not open
/// anywhere. Into the `clank-<repo>` session when this repo already lives
/// there (adding tabs, since a plain `attach` would ignore the layout —
/// codex a0c53c9); else a fresh session, under its conventional name unless
/// another repo holds that, then attach. Each tab gets a name no other tab
/// there has. Teamless worktrees are skipped with a notice.
async fn open_all(
    io: &mut impl OpenIo,
    source: &Path,
    targets: &[PathBuf],
    hosts: &[String],
    print: bool,
) -> anyhow::Result<()> {
    let conventional = session_name(&repo_basename(source)?);
    let (name, plan, mut taken) = if print {
        (
            conventional,
            AllPlan::CreateFresh {
                delete_first: false,
            },
            Vec::new(),
        )
    } else {
        let listing = query_listing(io, &SessionTarget::Named(conventional.clone())).await?;
        match plan_all(&listing, &conventional, hosts) {
            (true, AllPlan::AddTabs) => {
                let tabs = query_tabs(io, &SessionTarget::Named(conventional.clone())).await?;
                (conventional, AllPlan::AddTabs, tabs)
            }
            (true, plan) => (conventional, plan, Vec::new()),
            (false, plan) => (unused_session_name(source, &listing)?, plan, Vec::new()),
        }
    };
    let mut named = Vec::new();
    for target in targets {
        let tab = unused_tab_name(target, &taken)?;
        taken.push(tab.clone());
        named.push((target.clone(), tab));
    }
    let tabs = resolve_tabs(&named)?;
    if tabs.is_empty() {
        anyhow::bail!("no openable worktrees (none have clank agents configured)");
    }

    let (rows, cols) = io.term_size();
    let kdl = compose_multitab(&tabs, (cols, rows))?;
    let layout_path = layout_file_path(source);
    let (pre, spawn) = match plan {
        AllPlan::CreateFresh { delete_first } => (
            delete_first.then(|| delete_argv(&name)),
            create_argv(&layout_path, &name),
        ),
        AllPlan::AddTabs => (None, add_tabs_argv(&layout_path, &name)),
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
        let _ = io.spawn(pre);
    }
    io.spawn(&spawn)?;
    for tab in &tabs {
        own_opened(io, Path::new(&tab.repo_path), &tab.master, &tab.reviewers);
    }
    Ok(())
}

/// Resolve target worktrees, each with the tab name it is given, to tab
/// specs, skipping teamless ones with a notice.
fn resolve_tabs(targets: &[(PathBuf, String)]) -> anyhow::Result<Vec<TabSpec>> {
    let mut tabs = Vec::new();
    for (t, name) in targets {
        let Some(set) = crate::agent_store::try_resolve_via_team(t)? else {
            eprintln!("skipping `{}` (no agents configured)", t.display());
            continue;
        };
        let reviewers = set
            .members
            .iter()
            .map(|r| r.label.as_str().to_string())
            .collect();
        tabs.push(TabSpec {
            name: name.clone(),
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
             above. This result alone does not establish whether a tab/session \
             was created; inspect the selected session before retrying."
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
    with_pane_keys(&s)
}

/// Open the agent workspace for one repo/worktree that is not open
/// anywhere: a tab in the current session, or a session of its own outside
/// one. A tab or session already using the conventional name is another
/// repo's, so the target gets a name nothing there has.
async fn open_one(
    io: &mut impl OpenIo,
    target: &SessionTarget,
    repo: &Path,
    print: bool,
    known_tabs: Option<&[String]>,
) -> anyhow::Result<()> {
    open_tab(io, target, repo, print, known_tabs)
        .await
        .map(drop)
}

/// [`open_one`], saying the name of the tab it opened.
async fn open_tab(
    io: &mut impl OpenIo,
    target: &SessionTarget,
    repo: &Path,
    print: bool,
    known_tabs: Option<&[String]>,
) -> anyhow::Result<String> {
    let repo = repo.to_path_buf();
    let basename = repo_basename(&repo)?;
    // Registration is the resolved roster
    // (`teams-based-agent-registration`): exactly one master plus
    // its reviewers, no role-triage needed.
    let Some(set) = crate::agent_store::try_resolve_via_team(&repo)? else {
        anyhow::bail!(
            "this repo has no agents configured. Run `clank agent add <name>` + \
             `clank agent make-master <name>` to build a roster, or `clank init --team <name>` \
             to seed one from a template."
        );
    };
    let master_label = set.master.as_str().to_string();
    let reviewer_labels: Vec<String> = set
        .members
        .iter()
        .map(|r| r.label.as_str().to_string())
        .collect();
    let repo_path_str = repo.display().to_string();
    let layout_path = layout_file_path(&repo);
    // --print is a layout preview, not a query against the user's server.
    let (tab, pre_argv, spawn_argv) = match target {
        SessionTarget::Current { .. } => {
            let tab = if print {
                basename.clone()
            } else {
                let queried;
                let tabs = match known_tabs {
                    Some(tabs) => tabs,
                    None => {
                        queried = query_tabs(io, target).await?;
                        &queried
                    }
                };
                unused_tab_name(&repo, tabs)?
            };
            (tab, None, in_session_tab_argv(&layout_path))
        }
        SessionTarget::Named(name) => {
            let listing = if print {
                String::new()
            } else {
                query_listing(io, target).await?
            };
            let (pre, name) = match decide_session(&listing, name) {
                SessionPlan::Create => (None, name.clone()),
                SessionPlan::DeleteDeadThenCreate => (Some(delete_argv(name)), name.clone()),
                // Live, and not this repo's (the scan found it nowhere):
                // another repo's session, never attached to.
                SessionPlan::Attach => (None, unused_session_name(&repo, &listing)?),
            };
            (basename.clone(), pre, create_argv(&layout_path, &name))
        }
    };
    // User-authored layout chrome from `~/.clank/config.json`
    // (`zellij-layout-config-around-agent-panes`); None → the
    // built-in template.
    let user_template = io.user_template()?;
    // Orientation from the real WINDOW's dimensions — the client
    // tty when inside a session, the spawning terminal otherwise
    // (zellij-default-layout, zellij-in-session-orientation);
    // measurement stays at the shell, compose stays pure over
    // (cols, rows).
    let (rows, cols) = io.term_size();
    let kdl = compose_kdl(
        &tab,
        &repo_path_str,
        &master_label,
        &reviewer_labels,
        user_template.as_deref(),
        Orientation::detect((cols, rows)),
    )?;

    if print {
        println!("{kdl}");
        if let Some(pre) = &pre_argv {
            eprintln!("pre-spawn: {}", pre.join(" "));
        }
        eprintln!("spawn: {}", spawn_argv.join(" "));
        return Ok(tab);
    }

    write_layout_file(&repo, &kdl)?;

    if let Some(pre) = &pre_argv {
        // Best-effort: a failed delete of a dead session just means
        // the create below errors visibly.
        let _ = io.spawn(pre);
    }
    io.spawn(&spawn_argv)?;
    own_opened(io, &repo, &master_label, &reviewer_labels);
    Ok(tab)
}

/// Where a repo is already open: one tab of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Location {
    session: String,
    tab_id: Option<u32>,
    /// The tab's place in the tab bar, from 0, as zellij reports it: what
    /// a person counts, unlike `tab_id`.
    position: Option<usize>,
    tab_name: String,
    /// The agents with live panes there.
    agents: Vec<String>,
}

/// Whether `pane` is one of `repo`'s clank panes: an agent's, with the
/// label it runs (`Some(Some(label))`), or its status pane, in its current
/// or legacy spelling (`Some(None)`). A label or a path may itself hold
/// ` --repo `, so every place it could end the label is tried, and taken
/// only where the label is a valid agent label and the path after it is
/// `repo`.
fn pane_for<'a>(pane: &'a ZellijPane, repo: &Path) -> Option<Option<&'a str>> {
    if pane.is_plugin {
        return None;
    }
    let command = pane.terminal_command.as_deref()?;
    const REPO: &str = " --repo ";
    if let Some(rest) = command.strip_prefix("clank agent start ") {
        return rest
            .match_indices(REPO)
            .map(|(at, _)| (&rest[..at], &rest[at + REPO.len()..]))
            .find(|(label, path)| {
                clank_core::ids::AgentLabel::parse(label).is_ok() && is_repo(path, repo)
            })
            .map(|(label, _)| Some(label));
    }
    let path = command.strip_prefix("clank tui --repo ").or_else(|| {
        command
            .strip_prefix("clank status --repo ")?
            .strip_suffix(" --tui")
    })?;
    is_repo(path, repo).then_some(None)
}

/// Whether `path`, as a pane's command names it, is `repo`: compared by
/// canonical path, so a repo of the same name elsewhere is not it.
fn is_repo(path: &str, repo: &Path) -> bool {
    let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(Path::new(path)) == canonical(repo)
}

/// Every place `repo` is open in `scan`: the tabs holding a live pane of
/// it, by any agent label (on the roster or not) or its status pane.
fn locations(scan: &SessionScan, repo: &Path) -> Vec<Location> {
    let mut found: Vec<Location> = Vec::new();
    for sp in &scan.panes {
        let pane = &sp.pane;
        let Some(agent) = pane_for(pane, repo) else {
            continue;
        };
        if pane.exited {
            continue;
        }
        let at = match found
            .iter()
            .position(|l| l.session == sp.session && l.tab_id == pane.tab_id)
        {
            Some(at) => at,
            None => {
                found.push(Location {
                    session: sp.session.clone(),
                    tab_id: pane.tab_id,
                    position: pane.tab_position,
                    tab_name: crate::cli::status_tui::strip_leading_emoji(&pane.tab_name)
                        .to_string(),
                    agents: Vec::new(),
                });
                found.len() - 1
            }
        };
        if let Some(agent) = agent
            && !found[at].agents.iter().any(|a| a == agent)
        {
            found[at].agents.push(agent.to_string());
        }
    }
    found
}

/// An invocation's targets, sorted by one scan.
#[derive(Debug, PartialEq)]
struct Sorted {
    /// Open nowhere: to be opened.
    missing: Vec<PathBuf>,
    /// Where each open target is, and how to get there.
    reports: Vec<String>,
    /// The sessions holding any open target.
    hosts: Vec<String>,
}

fn sort_targets(
    scan: &SessionScan,
    targets: Vec<PathBuf>,
    caller: Option<&SessionTarget>,
) -> Sorted {
    let here = match caller {
        Some(SessionTarget::Current { name }) => name.as_deref(),
        _ => None,
    };
    let mut sorted = Sorted {
        missing: Vec::new(),
        reports: Vec::new(),
        hosts: Vec::new(),
    };
    for target in targets {
        let found = locations(scan, &target);
        if found.is_empty() {
            sorted.missing.push(target);
            continue;
        }
        sorted
            .reports
            .push(describe(&target, &found, caller.is_some(), here));
        sorted.hosts.extend(found.into_iter().map(|l| l.session));
    }
    sorted
}

/// Refuse to decide from a scan that did not see every session: an unseen
/// session may hold the repo, and opening it again is the duplicate this
/// exists to prevent.
fn require_complete(scan: &SessionScan) -> anyhow::Result<()> {
    if scan.listing_failed {
        anyhow::bail!(
            "could not list zellij's sessions, so nothing is known about whether this repo \
             is already open. Nothing was opened; check that `zellij` runs, then retry."
        );
    }
    if !scan.unanswered.is_empty() {
        anyhow::bail!(
            "zellij session(s) {} did not answer in time, so this repo may already be open \
             there. Nothing was opened; retry, or close the unresponsive session(s).",
            scan.unanswered
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Where `repo` is open, and how to get there from the caller: outside
/// zellij (`here` is `None`), inside the session it is in, or inside
/// another (`here` names the caller's session, when known).
fn describe(repo: &Path, found: &[Location], inside: bool, here: Option<&str>) -> String {
    let mut out = format!("`{}` is already open:", repo.display());
    for l in found {
        let tab = match l.position {
            Some(p) => format!("tab {} `{}`", p + 1, l.tab_name),
            None => format!("tab `{}`", l.tab_name),
        };
        let agents = match l.agents.as_slice() {
            [] => String::new(),
            agents => format!(" (agents: {})", agents.join(", ")),
        };
        let attach = format!(
            "zellij attach {}",
            crate::shell_quote::shell_quote(&l.session)
        );
        let way = match (inside, here) {
            (false, _) => format!("run `{attach}`, then go to {tab}"),
            (true, Some(here)) if here == l.session => format!("switch to {tab} in this session"),
            (true, _) => format!(
                "this is another session: switch to it with zellij's session manager \
                 (Ctrl+o w), or detach and run `{attach}`, then go to {tab}"
            ),
        };
        out.push_str(&format!(
            "\n  session `{}`, {tab}{agents}: {way}",
            l.session
        ));
    }
    out
}

/// The first of `variants` (0, 1, 2, …) that `taken` does not hold.
fn first_untaken(variants: impl Fn(usize) -> String, taken: impl Fn(&str) -> bool) -> String {
    (0..)
        .map(variants)
        .find(|name| !taken(name))
        .expect("an unbounded sequence has an untaken name")
}

/// The name of `repo`'s directory's parent, to tell same-named repos apart.
fn parent_name(repo: &Path) -> Option<String> {
    repo.parent()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// A tab name for `repo` that no tab in `tabs` has, glyphs aside.
fn unused_tab_name(repo: &Path, tabs: &[String]) -> anyhow::Result<String> {
    let basename = repo_basename(repo)?;
    let parent = parent_name(repo).unwrap_or_default();
    Ok(first_untaken(
        |n| match n {
            0 => basename.clone(),
            1 => format!("{basename} ({parent})"),
            n => format!("{basename} ({parent}) {n}"),
        },
        |name| {
            tabs.iter()
                .any(|t| crate::cli::status_tui::strip_leading_emoji(t) == name)
        },
    ))
}

/// A session name for `repo` that no session in `listing` (live or not)
/// has: the conventional `clank-<basename>` unless another holds it.
fn unused_session_name(repo: &Path, listing: &str) -> anyhow::Result<String> {
    let basename = repo_basename(repo)?;
    let parent = parent_name(repo).unwrap_or_default();
    let names: Vec<&str> = listed_sessions(listing).map(ListedSession::name).collect();
    Ok(first_untaken(
        |n| {
            session_name(&match n {
                0 => basename.clone(),
                1 => format!("{basename}-{parent}"),
                n => format!("{basename}-{parent}-{n}"),
            })
        },
        |name| names.contains(&name),
    ))
}

/// One `list-sessions -n` line, read. zellij prints the name, then the
/// `[Created …]` frame it appends, then the state; so the frame is the last
/// ` [Created ` on the line, the name is everything before it (kept whole,
/// whatever it contains), and the state is read only from after the frame's
/// `]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListedSession<'a> {
    Live(&'a str),
    Exited(&'a str),
    /// A line that could not be read: a session whose name and state are
    /// unknown, never an absent one.
    Unreadable(&'a str),
}

impl<'a> ListedSession<'a> {
    fn name(self) -> &'a str {
        match self {
            ListedSession::Live(n) | ListedSession::Exited(n) | ListedSession::Unreadable(n) => n,
        }
    }
}

fn listed_sessions(listing: &str) -> impl Iterator<Item = ListedSession<'_>> {
    listing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let framed = line.rfind(" [Created ").and_then(|at| {
                let name = line[..at].trim();
                let state = &line[at..][line[at..].find(']')? + 1..];
                (!name.is_empty()).then_some((name, state))
            });
            match framed {
                Some((name, state)) if state.contains("(EXITED") => ListedSession::Exited(name),
                Some((name, _)) => ListedSession::Live(name),
                None => ListedSession::Unreadable(line.trim()),
            }
        })
}

/// `zellij list-sessions -n` stdout, or `None` when the command could
/// not run or returned an unrecognized failure.
///
/// A non-zero exit is an answer only for the recognized no-sessions
/// response. Other exits and spawn failures mean nothing was learned.
fn zellij_list_sessions_checked() -> Option<String> {
    let out = std::process::Command::new("zellij")
        .args(["list-sessions", "-n"])
        .output()
        .ok()?;
    interpret_listing(
        out.status.success(),
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    )
}

/// What zellij says when there are none — verbatim from the binary.
/// It exits non-zero to say it, which is why a bare status check
/// cannot tell that answer from a failure.
const NO_SESSIONS: &str = "No active zellij sessions found.";

/// One `list-sessions` run, read strictly. `None` means NOTHING was
/// learned; `Some("")` means zellij answered that there are none.
///
/// Both arrive as a non-zero exit with no session names, so a reader
/// that only checks the status folds "the command broke" into "the
/// repo has no sessions" — which for `clank nudge` is the difference
/// between admitting it does not know and asserting an agent is absent.
fn interpret_listing(ok: bool, stdout: &str, stderr: &str) -> Option<String> {
    if ok {
        return Some(stdout.to_string());
    }
    // Match the answer, not a fragment of an otherwise failed diagnostic.
    let out = stdout.trim();
    let err = stderr.trim();
    if (out == NO_SESSIONS && (err.is_empty() || err == NO_SESSIONS))
        || (err == NO_SESSIONS && out.is_empty())
    {
        return Some(String::new());
    }
    None
}

fn layout_file_path(repo: &Path) -> PathBuf {
    repo.join(".clank/zellij/layout.kdl")
}

/// The user's layout chrome from `~/.clank/config.json#/zellij/layout`
/// (`zellij-layout-config-around-agent-panes`); `None` → the built-in.
/// The ONE source for both `clank open` and the live re-layout, so a
/// workspace opened from a marker template is reconciled with it.
pub(crate) fn user_layout_template(home: Option<&Path>) -> anyhow::Result<Option<String>> {
    Ok(home
        .map(crate::cli::team::read_user_config)
        .transpose()?
        .and_then(|cfg| cfg.zellij.and_then(|z| z.layout)))
}

/// The tab layout for the roster AS IT IS RUNNING, for
/// `override-layout` on a live tab: the same composition `clank open`
/// launches from, so every slot's command byte-matches a live pane's
/// `invoked_with`, and the same swap variants, so alt+[ / alt+] apply
/// the current roster at the other orientation
/// (placement-is-a-layout-applied-not-panes-shuffled).
///
/// `reviewers` must be the reviewers with a RUNNING pane, not the
/// roster's: a command slot with no live match makes zellij spawn a
/// second stage pane and dump every existing pane into the stack
/// (measured on 0.45.0).
pub(crate) fn compose_live_layout(
    repo: &Path,
    home: Option<&Path>,
    master: &str,
    reviewers: &[String],
    orientation: Orientation,
) -> anyhow::Result<String> {
    let basename = repo_basename(repo)?;
    let template = user_layout_template(home)?;
    compose_kdl(
        &basename,
        &repo.to_string_lossy(),
        master,
        reviewers,
        template.as_deref(),
        orientation,
    )
}

/// Apply `kdl` to the tab `tab_id` of the current session, in place.
///
/// `override-layout` acts on the ACTIVE tab and, without
/// `--apply-only-to-active-tab`, reshapes the whole session to the
/// layout's tabs — it CLOSED the other tab when measured. So the tab
/// is made active by its stable id (names may repeat across
/// worktrees), the override is scoped to it, and the previously active
/// tab is restored. Existing terminal panes the layout does not name —
/// a shell the user opened, a reviewer whose process exited — are
/// retained rather than closed. Writes the layout file `clank open`
/// uses, so the file on disk tracks the roster too. Returns whether the
/// override itself was accepted; the caller reads placement back.
pub(crate) fn override_tab_layout(repo: &Path, tab_id: u32, kdl: &str) -> bool {
    let Ok(path) = write_layout_file(repo, kdl) else {
        return false;
    };
    let path = path.display().to_string();
    override_transaction(
        tab_id,
        &path,
        |args| zellij_action(args).is_some(),
        active_tab_id,
    )
}

/// The override as a CHECKED transaction over injected zellij actions,
/// so every decision — not just the argv — is assertable without a
/// spawn (codex on d758045). `override-layout` acts on whatever tab is
/// active, so nothing here is fail-open: an active tab that cannot be
/// identified refuses the override; the by-id focus must succeed AND
/// read back as the target before the override is issued; and once
/// the focus moved, the previous tab is restored whether or not the
/// override was accepted.
fn override_transaction(
    tab_id: u32,
    path: &str,
    mut run: impl FnMut(&[&str]) -> bool,
    mut active: impl FnMut() -> Option<u32>,
) -> bool {
    let Some(was_active) = active() else {
        return false;
    };
    let target = tab_id.to_string();
    let moved = was_active != tab_id;
    if moved {
        if !run(&["go-to-tab-by-id", &target]) {
            return false;
        }
        if active() != Some(tab_id) {
            run(&["go-to-tab-by-id", &was_active.to_string()]);
            return false;
        }
    }
    let applied = run(&override_layout_argv(path));
    if moved {
        run(&["go-to-tab-by-id", &was_active.to_string()]);
    }
    applied
}

/// Every flag is a measured necessity: without
/// `--apply-only-to-active-tab` the override reshaped the SESSION and
/// closed another tab; without the retain flags an unmatched pane (a
/// user's shell, an exited reviewer) is closed.
fn override_layout_argv(path: &str) -> [&str; 5] {
    [
        "override-layout",
        path,
        "--apply-only-to-active-tab",
        "--retain-existing-terminal-panes",
        "--retain-existing-plugin-panes",
    ]
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
    orientation: Orientation,
) -> anyhow::Result<String> {
    let agents = agent_group_kdl(
        repo_path,
        master,
        ReviewerStack::Roster(reviewers),
        orientation,
    );
    let composed = match user_template {
        // User templates: unchanged marker contract — the detected
        // orientation's group, NO swap blocks (swap_tiled_layout is
        // a layout-root construct; splicing it into arbitrary user
        // templates is fragile — template authors write their own
        // swaps).
        Some(t) => substitute_marker(t, &agents)?,
        None => {
            let built_in = BUILT_IN_TEMPLATE.replace("__TAB__", &kdl_escape(tab_name));
            let base = substitute_marker(&built_in, &agents)?;
            // BOTH orientations ship as swap variants so alt+[ /
            // alt+] flips the arrangement at runtime.
            add_swap_variants(&base, repo_path, master)?
        }
    };
    with_pane_keys(&composed)
}

/// Keys zellij binds to pane focus by default (beside `Alt h/j/k/l`)
/// that the programs in clank's panes need for themselves: codex
/// edits its previous message on Alt+Up.
const PANE_KEYS: [&str; 4] = ["Alt Up", "Alt Down", "Alt Left", "Alt Right"];

/// The layout with its first root `keybinds` node unbinding
/// [`PANE_KEYS`] in its first global `unbind`, creating either when
/// absent and leaving everything else as written. zellij reads only
/// that first node and that first `unbind` (zellij-utils
/// `kdl/mod.rs:5002` and `:4738` at ff0925f), so a second block would
/// be ignored for a template carrying its own; and the global unbind
/// is applied after every bind block, so nothing else in the node can
/// rebind the keys (codex on 5262fa7). The result is re-parsed before
/// it is handed back: a layout zellij cannot read must fail here, not
/// at `clank open`.
fn with_pane_keys(kdl: &str) -> anyhow::Result<String> {
    let mut doc: kdl::KdlDocument = kdl
        .parse()
        .map_err(|e: kdl::KdlError| anyhow::anyhow!("composed layout is not valid KDL: {e}"))?;
    ensure_pane_keys(&mut doc);
    let out = doc.to_string();
    out.parse::<kdl::KdlDocument>().map_err(|e| {
        anyhow::anyhow!("layout is not valid KDL after merging the pane keys: {e}\n{out}")
    })?;
    Ok(out)
}

fn ensure_pane_keys(doc: &mut kdl::KdlDocument) {
    let nodes = doc.nodes_mut();
    let keybinds = match nodes.iter().position(|n| n.name().value() == "keybinds") {
        Some(i) => &mut nodes[i],
        None => {
            // Parsed whole: kdl keeps the whitespace a node was parsed
            // with and gives a built one none. A template's last node
            // may end at EOF, or with `;`, with no newline of its own
            // for this one to follow (codex on 9081ceb).
            let fresh: kdl::KdlDocument =
                "keybinds {\n    unbind\n}\n".parse().expect("literal KDL");
            let mut node = fresh.nodes()[0].clone();
            let ended = nodes
                .last()
                .and_then(|n| n.trailing())
                .is_some_and(|t| t.ends_with('\n'));
            if !nodes.is_empty() && !ended {
                node.set_leading("\n");
            }
            nodes.push(node);
            nodes.last_mut().expect("just pushed")
        }
    };
    let children = keybinds
        .children_mut()
        .get_or_insert_with(|| {
            let mut block = kdl::KdlDocument::new();
            block.set_leading("");
            block.set_trailing("");
            block
        })
        .nodes_mut();
    let unbind = match children.iter().position(|n| n.name().value() == "unbind") {
        Some(i) => &mut children[i],
        None => {
            // The node ends itself: whatever follows — a child on the
            // next line, one on the same line, or the closing brace
            // — owes it no newline (codex on 49fc3d8). The child it
            // displaces from the front gives up the one newline it
            // opened with, so the pretty case stays pretty.
            let mut fresh = kdl::KdlNode::new("unbind");
            fresh.set_leading("\n    ");
            fresh.set_trailing("\n");
            if let Some(next) = children.first_mut()
                && let Some(rest) = next.leading().and_then(|l| l.strip_prefix('\n'))
            {
                next.set_leading(rest.to_string());
            }
            children.insert(0, fresh);
            &mut children[0]
        }
    };
    for key in PANE_KEYS {
        let present = unbind
            .entries()
            .iter()
            .any(|e| e.name().is_none() && e.value().as_string() == Some(key));
        if !present {
            unbind.push(kdl::KdlEntry::new(key));
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
pub(crate) enum Orientation {
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
/// tui` instrument pane sits beside/below the stack
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
    push_agent_pane(&mut out, "    ", " size=\"65%\"", master, repo_path);
    // The side region: reviewer stack + instrument pane.
    out.push_str(&format!(
        "    pane size=\"35%\" split_direction=\"{inner}\" {{\n"
    ));
    match stack {
        ReviewerStack::Roster([]) => {}
        ReviewerStack::Roster(reviewers) => {
            out.push_str("        pane stacked=true {\n");
            for reviewer in reviewers {
                push_agent_pane(&mut out, "            ", "", reviewer.as_str(), repo_path);
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
    // An empty name is an empty title; no name at all titles the
    // pane with its whole command line (measured on zellij 0.46.0).
    out.push_str(&format!(
        "        pane {tui_size} name=\"\" cwd=\"{repo_esc}\" {{\n"
    ));
    out.push_str("            command \"clank\"\n");
    out.push_str(&format!(
        "            args \"tui\" \"--repo\" \"{repo_esc}\"\n"
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

/// The pane name for an agent: its label, and nothing else. The name is
/// the pane's identity ([`resolve_title`]), so a role in it would go
/// stale the moment the role changed. The single source of the format
/// for every writer: the layout, `new-pane`, `clank as` and the TUI's
/// glyph retitle.
pub(crate) fn agent_pane_title(label: &str) -> String {
    label.to_string()
}

/// The glyphs the TUI writes before an agent's pane name. Closed, so a
/// title is read as `<glyph> <label>` only for these.
pub(crate) const AGENT_PANE_GLYPHS: [&str; 3] = ["🔨", "💤", "👀"];

/// The role words an old `<label> (<role>)` title may carry: the resolved
/// roles earlier builds wrote, and the roster's tier words beside them.
const OLD_TITLE_ROLES: [&str; 7] = [
    "master", "reviewer", "standby", "commit", "plan", "final", "gate",
];

/// What a pane's title says about which agent it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneIdentity {
    Agent(String),
    /// The title reads as two or more distinct candidates. Refused, never
    /// guessed: neither present nor absent.
    Ambiguous(std::collections::BTreeSet<String>),
    Unknown,
}

/// Every label `title` can be read as: the title itself, `<glyph>
/// <label>`, and the old `<label> (<role>)` with or without a glyph.
fn title_readings(title: &str) -> Vec<&str> {
    let mut bare = vec![title];
    bare.extend(
        AGENT_PANE_GLYPHS
            .iter()
            .filter_map(|g| title.strip_prefix(g)?.strip_prefix(' ')),
    );
    let mut out = bare.clone();
    for t in bare {
        out.extend(
            OLD_TITLE_ROLES
                .iter()
                .filter_map(|role| t.strip_suffix(')')?.strip_suffix(role)?.strip_suffix(" (")),
        );
    }
    out
}

/// The one reading of a pane title as an agent: every reading that is a
/// candidate is collected before choosing, so a literal label is never
/// preferred over another label's decorated title.
pub(crate) fn resolve_title(
    title: &str,
    candidates: &std::collections::BTreeSet<String>,
) -> PaneIdentity {
    let mut found: std::collections::BTreeSet<String> = title_readings(title)
        .into_iter()
        .filter(|r| candidates.contains(*r))
        .map(str::to_string)
        .collect();
    match found.len() {
        0 => PaneIdentity::Unknown,
        1 => PaneIdentity::Agent(found.pop_first().expect("one")),
        _ => PaneIdentity::Ambiguous(found),
    }
}

/// A repo's workspace: one tab of one session, established by the repo's
/// TUI pane (or, for the TUI itself, its own pane), never by the agent
/// names it scopes. `candidates` are the labels a pane in it may be
/// identified as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    tab: u32,
    candidates: std::collections::BTreeSet<String>,
}

impl Workspace {
    /// The workspace of a listing that holds exactly one of `repo`'s TUI
    /// panes. `None` for none, several, or one whose tab is not listed.
    pub(crate) fn anchored(
        panes: &[ZellijPane],
        repo: &Path,
        candidates: std::collections::BTreeSet<String>,
    ) -> Option<Self> {
        Some(Self {
            tab: repo_tab_id(panes, repo)?,
            candidates,
        })
    }

    /// The TUI's workspace: the tab holding its own pane `caller` when
    /// the listing names it, which needs no anchor, else
    /// [`Self::anchored`].
    pub(crate) fn local(
        panes: &[ZellijPane],
        repo: &Path,
        caller: Option<&str>,
        candidates: std::collections::BTreeSet<String>,
    ) -> Option<Self> {
        match caller.and_then(|c| owned_tab(panes, c)) {
            Some(tab) => Some(Self { tab, candidates }),
            None => Self::anchored(panes, repo, candidates),
        }
    }

    pub(crate) fn tab(&self) -> u32 {
        self.tab
    }

    /// What `pane` is in this workspace: `Unknown` outside its tab and for
    /// plugin panes.
    ///
    /// EXIT STATE IS NOT CONSULTED, and every reader shares that rule. A
    /// crashed agent's pane is still that agent's pane, so the reconciler
    /// leaves it standing rather than opening a replacement: respawn is
    /// MANUAL. Auto-respawn would relaunch a tool that just failed, and
    /// one that refuses a second session straight back into that error on
    /// every pass. What must not follow is a corpse outliving a live
    /// duplicate, which is why [`remove_target_ids`] closes exited copies
    /// first.
    pub(crate) fn identify(&self, pane: &ZellijPane) -> PaneIdentity {
        if pane.is_plugin || pane.tab_id != Some(self.tab) {
            return PaneIdentity::Unknown;
        }
        resolve_title(&pane.title, &self.candidates)
    }

    /// The agent `pane` is, if exactly one. `None` is NOT absence: an
    /// ambiguous pane is `None` too, so this only serves projections whose
    /// callers are handed [`ambiguities`] beside them. A question about one
    /// label is [`Self::lookup`].
    fn agent_of(&self, pane: &ZellijPane) -> Option<String> {
        match self.identify(pane) {
            PaneIdentity::Agent(label) => Some(label),
            _ => None,
        }
    }

    fn ambiguity(&self, pane: &ZellijPane) -> Option<Ambiguity> {
        match self.identify(pane) {
            PaneIdentity::Ambiguous(candidates) => Some(Ambiguity {
                pane: pane.pane_id(),
                title: pane.title.clone(),
                candidates,
            }),
            _ => None,
        }
    }

    /// `label`'s panes among `panes`, or the panes that could be its and
    /// another's. Uncertain whenever any pane reads as `label` and another
    /// candidate, even beside one that is plainly `label`'s: that pane may
    /// be a second copy.
    pub(crate) fn lookup<'a, P: AsPane + 'a>(
        &self,
        panes: impl IntoIterator<Item = &'a P>,
        label: &str,
    ) -> Lookup<'a, P> {
        let mut found = Vec::new();
        let mut doubts = Vec::new();
        for item in panes {
            let pane = item.as_pane();
            match self.identify(pane) {
                PaneIdentity::Agent(l) if l == label => found.push(item),
                PaneIdentity::Ambiguous(c) if c.contains(label) => doubts.push(Ambiguity {
                    pane: pane.pane_id(),
                    title: pane.title.clone(),
                    candidates: c,
                }),
                _ => {}
            }
        }
        if doubts.is_empty() {
            Lookup::Known(found)
        } else {
            Lookup::Uncertain(doubts)
        }
    }
}

/// A pane whose title reads as two or more candidates: it may be any of
/// them, so it is evidence neither for nor against one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ambiguity {
    pub(crate) pane: String,
    pub(crate) title: String,
    pub(crate) candidates: std::collections::BTreeSet<String>,
}

impl std::fmt::Display for Ambiguity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.candidates.iter().map(|c| format!("`{c}`")).collect();
        write!(
            f,
            "pane {} is titled `{}`, which reads as {}",
            self.pane,
            self.title,
            names.join(" or ")
        )
    }
}

/// Every ambiguity, one sentence, for a refusal.
pub(crate) fn describe_ambiguities(all: &[Ambiguity]) -> String {
    all.iter()
        .map(Ambiguity::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a listing says about one label's panes.
#[derive(Debug)]
pub(crate) enum Lookup<'a, P> {
    /// Every pane that is the label's, possibly none, and no pane that
    /// might also be.
    Known(Vec<&'a P>),
    /// Some pane may or may not be the label's: neither absence nor a
    /// unique match follows.
    Uncertain(Vec<Ambiguity>),
}

/// One listed pane, from a session's own listing or a scan of them all.
pub(crate) trait AsPane {
    fn as_pane(&self) -> &ZellijPane;
}

impl AsPane for ZellijPane {
    fn as_pane(&self) -> &ZellijPane {
        self
    }
}

impl AsPane for SessionPane {
    fn as_pane(&self) -> &ZellijPane {
        &self.pane
    }
}

/// Why a scan of every session names no workspace for a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unresolved {
    ListingFailed,
    /// These sessions did not answer; any could hold the repo's tab.
    Unanswered(Vec<String>),
    NoAnchor,
    /// `(session, tab name)` of each TUI pane found.
    ManyAnchors(Vec<(String, String)>),
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListingFailed => f.write_str("zellij's sessions could not be listed"),
            Self::Unanswered(s) => write!(
                f,
                "zellij session(s) {} did not answer, and any of them could hold this \
                 repo's tab",
                s.join(", ")
            ),
            Self::NoAnchor => f.write_str("no session shows this repo's TUI pane"),
            Self::ManyAnchors(at) => write!(
                f,
                "this repo's TUI pane is open in {} places ({})",
                at.len(),
                at.iter()
                    .map(|(s, t)| format!("session `{s}`, tab `{t}`"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

/// The session and workspace `repo` has across every session `scan` saw.
/// Uniqueness is only known from a complete scan: one anchor beside a
/// session that did not answer is not unique.
pub(crate) fn scan_workspace(
    scan: &SessionScan,
    repo: &Path,
    candidates: std::collections::BTreeSet<String>,
) -> Result<(String, Workspace), Unresolved> {
    if scan.listing_failed {
        return Err(Unresolved::ListingFailed);
    }
    if !scan.unanswered.is_empty() {
        return Err(Unresolved::Unanswered(scan.unanswered.clone()));
    }
    let anchors: Vec<&SessionPane> = scan
        .panes
        .iter()
        .filter(|sp| is_anchor(&sp.pane, repo))
        .collect();
    match anchors.as_slice() {
        [] => Err(Unresolved::NoAnchor),
        [one] => match one.pane.tab_id {
            Some(tab) => Ok((one.session.clone(), Workspace { tab, candidates })),
            None => Err(Unresolved::NoAnchor),
        },
        many => Err(Unresolved::ManyAnchors(
            many.iter()
                .map(|sp| (sp.session.clone(), sp.pane.tab_name.clone()))
                .collect(),
        )),
    }
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
    repo_path: &str,
) {
    let name_esc = kdl_escape(&agent_pane_title(label));
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
/// <repo>`. What a pane runs, not who it is: identity is the pane's name
/// ([`Workspace::identify`]).
#[cfg(test)]
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
    /// Which tab the pane lives in — `list-panes --json` spans ALL tabs,
    /// so this scopes a relocation to the caller's active tab.
    #[serde(default)]
    tab_id: Option<u32>,
    /// Focused WITHIN its tab. Every tab reports one, so this alone
    /// cannot name the user's focus — paired with [`active_tab_id`] it
    /// can. Default false when absent, which costs a focus restore
    /// rather than a wrong one.
    #[serde(default)]
    is_focused: bool,
    /// The pane's process has ended (zellij holds the pane open with
    /// its exit code). Deliberately NOT part of identifying a pane —
    /// see [`Workspace::identify`] — and used only to choose WHICH copy
    /// to close in [`remove_target_ids`].
    #[serde(default)]
    exited: bool,
    /// The pane's name, which is the agent's identity.
    #[serde(default)]
    title: String,
    /// Present only with `--tab`; empty otherwise.
    #[serde(default)]
    tab_name: String,
    /// The tab's place in the tab bar, from 0. Present only with `--tab`.
    #[serde(default)]
    tab_position: Option<usize>,
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
    /// The tab this pane lives in, `0` when the listing did not say.
    /// Ownership must not use this: an absent tab is unknown, not tab 0
    /// (see [`owned_tab`]).
    fn tab(&self) -> u32 {
        self.tab_id.unwrap_or(0)
    }

    /// The pane-id string zellij's `focus-pane-id` / `close-pane
    /// --pane-id` accept (`terminal_<id>` / `plugin_<id>`).
    pub(crate) fn pane_id(&self) -> String {
        let kind = if self.is_plugin { "plugin" } else { "terminal" };
        format!("{kind}_{}", self.id)
    }

    pub(crate) fn has_exited(&self) -> bool {
        self.exited
    }

    pub(crate) fn tab_name(&self) -> &str {
        &self.tab_name
    }
}

/// Whether `pane` is `repo`'s TUI pane, the anchor of its workspace:
/// `clank tui --repo <repo>` or the older `clank status --repo <repo>
/// --tui`, the path compared canonically.
fn is_anchor(pane: &ZellijPane, repo: &Path) -> bool {
    pane_for(pane, repo) == Some(None)
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

/// The session `clank open` names for `repo` — the convention, not
/// necessarily where the repo's panes ARE: `open_one` adds a tab to
/// whatever session the caller is in. A caller that needs the truth
/// verifies the listing (`repo_tab_id`).
pub(crate) fn repo_session_name(repo: &Path) -> String {
    let basename = repo.file_name().and_then(|s| s.to_str()).unwrap_or("repo");
    session_name(basename)
}

/// The session this process runs inside, if any.
pub(crate) fn current_session() -> Option<String> {
    std::env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty())
}

/// [`snapshot_panes`] for a NAMED session, from inside it or not.
/// The remote runs wherever the TUI was started and asks about the
/// session it was pointed at.
pub(crate) fn snapshot_panes_in(session: &str) -> Option<Vec<ZellijPane>> {
    let out = std::process::Command::new("zellij")
        .args([
            "--session",
            session,
            "action",
            "list-panes",
            "--json",
            "--command",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// One agent pane as the web page needs it: identity, size, and
/// whether its process has ended. Columns matter because a subscribed
/// viewport carries rows (its line count) but not width.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct PaneMeta {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) exited: bool,
}

/// The workspace's agent panes from a listing, by label.
pub(crate) fn agent_pane_table(panes: &[ZellijPane], ws: &Workspace) -> Vec<PaneMeta> {
    let mut out: Vec<PaneMeta> = panes
        .iter()
        .filter_map(|p| {
            let label = ws.agent_of(p)?;
            Some(PaneMeta {
                id: p.pane_id(),
                label,
                columns: p.pane_columns,
                rows: p.pane_rows,
                exited: p.exited,
            })
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
    out
}

/// The `subscribe` argv for a set of panes, or nothing for none — a
/// subscription to no panes is not a subscription. `--ansi` because
/// the page renders through a terminal emulator that wants the
/// escapes; `--format json` because each line then names its pane.
pub(crate) fn subscribe_argv(session: &str, panes: &[String]) -> Option<Vec<String>> {
    if panes.is_empty() {
        return None;
    }
    let mut argv: Vec<String> = [
        "--session",
        session,
        "subscribe",
        "--format",
        "json",
        "--ansi",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for p in panes {
        argv.push("-p".to_string());
        argv.push(p.clone());
    }
    Some(argv)
}

/// A running `zellij subscribe`, killed when dropped: it is a child
/// of ours and must not outlive us.
pub(crate) struct SubscribeChild(std::process::Child);

impl SubscribeChild {
    /// A test's own process standing in for `zellij subscribe`.
    #[cfg(test)]
    pub(crate) fn from_child(child: std::process::Child) -> Self {
        Self(child)
    }

    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.0.stdout.take()
    }
}

impl Drop for SubscribeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start streaming `panes`' viewports from `session`. `None` for no
/// panes, or when zellij could not be started.
pub(crate) fn subscribe_panes(session: &str, panes: &[String]) -> Option<SubscribeChild> {
    let argv = subscribe_argv(session, panes)?;
    std::process::Command::new("zellij")
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
        .map(SubscribeChild)
}

// Terminal transport and provider key knowledge live in one module.
#[cfg(test)]
use super::pane_input::{codex_submit_argv, say_argvs, stop_argv};

/// `pane`'s screen in `session`, its colours kept, or `None` when zellij
/// does not answer within its budget.
pub(crate) async fn dump_pane_screen(session: &str, pane: &str) -> Option<String> {
    let argv = [
        "zellij",
        "--session",
        session,
        "action",
        "dump-screen",
        "--pane-id",
        pane,
        "--ansi",
    ]
    .map(str::to_string);
    let out = run_open_query(&argv, std::time::Duration::from_secs(3))
        .await
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Why a terminal write did not complete, as what it may have done.
#[derive(Debug)]
pub(crate) enum PaneWrite {
    /// The transport never ran: nothing was written.
    NotStarted(String),
    /// It ran, and may have written some or all of it.
    Unknown(String),
}

impl std::fmt::Display for PaneWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaneWrite::NotStarted(e) | PaneWrite::Unknown(e) => f.write_str(e),
        }
    }
}

/// Execute one typed terminal-input action, bounded. Provider policy and the
/// admission rule belong to pane_input, its one caller; a timeout is an
/// uncertain mutation, not evidence that the server ignored the bytes, so
/// callers must not resend it. Says whether a failure wrote nothing.
pub(crate) async fn write_pane_action_classified(
    session: &str,
    pane: &str,
    argv: &[String],
) -> Result<(), PaneWrite> {
    let mut command = tokio::process::Command::new("zellij");
    command.args(["--session", session]).args(argv);
    run_pane_write(&mut command, std::time::Duration::from_secs(3))
        .await
        .map(|_| ())
        .map_err(|e| match e {
            PaneWrite::Unknown(e) => PaneWrite::Unknown(format!("terminal input for {pane}: {e}")),
            not_started => not_started,
        })
}

async fn run_pane_write(
    command: &mut tokio::process::Command,
    budget: std::time::Duration,
) -> Result<std::process::ExitStatus, PaneWrite> {
    use tokio::io::AsyncReadExt;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| PaneWrite::NotStarted(format!("starting terminal input transport: {e}")))?;
    let mut group = super::wait::GroupKill::over(child.id());
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut tail = super::wait::RingTail::new(4096);
    let result = tokio::time::timeout(budget, async {
        let mut sink = tokio::io::sink();
        let out = tokio::io::copy(&mut stdout, &mut sink);
        let err = async {
            let mut bytes = [0; 8192];
            loop {
                let count = stderr.read(&mut bytes).await?;
                if count == 0 {
                    break;
                }
                tail.push(&bytes[..count]);
            }
            Ok::<_, std::io::Error>(())
        };
        tokio::try_join!(child.wait(), out, err).map(|(status, _, _)| status)
    })
    .await;
    group.terminate();
    if !matches!(result, Ok(Ok(_))) {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), child.wait()).await;
    }
    match result {
        Ok(Ok(status)) if status.success() => Ok(status),
        Ok(Ok(status)) => Err(PaneWrite::Unknown(format!(
            "terminal input transport exited {status}; stderr: {}",
            tail.into_string()
        ))),
        Ok(Err(error)) => Err(PaneWrite::Unknown(format!(
            "terminal input transport failed: {error}; acceptance is unknown; stderr: {}",
            tail.into_string()
        ))),
        Err(_) => Err(PaneWrite::Unknown(format!(
            "terminal input transport timed out after {budget:?}; acceptance is unknown; stderr: {}",
            tail.into_string()
        ))),
    }
}

/// Project a pane listing to the workspace's agents as `(label, staged)`
/// — the label from the pane's name, `staged` from GEOMETRY:
/// the pane holding the most cells of any agent pane in its tab is
/// the one on the stage. Titles used to answer this and were a second
/// source of truth for who is master — stamped by one path, parsed by
/// another, and wrong after alt+[ re-applied a stale swap. Where a
/// pane sits is what the layout decides and what the read-back checks
/// (placement-is-a-layout-applied-not-panes-shuffled). Pure.
pub(crate) fn agent_pane_pairs(panes: &[ZellijPane], ws: &Workspace) -> Vec<(String, bool)> {
    let mine: Vec<(&ZellijPane, String)> = panes
        .iter()
        .filter_map(|p| ws.agent_of(p).map(|l| (p, l)))
        .collect();
    let area = |p: &ZellijPane| u32::from(p.pane_columns) * u32::from(p.pane_rows);
    mine.iter()
        .map(|(p, l)| {
            let biggest_in_tab = mine
                .iter()
                .filter(|(o, _)| o.tab() == p.tab() && o.id != p.id)
                .all(|(o, _)| area(o) < area(p));
            (l.clone(), biggest_in_tab && area(p) > 0)
        })
        .collect()
}

/// `(pane id, label)` for every pane in the listing that is one of the
/// workspace's agents. The retitler's map.
pub(crate) fn agent_panes_by_title(panes: &[ZellijPane], ws: &Workspace) -> Vec<(String, String)> {
    panes
        .iter()
        .filter_map(|p| ws.agent_of(p).map(|l| (p.pane_id(), l)))
        .collect()
}

/// The workspace's panes that read as two or more candidates: what the
/// projections above leave out, which is not absence.
pub(crate) fn ambiguities(panes: &[ZellijPane], ws: &Workspace) -> Vec<Ambiguity> {
    panes.iter().filter_map(|p| ws.ambiguity(p)).collect()
}

/// The names of the workspace's terminal panes that read as a valid
/// agent label no candidate claims: what clank sees and will not close.
/// `repo`'s TUI pane and `caller` are not counted.
pub(crate) fn unclaimed_labels(
    panes: &[ZellijPane],
    ws: &Workspace,
    repo: &Path,
    caller: Option<&str>,
) -> std::collections::BTreeSet<String> {
    panes
        .iter()
        .filter(|p| !p.is_plugin && p.tab_id == Some(ws.tab))
        .filter(|p| !is_anchor(p, repo) && Some(p.pane_id().as_str()) != caller)
        .filter(|p| ws.identify(p) == PaneIdentity::Unknown)
        .filter_map(|p| {
            title_readings(&p.title)
                .into_iter()
                .rev()
                .find(|r| clank_core::ids::AgentLabel::parse(r).is_ok())
                .map(str::to_string)
        })
        .collect()
}

/// The tab of this listing's one TUI pane for `repo`. `None` for none,
/// several, or one whose tab the listing does not say. Never inferred
/// from agent panes: the workspace scopes the agent matcher, so it cannot
/// depend on it.
pub(crate) fn repo_tab_id(panes: &[ZellijPane], repo: &Path) -> Option<u32> {
    let mut anchors = panes.iter().filter(|p| is_anchor(p, repo));
    let one = anchors.next()?;
    if anchors.next().is_some() {
        return None;
    }
    one.tab_id
}

/// The tab's orientation: its SHAPE decides, the same rule
/// `clank open` composes with ([`Orientation::detect`]).
///
/// It used to be inferred from where the panes sat — the instrument
/// pane beside the stage read landscape, below it portrait — so that a
/// re-layout would keep whatever alt+[ last chose. That reading is
/// taken at the one moment it cannot be trusted: a roster change adds
/// the new pane with `new-pane`, which splits whatever is focused, and
/// the reconciler then lists the tab and infers from that
/// half-finished arrangement. The stage is merely the biggest agent
/// pane, and a pane created seconds ago, before any layout placed it,
/// is not reliably the one the operator sees — so a portrait tab could
/// re-lay as landscape, on a swap, for no reason but the transient
/// (lloyd on ed080a7).
///
/// The extent cannot do that. Panes TILE the tab, so
/// `max(x + columns), max(y + rows)` is the tab's size however they
/// are arranged inside it: adding a pane, closing one, or flipping the
/// arrangement leaves it exactly as it was. That is the whole property
/// — the answer is the same before and after the change that provoked
/// the question.
///
/// The cost is stated where it is paid: alt+[ / alt+] are ZELLIJ's
/// keys over the swap variants clank ships, so a manual flip still
/// works and is still forgotten by the next re-layout, which returns
/// the tab to the shape its size implies.
pub(crate) fn tab_orientation(panes: &[ZellijPane], tab_id: u32) -> Orientation {
    Orientation::detect(tab_dims(panes, tab_id))
}

/// Open a pane for `label` in the workspace's tab, named for it — its
/// identity to every listing that follows — and running its launch
/// command. Placement is not attempted here: the layout applied
/// afterwards puts it where it belongs. Focus only picks the TAB
/// `new-pane` opens in. Returns the id zellij printed for the new pane,
/// so the caller can believe it live before a listing lists it.
pub(crate) fn open_agent_pane(
    repo: &Path,
    label: &str,
    panes: &[ZellijPane],
    ws: &Workspace,
) -> Option<String> {
    let in_tab = open_beside(panes, ws, label)?;
    let repo_str = repo.to_string_lossy();
    zellij_action(&["focus-pane-id", &in_tab]);
    let new_pane = new_pane_argv(label, &repo_str);
    let refs: Vec<&str> = new_pane.iter().map(String::as_str).collect();
    let out = zellij_action(&refs)?;
    let created = String::from_utf8_lossy(&out).trim().to_string();
    (!created.is_empty()).then_some(created)
}

/// The pane of the workspace's tab to open `label`'s beside, when it has
/// none, alive or not. A pane that might be `label`'s is not none.
fn open_beside(panes: &[ZellijPane], ws: &Workspace, label: &str) -> Option<String> {
    match ws.lookup(panes, label) {
        Lookup::Known(found) if found.is_empty() => {}
        _ => return None,
    }
    panes
        .iter()
        .find(|p| p.tab_id == Some(ws.tab) && !p.is_plugin)
        .map(ZellijPane::pane_id)
}

/// The workspace's agent labels with a LIVE pane: the process in it is
/// still running. [`agent_pane_pairs`] counts a pane zellij keeps open
/// after its process ended — identity, so the corpse can be found and
/// closed — but "is this agent open" is a question about the process
/// (the-tui-knows-whether-a-pane-is-open).
pub(crate) fn live_agent_labels(
    panes: &[ZellijPane],
    ws: &Workspace,
) -> std::collections::BTreeSet<String> {
    panes
        .iter()
        .filter(|p| !p.exited)
        .filter_map(|p| ws.agent_of(p))
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
        .find(|p| p.is_focused && p.tab() == tab)
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

/// Two panes in the same column span of the same tab.
fn same_column(a: &ZellijPane, b: &ZellijPane) -> bool {
    a.tab() == b.tab() && a.pane_x == b.pane_x && a.pane_columns == b.pane_columns
}

/// Full placement identity. `tab_id` FIRST: identical coordinates in
/// two different tabs are two panes, not one stack.
fn pane_geom(p: &ZellijPane) -> (u32, u16, u16, u16, u16) {
    (p.tab(), p.pane_x, p.pane_y, p.pane_columns, p.pane_rows)
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

/// The pane the current clank process runs in, identified authoritatively
/// from the `ZELLIJ_PANE_ID` env var zellij sets per-pane. A clank command
/// always runs in a terminal pane (never the plugin pane that can share
/// the same numeric id), so the ref is `terminal_<id>`. `None` when unset.
pub(crate) fn caller_pane_id() -> Option<String> {
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

/// What one all-sessions scan saw, and what it did not.
///
/// `unanswered` is the part a caller can get wrong by not being told:
/// a session dropped for a timeout, a failed exit, or an unreadable
/// listing contributes no panes, so an absent pane means "not there"
/// only when this is empty. `clank agent start` may fail open on it;
/// `clank nudge` must not, because reporting "no agent" for a session
/// that simply did not answer sends someone looking in the wrong place.
#[derive(Debug, Default)]
pub(crate) struct SessionScan {
    pub(crate) panes: Vec<SessionPane>,
    pub(crate) unanswered: Vec<String>,
    /// The session LISTING itself could not be run, so `unanswered` is
    /// not the list of what was missed — nothing is known, including
    /// how many sessions exist.
    pub(crate) listing_failed: bool,
}

/// What one session's `list-panes` produced: its panes, or nothing
/// learned.
pub(crate) type Listed = (String, Option<Vec<ZellijPane>>);

/// Fold per-session outcomes into a scan. Pure, so the fail-closed
/// contract is testable without a zellij server: every `None` becomes
/// an `unanswered` name rather than an absence.
pub(crate) fn assemble_scan(listing: Option<Vec<Listed>>) -> SessionScan {
    let Some(outcomes) = listing else {
        return SessionScan {
            listing_failed: true,
            ..Default::default()
        };
    };
    let mut scan = SessionScan::default();
    for (session, listed) in outcomes {
        match listed {
            Some(panes) => scan.panes.extend(panes.into_iter().map(|pane| SessionPane {
                session: session.clone(),
                pane,
            })),
            None => scan.unanswered.push(session),
        }
    }
    scan
}

/// The names of sessions whose server is up, from `list-sessions -n`
/// output: the first token of each line, skipping the EXITED ones
/// (their server is gone and would not answer an action anyway).
fn live_session_names(list_output: &str) -> Vec<String> {
    listed_sessions(list_output)
        .filter_map(|s| match s {
            ListedSession::Live(name) => Some(name.to_string()),
            _ => None,
        })
        .collect()
}

/// The lines of a listing that name no session clank can read: each is a
/// session not seen, so a scan reports it unanswered.
fn unreadable_sessions(list_output: &str) -> Vec<String> {
    listed_sessions(list_output)
        .filter_map(|s| match s {
            ListedSession::Unreadable(line) => Some(line.to_string()),
            _ => None,
        })
        .collect()
}

/// Every pane of every live session, each tagged with its session:
/// one `list-panes` per session, all started at once, collected until
/// `deadline`. A session that has not answered by then is killed and
/// dropped — measured here: two stale two-month-old servers took 8.7 s
/// and 4.8 s to answer while every healthy one took under 0.2 s, and
/// an agent launch cannot wait on that. Dropping a session means its
/// panes are not seen, which fails OPEN (no holder found).
pub(crate) fn panes_in_all_sessions(deadline: std::time::Duration) -> SessionScan {
    scan_sessions(deadline, zellij_list_sessions_checked, start_list_panes)
}

/// Scoped async discovery for event/nudge control. Every query owns its client
/// group and pipe drains through a deadline; no detached reader thread or
/// blocking global session listing can outlive cancellation or retirement.
pub(crate) async fn panes_in_all_sessions_async(deadline: std::time::Duration) -> SessionScan {
    use futures_util::StreamExt;
    let argv = vec!["zellij".into(), "list-sessions".into(), "-n".into()];
    let Some(listing) = (match run_open_query(&argv, deadline).await {
        Ok(output) => interpret_listing(output.status.success(), &output.stdout, &output.stderr),
        Err(_) => None,
    }) else {
        return assemble_scan(None);
    };
    let mut queries = futures_util::stream::FuturesUnordered::new();
    let mut outcomes: Vec<Listed> = unreadable_sessions(&listing)
        .into_iter()
        .map(|line| (line, None))
        .collect();
    for session in live_session_names(&listing) {
        queries.push(async move {
            let argv = vec![
                "zellij".into(),
                "-s".into(),
                session.clone(),
                "action".into(),
                "list-panes".into(),
                "--json".into(),
                "--command".into(),
                "--tab".into(),
            ];
            let panes = match run_open_query(&argv, deadline).await {
                Ok(output) if output.status.success() => {
                    serde_json::from_str::<Vec<ZellijPane>>(&output.stdout).ok()
                }
                _ => None,
            };
            (session, panes)
        });
    }
    while let Some(outcome) = queries.next().await {
        outcomes.push(outcome);
    }
    assemble_scan(Some(outcomes))
}

/// [`panes_in_all_sessions`] with the session listing supplied, so the
/// blind case is reachable in a test. `None` means the listing could
/// not be run; `Some("")` means it ran and found nothing, and the two
/// must not converge.
/// One session's `list-panes` in flight.
struct Pending {
    session: String,
    child: std::process::Child,
    reader: std::thread::JoinHandle<Vec<u8>>,
}

/// Start `list-panes` for `session`. `None` when the process could not
/// be started at all, which is a session whose panes are UNKNOWN.
fn start_list_panes(session: &str) -> Option<Pending> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new("zellij")
        .args([
            "-s",
            session,
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
    // Drained on its own thread so a listing larger than the pipe
    // buffer cannot wedge the child before it exits.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    Some(Pending {
        session: session.to_string(),
        child,
        reader,
    })
}

fn scan_sessions(
    deadline: std::time::Duration,
    list: impl FnOnce() -> Option<String>,
    start: impl Fn(&str) -> Option<Pending>,
) -> SessionScan {
    let Some(listing) = list() else {
        return assemble_scan(None);
    };
    // A session whose `list-panes` could not even START is one whose
    // panes are unknown, not one with none — it used to vanish here.
    let mut outcomes: Vec<Listed> = unreadable_sessions(&listing)
        .into_iter()
        .map(|line| (line, None))
        .collect();
    let mut pending: Vec<Pending> = Vec::new();
    for session in live_session_names(&listing) {
        match start(&session) {
            Some(p) => pending.push(p),
            None => outcomes.push((session, None)),
        }
    }

    let started = std::time::Instant::now();
    loop {
        let mut i = 0;
        while i < pending.len() {
            match pending[i].child.try_wait() {
                Ok(Some(status)) => {
                    let done = pending.swap_remove(i);
                    let listed = match done.reader.join() {
                        Ok(buf) if status.success() => {
                            serde_json::from_slice::<Vec<ZellijPane>>(&buf).ok()
                        }
                        _ => None,
                    };
                    outcomes.push((done.session, listed));
                }
                Ok(None) => i += 1,
                Err(_) => {
                    let gone = pending.swap_remove(i);
                    outcomes.push((gone.session, None));
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
        outcomes.push((late.session, None));
    }
    assemble_scan(Some(outcomes))
}

/// The `new-pane` argv, split out so it is assertable without
/// spawning a zellij. Never `--stacked`: where the pane belongs is the
/// layout's decision, applied right after.
fn new_pane_argv(label: &str, repo: &str) -> Vec<String> {
    let mut argv = vec!["new-pane".to_string()];
    argv.extend([
        "--name".to_string(),
        agent_pane_title(label),
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

/// Whether zellij accepted the rename. Accepted is not applied: a
/// caller that needs the name reads it back.
pub(crate) fn rename_tab(id: u32, name: &str) -> bool {
    // `.output()` (NOT `.status()`): capture + discard the child's
    // stdout/stderr so a rename error never bleeds onto the alt-screen
    // the TUI owns. The loop is event-driven, so an inherited error
    // line would PERSIST until the next watcher event, not flicker
    // (ruthless 9c38523).
    std::process::Command::new("zellij")
        .args(["action", "rename-tab-by-id", &id.to_string(), name])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// This session's tabs by stable id, with their names; `None` when the
/// listing fails or does not parse.
pub(crate) fn list_tabs() -> Option<Vec<(u32, String)>> {
    let out = std::process::Command::new("zellij")
        .args(["action", "list-tabs", "--json"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_tabs(&out.stdout))
        .flatten()
}

/// `list-tabs --json`, each tab's id and name. A tab without an id
/// fails the whole parse: a tab nobody can name by id is not one to
/// rename.
pub(crate) fn parse_tabs(json: &[u8]) -> Option<Vec<(u32, String)>> {
    #[derive(serde::Deserialize)]
    struct Tab {
        tab_id: u32,
        name: String,
    }
    let tabs: Vec<Tab> = serde_json::from_slice(json).ok()?;
    Some(tabs.into_iter().map(|t| (t.tab_id, t.name)).collect())
}

/// The tab holding the terminal pane `caller` (`terminal_<id>`): exactly
/// one such pane in the listing, which names its tab. An absent tab is
/// unknown, never tab 0, and a duplicate pane id is ambiguous.
pub(crate) fn owned_tab(panes: &[ZellijPane], caller: &str) -> Option<u32> {
    let mut found = panes
        .iter()
        .filter(|p| !p.is_plugin && p.pane_id() == caller);
    let one = found.next()?;
    if found.next().is_some() {
        return None;
    }
    one.tab_id
}

pub(crate) fn rename_pane(id: &str, name: &str) {
    let _ = rename_pane_checked(id, name);
}

/// Rename the pane `id` in this session, or say what zellij answered.
pub(crate) fn rename_pane_checked(id: &str, name: &str) -> Result<(), String> {
    // `.output()` (NOT `.status()`): isolate the child's stdout/stderr
    // from the alt-screen — a stale pane id (closed between list-panes
    // and the rename) or any zellij hiccup must not bleed an error line
    // onto the TUI, which the event-driven loop would leave until the
    // next watcher event (ruthless 9c38523).
    let out = rename_pane_command(id, name)
        .output()
        .map_err(|e| format!("could not run zellij: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "zellij exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

fn rename_pane_command(id: &str, name: &str) -> std::process::Command {
    let mut command = std::process::Command::new("zellij");
    command.args(["action", "rename-pane", "--pane-id", id, name]);
    command
}

/// zellij is the workspace, so `clank open` without it is a setup
/// problem, not a runtime one. Say so ONCE, before composing anything:
/// the failure otherwise surfaced as a raw OS error from the spawn
/// (`spawning zellij: No such file or directory`) after a silent
/// empty `list-sessions`, which told the user nothing about what to
/// install (zellij-is-the-workspace).
pub(crate) fn require_zellij() -> anyhow::Result<()> {
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
    /// A client without `override-layout` (zellij 0.45): roster changes
    /// are not placed and alt+[ / alt+] apply the arrangement from
    /// when the session was opened, until an upgrade.
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
        .args(["action", "override-layout", "--help"])
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
    for p in panes.iter().filter(|p| p.tab() == tab_id) {
        cols = cols.max(p.pane_x.saturating_add(p.pane_columns));
        rows = rows.max(p.pane_y.saturating_add(p.pane_rows));
    }
    (cols, rows)
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
    ws: &Workspace,
    repo: &Path,
    reviewers: &[String],
) -> bool {
    let mine: Vec<&ZellijPane> = panes
        .iter()
        .filter(|p| ws.agent_of(p).is_some_and(|l| reviewers.contains(&l)))
        .collect();
    let Some(first) = mine.first() else {
        return true;
    };
    let statuses: Vec<&ZellijPane> = panes.iter().filter(|p| is_anchor(p, repo)).collect();
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

/// Best-effort: close the panes of removed reviewers, matched by name in
/// the workspace of the pass's shared listing. `labels` is a MULTISET
/// — duplicate entries close DISTINCT panes (the duplicate-pane plan
/// entries; codex 5d498d0). Closing kills each pane's process tree —
/// that is the agent-exit guarantee. Focus restoration is the worker's
/// pass-level transaction.
pub(crate) fn remove_reviewer_panes(labels: &[String], panes: &[ZellijPane], ws: &Workspace) {
    for id in remove_target_ids(labels, panes, ws) {
        zellij_action(&["close-pane", "--pane-id", &id]);
    }
}

/// The DISTINCT pane ids the remove multiset resolves to: each label
/// occurrence consumes one matching pane, so duplicate labels map to
/// different panes (codex 4df5f0c). Pure.
pub(crate) fn remove_target_ids(
    labels: &[String],
    panes: &[ZellijPane],
    ws: &Workspace,
) -> Vec<String> {
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
            let Some(label) = ws.agent_of(pane) else {
                continue;
            };
            let Some(n) = budget.get_mut(label.as_str()) else {
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
    for session in listed_sessions(list_output) {
        if session.name() != name {
            continue;
        }
        if let ListedSession::Exited(_) = session {
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

fn delete_argv(name: &str) -> Vec<String> {
    vec![
        "zellij".to_string(),
        "delete-session".to_string(),
        name.to_string(),
    ]
}

/// Retain the existing implicit current-session layout routing. The caller
/// queries that same current session before issuing this argv, and skips
/// creation when the tab is already present.
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

    /// The name clank gives the pane a test describes by its launch
    /// command; empty for any other command.
    fn title_for(cmd: &str) -> String {
        cmd.strip_prefix("clank agent start ")
            .and_then(|rest| rest.split(" --repo ").next())
            .unwrap_or_default()
            .to_string()
    }

    /// The workspace of `panes`' first pane's tab, with each pane's title,
    /// stripped of glyph and role, as a candidate.
    fn every(panes: &[ZellijPane]) -> Workspace {
        Workspace {
            tab: panes[0].tab_id.expect("fixture pane has a tab"),
            candidates: panes
                .iter()
                .filter_map(|p| title_readings(&p.title).last().map(|r| r.to_string()))
                .filter(|t| !t.is_empty())
                .collect(),
        }
    }

    #[derive(Default)]
    struct FakeOpenIo {
        replies: std::collections::VecDeque<anyhow::Result<QueryOutput>>,
        calls: Vec<(&'static str, Vec<String>)>,
        scan: SessionScan,
        reports: Vec<String>,
        /// Every ownership record call: the repo, then its labels.
        owned: Vec<Vec<String>>,
    }

    impl FakeOpenIo {
        fn with(replies: impl IntoIterator<Item = anyhow::Result<QueryOutput>>) -> Self {
            Self {
                replies: replies.into_iter().collect(),
                ..Default::default()
            }
        }
        fn spawned(&self) -> Vec<&[String]> {
            self.calls
                .iter()
                .filter(|(kind, _)| *kind == "spawn")
                .map(|(_, argv)| argv.as_slice())
                .collect()
        }
    }

    impl OpenIo for FakeOpenIo {
        async fn query(&mut self, argv: &[String]) -> anyhow::Result<QueryOutput> {
            self.calls.push(("query", argv.to_vec()));
            self.replies.pop_front().expect("unexpected query")
        }
        fn spawn(&mut self, argv: &[String]) -> anyhow::Result<()> {
            self.calls.push(("spawn", argv.to_vec()));
            Ok(())
        }
        async fn scan(&mut self) -> SessionScan {
            self.calls.push(("scan", Vec::new()));
            std::mem::take(&mut self.scan)
        }
        fn own(
            &mut self,
            repo: &Path,
            labels: &std::collections::BTreeSet<String>,
        ) -> anyhow::Result<()> {
            let mut call = vec![repo.display().to_string()];
            call.extend(labels.iter().cloned());
            self.owned.push(call);
            Ok(())
        }
        fn report(&mut self, text: &str) {
            self.reports.push(text.to_string());
        }
        fn user_template(&self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn term_size(&self) -> (u16, u16) {
            (40, 120)
        }
    }

    fn query_output(code: i32, stdout: &str, stderr: &str) -> anyhow::Result<QueryOutput> {
        use std::os::unix::process::ExitStatusExt;
        Ok(QueryOutput {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    fn current_session() -> SessionTarget {
        SessionTarget::Current {
            name: Some("user-workspace".into()),
        }
    }

    fn openable_repo(root: &Path, name: &str) -> PathBuf {
        use crate::cli::teams_config::{RepoConfigFile, RosterAgent, RosterRole};
        let repo = root.join(name);
        let mut cfg = RepoConfigFile::default();
        cfg.agents.insert(
            clank_core::ids::AgentLabel::parse("codex").unwrap(),
            RosterAgent {
                tool: clank_core::vocab::Tool::Codex,
                launch: None,
                initial_prompt: None,
                role: RosterRole::Master,
            },
        );
        crate::agent_store::write_typed_config(&repo.join(".clank/config.json"), &cfg).unwrap();
        repo
    }

    #[tokio::test]
    async fn explicit_query_failure_after_auto_open_does_not_undo_the_visible_tab() {
        let root = tempfile::tempdir().unwrap();
        let repo = openable_repo(root.path(), "changeless-hub-research");
        let session = current_session();
        let mut io = FakeOpenIo::with([
            query_output(0, "other-tab\n", ""),
            query_output(1, "", "permission denied opening session socket"),
        ]);
        super::super::fork::open_created_fork(&repo, true, false, |args| {
            assert_eq!(args.repo.as_deref(), Some(repo.as_path()));
            open_one(&mut io, &session, &repo, args.print, None)
        })
        .await
        .unwrap();
        let first_layout = std::fs::read(layout_file_path(&repo)).unwrap();
        let error = open_one(&mut io, &session, &repo, false, None)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("user-workspace") && text.contains("query-tab-names"),
            "{text}"
        );
        assert!(
            text.contains("exit status: 1") && text.contains("permission denied"),
            "{text}"
        );
        assert!(
            !text.contains("dead") && !text.contains("fresh terminal"),
            "{text}"
        );
        assert_eq!(
            io.calls.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            ["query", "spawn", "query"]
        );
        assert_eq!(
            io.spawned(),
            [in_session_tab_argv(&layout_file_path(&repo)).as_slice()]
        );
        assert_eq!(
            std::fs::read(layout_file_path(&repo)).unwrap(),
            first_layout
        );
        assert!(repo.exists());
    }

    #[tokio::test]
    async fn a_current_session_query_failure_never_writes_or_spawns() {
        let root = tempfile::tempdir().unwrap();
        let repo = openable_repo(root.path(), "fork");
        for reply in [
            query_output(1, "", "session socket does not exist"),
            query_output(1, "", "permission denied"),
            Err(anyhow::anyhow!(
                "could not start zellij: executable missing"
            )),
            Err(anyhow::anyhow!("query timed out")),
        ] {
            let mut io = FakeOpenIo::with([reply]);
            let error = open_one(&mut io, &current_session(), &repo, false, None)
                .await
                .unwrap_err();
            let text = format!("{error:#}");
            assert!(
                text.contains("could not determine whether the tab already exists"),
                "{text}"
            );
            assert!(io.spawned().is_empty());
            assert!(!layout_file_path(&repo).exists());
        }
    }

    /// A tab named for the repo is not the repo: the scan has already said
    /// it is open nowhere, so a same-named tab is another repo's, and the
    /// target opens beside it under a name of its own.
    #[tokio::test]
    async fn a_same_named_tab_is_another_repos_and_the_target_opens_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = openable_repo(root.path(), "fork");
        let mut io = FakeOpenIo::with([query_output(0, "👀 fork\n", "")]);
        open_one(&mut io, &current_session(), &repo, false, None)
            .await
            .unwrap();
        assert_eq!(
            io.spawned(),
            [in_session_tab_argv(&layout_file_path(&repo)).as_slice()]
        );
        let layout = std::fs::read_to_string(layout_file_path(&repo)).unwrap();
        let parent = parent_name(&repo).unwrap();
        assert!(
            layout.contains(&format!("tab name=\"fork ({parent})\"")),
            "{layout}"
        );
        std::fs::remove_file(layout_file_path(&repo)).unwrap();
        let mut io = FakeOpenIo::with([query_output(0, "unrelated\n", "")]);
        open_one(&mut io, &current_session(), &repo, false, None)
            .await
            .unwrap();
        assert_eq!(io.calls[0].1, current_session().tab_query_argv());
        assert_eq!(
            io.spawned(),
            [in_session_tab_argv(&layout_file_path(&repo)).as_slice()]
        );
        assert!(layout_file_path(&repo).exists());
    }

    #[tokio::test]
    async fn outside_all_does_not_turn_an_unanswered_live_session_into_missing_tabs() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let fork = openable_repo(root.path(), "fork");
        let mut io = FakeOpenIo::with([
            query_output(0, "clank-proj [Created 1m ago]\n", ""),
            query_output(1, "", "permission denied opening clank-proj socket"),
        ]);
        let hosts = ["clank-proj".to_string()];
        let error = open_all(&mut io, &source, &[source.clone(), fork], &hosts, false)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("session `clank-proj`") && text.contains("permission denied"),
            "{text}"
        );
        assert_eq!(
            io.calls[1].1,
            SessionTarget::Named("clank-proj".into()).tab_query_argv()
        );
        assert!(io.spawned().is_empty());
        assert!(!layout_file_path(&source).exists());
    }

    #[tokio::test]
    async fn a_failed_outside_listing_does_not_authorize_single_or_all_creation() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let mut io = FakeOpenIo::with([query_output(1, "", "socket directory: permission denied")]);
        open_one(
            &mut io,
            &SessionTarget::Named("clank-proj".into()),
            &source,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(io.spawned().is_empty());
        let mut io = FakeOpenIo::with([query_output(1, "", "socket directory: permission denied")]);
        open_all(&mut io, &source, std::slice::from_ref(&source), &[], false)
            .await
            .unwrap_err();
        assert!(io.spawned().is_empty());
        assert!(!layout_file_path(&source).exists());
    }

    /// Outside a session: create the conventional session, or recreate a
    /// dead one; a live one of that name is another repo's (the scan found
    /// this one nowhere), so it is never attached to, and the target gets a
    /// session of its own.
    #[tokio::test]
    async fn outside_single_creates_its_own_session_and_never_attaches_to_anothers() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let layout = layout_file_path(&source);
        let own = session_name(&format!("proj-{}", parent_name(&source).unwrap()));
        for (response, expected) in [
            (
                query_output(1, "", NO_SESSIONS),
                vec![create_argv(&layout, "clank-proj")],
            ),
            (
                query_output(0, "clank-proj [Created 1m ago]\n", ""),
                vec![create_argv(&layout, &own)],
            ),
            (
                query_output(0, "clank-proj [Created 1m ago] (EXITED)\n", ""),
                vec![
                    delete_argv("clank-proj"),
                    create_argv(&layout, "clank-proj"),
                ],
            ),
        ] {
            let mut io = FakeOpenIo::with([response]);
            open_one(
                &mut io,
                &SessionTarget::Named("clank-proj".into()),
                &source,
                false,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                io.spawned(),
                expected.iter().map(Vec::as_slice).collect::<Vec<_>>()
            );
        }
    }

    /// `--all` outside: the missing targets are added to the conventional
    /// session when this repo lives there; when another repo holds that
    /// name, they get a session of their own.
    #[tokio::test]
    async fn outside_all_adds_to_its_own_session_and_never_to_anothers() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let fork = openable_repo(root.path(), "fork");
        let hosts = ["clank-proj".to_string()];
        let mut io = FakeOpenIo::with([
            query_output(0, "clank-proj [Created 1m ago]\n", ""),
            query_output(0, "🔨 proj\n", ""),
        ]);
        open_all(&mut io, &source, std::slice::from_ref(&fork), &hosts, false)
            .await
            .unwrap();
        assert_eq!(
            io.spawned(),
            [add_tabs_argv(&layout_file_path(&source), "clank-proj").as_slice()]
        );
        let layout = std::fs::read_to_string(layout_file_path(&source)).unwrap();
        assert!(layout.contains("tab name=\"fork\""));
        assert!(!layout.contains("tab name=\"proj\""));
        std::fs::remove_file(layout_file_path(&source)).unwrap();

        let mut io = FakeOpenIo::with([query_output(0, "clank-proj [Created 1m ago]\n", "")]);
        open_all(&mut io, &source, &[source.clone(), fork], &[], false)
            .await
            .unwrap();
        let own = session_name(&format!("proj-{}", parent_name(&source).unwrap()));
        assert_eq!(
            io.spawned(),
            [create_argv(&layout_file_path(&source), &own).as_slice()]
        );
    }

    #[tokio::test]
    async fn outside_all_creates_every_tab_only_after_an_answered_empty_listing() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let fork = openable_repo(root.path(), "fork");
        let mut io = FakeOpenIo::with([query_output(1, NO_SESSIONS, "")]);
        open_all(&mut io, &source, &[source.clone(), fork], &[], false)
            .await
            .unwrap();
        assert_eq!(io.calls.len(), 2, "one listing, one creation, no tab query");
        assert_eq!(
            io.spawned(),
            [create_argv(&layout_file_path(&source), "clank-proj").as_slice()]
        );
        let layout = std::fs::read_to_string(layout_file_path(&source)).unwrap();
        assert!(layout.contains("tab name=\"fork\"") && layout.contains("tab name=\"proj\""));
        assert_eq!(
            io.owned
                .iter()
                .map(|call| call[1..].to_vec())
                .collect::<Vec<_>>(),
            [["codex"], ["codex"]],
            "each opened tab's team is recorded as clank's"
        );
    }

    /// `--all` inside a session queries its tabs once, and gives each
    /// target, already sorted by the scan, a tab name nothing there has,
    /// those it has just added included.
    #[tokio::test]
    async fn current_all_queries_once_and_names_each_tab_apart() {
        let root = tempfile::tempdir().unwrap();
        let a = openable_repo(&root.path().join("a"), "fork");
        let b = openable_repo(&root.path().join("b"), "fork");
        let mut io = FakeOpenIo::with([query_output(0, "🔨 proj\n", "")]);
        open_current_targets(&mut io, &current_session(), &[a.clone(), b.clone()], false)
            .await
            .unwrap();
        assert_eq!(
            io.calls.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            ["query", "spawn", "spawn"]
        );
        let layout = std::fs::read_to_string(layout_file_path(&b)).unwrap();
        assert!(layout.contains("tab name=\"fork (b)\""), "{layout}");
    }

    #[tokio::test]
    async fn preview_does_not_query_write_or_spawn_for_any_open_route() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let mut io = FakeOpenIo::default();
        open_one(&mut io, &current_session(), &source, true, None)
            .await
            .unwrap();
        open_one(
            &mut io,
            &SessionTarget::Named("clank-proj".into()),
            &source,
            true,
            None,
        )
        .await
        .unwrap();
        open_all(&mut io, &source, std::slice::from_ref(&source), &[], true)
            .await
            .unwrap();
        open_current_targets(
            &mut io,
            &current_session(),
            std::slice::from_ref(&source),
            true,
        )
        .await
        .unwrap();
        assert!(io.calls.is_empty());
        assert!(!layout_file_path(&source).exists());
    }

    // Real pipe/deadline tests use harmless shell fixtures, never Zellij.
    #[tokio::test]
    async fn open_query_drains_both_streams_without_unbounded_capture() {
        let argv = ["/bin/sh", "-c", "i=0; while [ $i -lt 5000 ]; do printf 'stderr-line\n' >&2; i=$((i+1)); done; printf 'tab\n'; exit 1"].map(str::to_string);
        let out = run_open_query(&argv, std::time::Duration::from_secs(3))
            .await
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(out.stdout, "tab\n");
        assert_eq!(out.stderr.len(), 4096);
        assert!(out.stderr.ends_with("stderr-line\n"));
    }

    #[tokio::test]
    async fn pane_write_discards_stdout_and_retains_failure_tail() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "head -c 100000 /dev/zero; printf transport-refused >&2; exit 7",
        ]);
        let error = run_pane_write(&mut command, std::time::Duration::from_secs(3))
            .await
            .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("transport-refused"));
        assert!(diagnostic.contains("7"));
        assert!(
            diagnostic.len() < 5000,
            "stdout must not be captured as an error body"
        );
    }

    #[tokio::test]
    async fn pane_write_deadline_covers_descendants_retaining_output_pipes() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let started = std::time::Instant::now();
        let error = run_pane_write(&mut command, std::time::Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("acceptance is unknown"));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn oversized_query_stdout_is_unknown_instead_of_a_partial_listing() {
        let argv = ["/bin/sh", "-c", "head -c 1100000 /dev/zero"].map(str::to_string);
        let error = run_open_query(&argv, std::time::Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("stdout exceeded 1048576 bytes"));
    }

    #[tokio::test]
    async fn open_query_times_out_even_when_a_descendant_retains_the_pipes() {
        let argv = [
            "/bin/sh",
            "-c",
            "printf 'socket query stalled\n' >&2; /bin/sleep 30 & exit 0",
        ]
        .map(str::to_string);
        let started = std::time::Instant::now();
        let error = run_open_query(&argv, std::time::Duration::from_millis(500))
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("timed out") && text.contains("socket query stalled"),
            "{text}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_query_that_cannot_start_retains_its_argv_and_spawn_error() {
        let argv = vec![
            "/clank-test/no-such-zellij-executable".into(),
            "action".into(),
            "query-tab-names".into(),
        ];
        let error = run_open_query(&argv, std::time::Duration::from_secs(3))
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("query-tab-names") && text.contains("No such file"),
            "{text}"
        );
    }

    /// "I could not ask" and "there are none" must not converge.
    ///
    /// Zellij exits non-zero when no sessions exist, but a scan must not
    /// fold every failed command into an empty listing and report a confident
    /// absence after learning nothing (codex on 1eec51b). Neither case
    /// starts a zellij process: one never lists, the other lists none.
    #[test]
    fn a_scan_that_could_not_list_is_blind_not_empty() {
        let quick = std::time::Duration::from_millis(1);
        let blind = scan_sessions(quick, || None, |_| None);
        assert!(blind.listing_failed, "nothing was learned");

        let empty = scan_sessions(quick, || Some(String::new()), |_| None);
        assert!(
            !empty.listing_failed,
            "the listing answered: there are none"
        );
        assert!(empty.panes.is_empty() && empty.unanswered.is_empty());
    }

    /// A session that cannot be probed is UNANSWERED, through the real
    /// mapping — a test of `assemble_scan` alone passes against a scan
    /// that never reaches it (codex on 66afcf2). The fake starter
    /// begins no process, so this spawns no zellij.
    #[test]
    fn a_session_that_cannot_be_probed_is_unanswered_not_absent() {
        let scan = scan_sessions(
            std::time::Duration::from_millis(1),
            || Some("alpha [Created 1s ago]\nbeta [Created 1s ago]\n".to_string()),
            |_| None,
        );
        assert_eq!(
            scan.unanswered,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert!(scan.panes.is_empty());
        assert!(!scan.listing_failed, "the listing itself was fine");
    }

    /// zellij exits NON-ZERO to say there are no sessions, so a status
    /// check alone reads every other failure as "none" — which is the
    /// difference between not knowing and asserting an absence.
    #[test]
    fn only_the_known_no_sessions_answer_survives_a_nonzero_exit() {
        assert_eq!(
            interpret_listing(true, "clank-proj [Created 1s ago]\n", ""),
            Some("clank-proj [Created 1s ago]\n".to_string()),
            "a clean exit is the listing"
        );
        assert_eq!(
            interpret_listing(false, "", NO_SESSIONS),
            Some(String::new()),
            "zellij's own no-sessions answer is an answer"
        );
        assert_eq!(
            interpret_listing(false, NO_SESSIONS, ""),
            Some(String::new()),
            "on either stream"
        );
        assert_eq!(
            interpret_listing(false, "", "error: permission denied"),
            None,
            "any other failure taught us nothing"
        );
        assert_eq!(
            interpret_listing(false, NO_SESSIONS, "socket permission denied"),
            None,
            "a conflicting error is not proof of absence"
        );
        assert_eq!(
            interpret_listing(
                false,
                "",
                &format!("socket permission denied; {NO_SESSIONS}")
            ),
            None,
            "a fragment in a failed diagnostic is not the no-sessions answer"
        );
        assert_eq!(
            interpret_listing(false, "", ""),
            None,
            "and a silent failure least of all"
        );
    }
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
                .find(|l| l.contains("name=\"\""))
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

    #[test]
    fn a_pane_is_renamed_by_its_id() {
        let command = rename_pane_command("terminal_5", "clank 1.0.0");
        assert_eq!(command.get_program(), "zellij");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "action",
                "rename-pane",
                "--pane-id",
                "terminal_5",
                "clank 1.0.0"
            ]
        );
    }

    fn reviewers(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    const TEST_REPO: &str = "/tmp/test-repo";
    /// 200x50 cells — comfortably landscape (cols >= 2*rows).
    const LANDSCAPE: (u16, u16) = (200, 50);
    const LAND: Orientation = Orientation::Landscape;
    const PORT: Orientation = Orientation::Portrait;
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
        assert!(kdl.contains("plugin location=\"zellij:tab-bar\""));
        assert!(kdl.contains("plugin location=\"zellij:status-bar\""));
    }

    #[test]
    fn compose_kdl_wraps_panes_in_tab_block_with_name() {
        let kdl = compose_kdl("basename", TEST_REPO, "alice", &[], None, LAND).unwrap();
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
            LAND,
        )
        .unwrap();
        assert!(kdl.contains("name=\"alice\""));
        assert!(kdl.contains("name=\"bob\""));
        assert!(kdl.contains("name=\"carol\""));
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
            remove_target_ids(&["codex".into(), "codex".into()], &panes, &every(&panes)),
            vec!["terminal_1".to_string(), "terminal_2".to_string()],
            "two entries, two DISTINCT panes; the third codex pane and claude survive"
        );
    }

    /// The caller's tab is the one its own terminal pane is listed in:
    /// a plugin sharing the number is not it, a listing that does not
    /// say which tab is not tab 0, and two panes claiming the id prove
    /// nothing.
    #[test]
    fn the_owned_tab_is_where_the_callers_pane_is_listed() {
        let panes = |json: &str| serde_json::from_str::<Vec<ZellijPane>>(json).unwrap();
        let listed = panes(
            r#"[{"id":5,"is_plugin":true,"tab_id":2},
                {"id":5,"tab_id":1},
                {"id":6,"tab_id":2,"is_focused":true}]"#,
        );
        assert_eq!(owned_tab(&listed, "terminal_5"), Some(1));
        assert_eq!(owned_tab(&listed, "terminal_7"), None, "not listed");
        let untold = panes(r#"[{"id":5}]"#);
        assert_eq!(owned_tab(&untold, "terminal_5"), None, "no tab named");
        let twice = panes(r#"[{"id":5,"tab_id":1},{"id":5,"tab_id":2}]"#);
        assert_eq!(owned_tab(&twice, "terminal_5"), None, "ambiguous");
    }

    /// A tab listing names each tab by id; one without an id is not a
    /// listing to act on.
    #[test]
    fn a_tab_listing_needs_every_tabs_id() {
        assert_eq!(
            parse_tabs(br#"[{"tab_id":1,"name":"clank","active":false},{"tab_id":4,"name":"x"}]"#),
            Some(vec![(1, "clank".to_string()), (4, "x".to_string())])
        );
        assert_eq!(parse_tabs(br#"[{"name":"clank"}]"#), None);
        assert_eq!(parse_tabs(b"not json"), None);
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
            title: title_for(cmd),
            tab_id: Some(tab),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
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
        type Geom = (u16, u16, u16, u16);
        let live: [(&str, Geom, Geom); 5] = [
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
                reviewers_are_stacked(&panes, &every(&panes), repo, &two),
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
            title: title_for(cmd),
            tab_id: Some(1),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
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
        assert!(!reviewers_are_stacked(&panes, &every(&panes), repo, &two));
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
            title: title_for(cmd),
            tab_id: Some(1),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
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
            !reviewers_are_stacked(&zeroed, &every(&zeroed), repo, &two),
            "all-zero geometry must not read as an identical-shape stack"
        );

        // Run shape, with a ZERO-row member that satisfies contiguity
        // (y + 0 == y) but is not a collapsed title bar.
        let zero_member = vec![bare(cx, 1, 62, 0), bare(ru, 1, 62, 93)];
        assert!(
            !reviewers_are_stacked(&zero_member, &every(&zero_member), repo, &two),
            "a zero-row pane is not a collapsed member"
        );

        // The real collapsed shape still passes, so the guard did not
        // simply reject everything.
        let real = vec![bare(cx, 1, 62, 1), bare(ru, 2, 62, 93)];
        assert!(reviewers_are_stacked(&real, &every(&real), repo, &two));
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
            title: title_for(cmd),
            tab_id: Some(tab),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
            pane_x: 116,
            pane_y: 1,
            pane_columns: 62,
            pane_rows: 40,
        };
        let panes = [
            pane("clank agent start codex --repo /r", 1),
            pane("clank agent start ruthless --repo /r", 9),
        ];
        let mine: Vec<&ZellijPane> = panes.iter().collect();
        assert!(
            reviewer_stack_span(&mine).is_none(),
            "identical coordinates in different tabs are two panes, not a stack"
        );
    }

    #[test]
    fn reviewers_in_different_tabs_are_never_stacked() {
        // `list-panes` spans ALL tabs, so identical column spans can
        // collide across tabs and read as contiguous by coordinate
        // alone. Same-tab is an invariant, not a coincidence (codex on
        // 5320c1d) — and no layout can join two tabs.
        let pane = |cmd: &str, tab: u32, y: u16, h: u16| ZellijPane {
            id: 1,
            is_plugin: false,
            terminal_command: Some(cmd.to_string()),
            title: title_for(cmd),
            tab_id: Some(tab),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
            pane_x: 116,
            pane_y: y,
            pane_columns: 62,
            pane_rows: h,
        };
        let panes = [
            pane("clank agent start codex --repo /r", 1, 1, 1),
            pane("clank agent start ruthless --repo /r", 7, 2, 93),
        ];
        let mine: Vec<&ZellijPane> = panes.iter().collect();
        assert!(
            reviewer_stack_span(&mine).is_none(),
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
            title: title_for(cmd),
            tab_id: Some(1),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
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
            !reviewers_are_stacked(&panes, &every(&panes), repo, &two),
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
            title: title_for(cmd),
            tab_id: Some(0),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
            pane_x: x,
            pane_y: y,
            pane_columns: w,
            pane_rows: h,
        };
        let repo = std::path::Path::new("/repo");
        let one = vec!["kimi".to_string()];
        let rev = agent_start_command("kimi", "/repo");
        let status = "clank status --repo /repo --tui";

        let shared = [pane(&rev, 125, 1, 53, 133), pane(status, 125, 1, 53, 133)];
        assert!(
            !reviewers_are_stacked(&shared, &every(&shared), repo, &one),
            "one reviewer sharing the instrument pane's geometry is NOT placed"
        );
        let beside = [pane(&rev, 125, 1, 53, 100), pane(status, 125, 101, 53, 33)];
        assert!(
            reviewers_are_stacked(&beside, &every(&beside), repo, &one),
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
            title: title_for(cmd),
            tab_id: Some(0),
            is_focused: false,
            exited: false,
            tab_name: String::new(),
            tab_position: None,
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
            !reviewers_are_stacked(&broken, &every(&broken), repo, &reviewers),
            "status inside the reviewer stack is NOT placed"
        );

        // GOOD: reviewers stacked, status beside them.
        let good = vec![
            pane(&r1, 40, 0, 40, 16),
            pane(&r2, 40, 0, 40, 16),
            pane(status, 40, 16, 40, 8),
        ];
        assert!(reviewers_are_stacked(
            &good,
            &every(&good),
            repo,
            &reviewers
        ));

        // Reviewers not stacked at all is also not placed.
        let split = vec![pane(&r1, 40, 0, 40, 8), pane(&r2, 40, 8, 40, 8)];
        assert!(!reviewers_are_stacked(
            &split,
            &every(&split),
            repo,
            &reviewers
        ));

        // A repo with no status pane still resolves on the reviewers.
        let no_status = vec![pane(&r1, 40, 0, 40, 16), pane(&r2, 40, 0, 40, 16)];
        assert!(reviewers_are_stacked(
            &no_status,
            &every(&no_status),
            repo,
            &reviewers
        ));
    }

    fn set(labels: &[&str]) -> std::collections::BTreeSet<String> {
        labels.iter().map(|l| l.to_string()).collect()
    }

    /// The exact label, `<glyph> <label>`, and the old `<label> (<role>)`
    /// with and without a glyph all name the agent, for every real glyph
    /// and labels with spaces or emoji in them.
    #[test]
    fn every_title_form_names_its_agent() {
        for label in ["codex", "two words", "🔥bot"] {
            let roster = set(&[label, "claude"]);
            let mut titles = vec![label.to_string(), format!("{label} (reviewer)")];
            for glyph in AGENT_PANE_GLYPHS {
                titles.push(format!("{glyph} {label}"));
                titles.push(format!("{glyph} {label} (master)"));
            }
            for title in titles {
                assert_eq!(
                    resolve_title(&title, &roster),
                    PaneIdentity::Agent(label.to_string()),
                    "{title:?}"
                );
            }
        }
        assert_eq!(
            resolve_title("zsh", &set(&["codex"])),
            PaneIdentity::Unknown
        );
        assert_eq!(
            resolve_title("🚀 codex", &set(&["codex"])),
            PaneIdentity::Unknown,
            "only the TUI's own glyphs are stripped"
        );
        assert_eq!(
            resolve_title("codex (painter)", &set(&["codex"])),
            PaneIdentity::Unknown,
            "only role words are stripped"
        );
    }

    /// A title that reads as two distinct roster labels is refused, never
    /// resolved to the literal one; the same title is that label when it
    /// is the only candidate.
    #[test]
    fn a_title_reading_as_two_candidates_is_ambiguous() {
        let glyphed = format!("{} bob", AGENT_PANE_GLYPHS[1]);
        let old_glyphed = format!("{} x (plan)", AGENT_PANE_GLYPHS[0]);
        for (title, roster) in [
            (glyphed.as_str(), set(&["bob", &glyphed])),
            ("x (plan)", set(&["x (plan)", "x"])),
            (old_glyphed.as_str(), set(&["x (plan)", "x"])),
        ] {
            assert!(
                matches!(resolve_title(title, &roster), PaneIdentity::Ambiguous(ref c) if c.len() == 2),
                "{title:?} with {roster:?}"
            );
        }
        assert_eq!(
            resolve_title("x (plan)", &set(&["x (plan)"])),
            PaneIdentity::Agent("x (plan)".into()),
            "a literal label that is the only candidate is itself"
        );
        assert_eq!(
            resolve_title(&glyphed, &set(&[&glyphed])),
            PaneIdentity::Agent(glyphed.clone())
        );
    }

    /// A pane that reads as `painter` and `💤 painter` makes painter's
    /// panes uncertain, even beside one plainly painter's, and no pane is
    /// opened for it; a label it does not name stays known.
    #[test]
    fn an_ambiguous_pane_leaves_its_labels_uncertain_and_unopened() {
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":2,"title":"🔨 claude","tab_id":0},
              {"id":3,"title":"💤 painter","tab_id":0}
            ]"#,
        )
        .unwrap();
        let ws = Workspace {
            tab: 0,
            candidates: set(&["claude", "painter", "💤 painter"]),
        };
        for label in ["painter", "💤 painter"] {
            let Lookup::Uncertain(doubts) = ws.lookup(&panes, label) else {
                panic!("{label}");
            };
            assert_eq!(doubts[0].pane, "terminal_3");
            assert_eq!(open_beside(&panes, &ws, label), None, "{label}");
        }
        assert!(matches!(ws.lookup(&panes, "claude"), Lookup::Known(ref f) if f.len() == 1));
        assert!(matches!(ws.lookup(&panes, "bea"), Lookup::Known(ref f) if f.is_empty()));
        assert_eq!(
            open_beside(&panes, &ws, "bea").as_deref(),
            Some("terminal_2")
        );
    }

    /// Identity is the name in the workspace's tab: a plugin, a pane in
    /// another tab, and a pane running an agent's launch command under
    /// another name are none of its agents.
    #[test]
    fn a_workspace_names_only_its_own_tabs_terminal_panes() {
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"is_plugin":true,"title":"codex","tab_id":0},
              {"id":2,"title":"codex","tab_id":1},
              {"id":3,"title":"editor","terminal_command":"clank agent start codex --repo /a","tab_id":0},
              {"id":4,"title":"👀 codex","terminal_command":"bash","tab_id":0}
            ]"#,
        )
        .unwrap();
        let ws = Workspace {
            tab: 0,
            candidates: set(&["codex"]),
        };
        let named: Vec<Option<String>> = panes.iter().map(|p| ws.agent_of(p)).collect();
        assert_eq!(named, [None, None, None, Some("codex".to_string())]);
    }

    /// One session holding two repos' tabs, each with `claude` and
    /// `codex`: each repo's workspace is its own TUI pane's tab, so each
    /// sees only its own panes and neither counts the other's as a
    /// duplicate.
    #[test]
    fn two_repos_in_one_session_each_see_only_their_tab() {
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":1,"title":"","terminal_command":"clank tui --repo /a","tab_id":0},
              {"id":2,"title":"💤 claude","tab_id":0},
              {"id":3,"title":"👀 codex","tab_id":0},
              {"id":5,"title":"","terminal_command":"clank tui --repo /b","tab_id":1},
              {"id":6,"title":"💤 claude","tab_id":1},
              {"id":7,"title":"codex","tab_id":1}
            ]"#,
        )
        .unwrap();
        for (repo, ids) in [
            ("/a", ["terminal_2", "terminal_3"]),
            ("/b", ["terminal_6", "terminal_7"]),
        ] {
            let ws =
                Workspace::anchored(&panes, Path::new(repo), set(&["claude", "codex"])).unwrap();
            let mine: Vec<String> = agent_panes_by_title(&panes, &ws)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(mine, ids, "{repo}");
            assert_eq!(
                remove_target_ids(&["codex".into()], &panes, &ws).len(),
                1,
                "{repo}: one codex to close, not the other repo's too"
            );
        }
    }

    /// A workspace needs exactly one of the repo's TUI panes; agent panes
    /// never stand in for one.
    #[test]
    fn a_workspace_is_anchored_by_exactly_one_tui_pane() {
        let panes = |json: &str| serde_json::from_str::<Vec<ZellijPane>>(json).unwrap();
        let repo = Path::new("/a");
        let none = panes(r#"[{"id":2,"title":"codex","tab_id":4}]"#);
        assert_eq!(repo_tab_id(&none, repo), None);
        let two = panes(
            r#"[{"id":1,"terminal_command":"clank tui --repo /a","tab_id":0},
                {"id":5,"terminal_command":"clank status --repo /a --tui","tab_id":3}]"#,
        );
        assert_eq!(repo_tab_id(&two, repo), None);
        let untabbed = panes(r#"[{"id":1,"terminal_command":"clank tui --repo /a"}]"#);
        assert_eq!(repo_tab_id(&untabbed, repo), None);
        let one =
            panes(r#"[{"id":1,"terminal_command":"clank status --repo /a --tui","tab_id":3}]"#);
        assert_eq!(repo_tab_id(&one, repo), Some(3));
        assert!(
            Workspace::local(&none, repo, Some("terminal_2"), set(&[]))
                .is_some_and(|w| w.tab() == 4)
        );
    }

    /// Across sessions a workspace is resolved only from a complete scan
    /// with exactly one TUI pane; a TUI pane in a session other than the
    /// conventional one counts the same.
    #[test]
    fn a_scan_resolves_a_workspace_only_when_complete_and_unique() {
        let pane = |session: &str, id: u32, tab: u32, command: &str| SessionPane {
            session: session.into(),
            pane: serde_json::from_value(serde_json::json!({
                "id": id, "terminal_command": command, "tab_id": tab
            }))
            .unwrap(),
        };
        let repo = Path::new("/a");
        let tui = |session: &str, tab: u32| pane(session, 1, tab, "clank tui --repo /a");
        let scan =
            |panes: Vec<SessionPane>, unanswered: &[&str], listing_failed: bool| SessionScan {
                panes,
                unanswered: unanswered.iter().map(|s| s.to_string()).collect(),
                listing_failed,
            };
        let resolved = scan_workspace(
            &scan(vec![tui("someones-own", 2)], &[], false),
            repo,
            set(&[]),
        )
        .unwrap();
        assert_eq!((resolved.0.as_str(), resolved.1.tab()), ("someones-own", 2));
        let unresolved = [
            (scan(vec![], &[], false), Unresolved::NoAnchor),
            (
                scan(vec![tui("s", 0), tui("t", 0)], &[], false),
                Unresolved::ManyAnchors(vec![
                    ("s".into(), String::new()),
                    ("t".into(), String::new()),
                ]),
            ),
            (
                scan(vec![], &["slow"], false),
                Unresolved::Unanswered(vec!["slow".into()]),
            ),
            (
                scan(vec![tui("s", 0)], &["slow"], false),
                Unresolved::Unanswered(vec!["slow".into()]),
            ),
            (scan(vec![], &[], true), Unresolved::ListingFailed),
        ];
        for (scan, why) in unresolved {
            assert_eq!(scan_workspace(&scan, repo, set(&[])).unwrap_err(), why);
        }
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
            remove_target_ids(&["codex".to_string()], &panes, &every(&panes)),
            vec!["terminal_2".to_string()],
            "the corpse is the excess copy, whatever order the listing reports"
        );
        // A label leaving the roster entirely still loses BOTH panes:
        // the preference picks WHICH, never HOW MANY.
        assert_eq!(
            remove_target_ids(
                &["codex".to_string(), "codex".to_string()],
                &panes,
                &every(&panes)
            ),
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
        let ws = every(&panes);
        assert_eq!(ws.agent_of(&panes[0]).as_deref(), Some("codex"));
        assert_eq!(
            agent_pane_pairs(&panes, &ws),
            vec![("codex".to_string(), false)]
        );
        // The idempotence guard sees it too, so no replacement is made.
        assert_eq!(
            open_beside(&panes, &ws, "codex"),
            None,
            "a corpse must still satisfy the guard, or the pass respawns it"
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

    /// Whatever creates the pane — the layout, an add, a reopen — the
    /// pane runs `clank agent start`, and THAT is where the session's
    /// holder is found before anything resumes (a-session-has-one-
    /// holder). A pane that launched the tool directly would skip it.
    #[test]
    fn every_new_pane_launches_through_agent_start() {
        let argv = new_pane_argv("ruthless", "/repo");
        let sep = argv
            .iter()
            .position(|a| a == "--")
            .expect("a `--` separator");
        assert_eq!(
            argv[sep + 1..],
            ["clank", "agent", "start", "ruthless", "--repo", "/repo"],
            "{argv:?}"
        );
        assert_eq!(
            argv[sep + 1..].join(" "),
            agent_start_command("ruthless", "/repo"),
            "byte-identical to the identity the scans match on"
        );
        assert!(
            !argv.iter().any(|a| a == "--stacked"),
            "placement is the layout's, applied after: {argv:?}"
        );
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
            LAND,
        )
        .unwrap();
        assert!(
            kdl.contains(&format!("name=\"alice\" cwd=\"{TEST_REPO}\"")),
            "master pane should set cwd; got:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!("name=\"bob\" cwd=\"{TEST_REPO}\"")),
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
            LAND,
        )
        .unwrap();
        let bob_idx = kdl.find("name=\"bob\"").unwrap();
        let alice_idx = kdl.find("name=\"alice\"").unwrap();
        let codex_idx = kdl.find("name=\"codex\"").unwrap();
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, r#"weird"name"#, &[], None, LAND).unwrap();
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
            LAND,
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
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
        // + a `clank tui` pane) around the marker.
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
            args "tui"
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
            LAND,
        )
        .unwrap();
        // Chrome preserved verbatim.
        assert!(
            kdl.contains("plugin location=\"compact-bar\""),
            "got:\n{kdl}"
        );
        assert!(kdl.contains("tab name=\"my-clank\""));
        assert!(kdl.contains("\"tui\""));
        // The built-in bars are NOT injected — user owns the chrome.
        assert!(!kdl.contains("zellij:tab-bar"));
        // Marker replaced with the agent group.
        assert!(!kdl.contains(AGENTS_MARKER));
        assert!(kdl.contains("name=\"alice\""));
        assert!(kdl.contains("name=\"bob\""));
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap();
        assert!(!kdl.contains(AGENTS_MARKER));
        assert!(kdl.contains("name=\"m\""));
    }

    /// (how many root `keybinds` nodes, the first one's first global
    /// `unbind`'s keys)
    fn pane_keys_of(kdl: &str) -> (usize, Vec<String>) {
        let doc: kdl::KdlDocument = kdl.parse().unwrap();
        let count = doc
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "keybinds")
            .count();
        let keys = doc
            .get("keybinds")
            .and_then(|k| k.children())
            .and_then(|c| c.get("unbind"))
            .map(|u| {
                u.entries()
                    .iter()
                    .filter_map(|e| e.value().as_string().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        (count, keys)
    }

    fn pane_keys() -> Vec<String> {
        PANE_KEYS.iter().map(|k| k.to_string()).collect()
    }

    /// Every layout clank creates a session with unbinds the four
    /// Alt+arrows, so they reach the agent in the pane; the `layout`
    /// node comes first and is as it was.
    #[test]
    fn every_composed_layout_unbinds_the_pane_keys() {
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
        assert_eq!(pane_keys_of(&kdl), (1, pane_keys()), "{kdl}");
        assert!(kdl.ends_with(BLOCK), "{kdl}");
        let doc: kdl::KdlDocument = kdl.parse().unwrap();
        assert_eq!(doc.nodes()[0].name().value(), "layout");
        let swaps = doc.nodes()[0]
            .children()
            .unwrap()
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "swap_tiled_layout")
            .count();
        assert_eq!(swaps, 2);

        let tabs = vec![TabSpec {
            name: "main".into(),
            repo_path: "/repo".into(),
            master: "alice".into(),
            reviewers: vec!["bob".into()],
        }];
        let kdl = compose_multitab(&tabs, LANDSCAPE).unwrap();
        assert_eq!(pane_keys_of(&kdl), (1, pane_keys()), "{kdl}");
        assert!(kdl.starts_with("layout {"));
        assert!(kdl.ends_with(BLOCK), "{kdl}");
    }

    const BLOCK: &str =
        "}\nkeybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\"\n}\n";

    /// A template with its own `keybinds` keeps it: zellij reads only
    /// the first such node and its first global `unbind`, so the keys
    /// go INTO those — other unbinds, modes, binds and
    /// `clear-defaults` as written, and the marker still substituted.
    #[test]
    fn a_templates_own_keybinds_are_merged_not_shadowed() {
        let template = r##"keybinds clear-defaults=true {
    unbind "Ctrl q"
    locked {
        bind "Alt Up" { MoveFocus "Up"; }
    }
    unbind "Ctrl w"
}
layout {
    clank_agents
}
"##;
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap();
        let mut want = vec!["Ctrl q".to_string()];
        want.extend(pane_keys());
        assert_eq!(pane_keys_of(&kdl), (1, want), "{kdl}");
        assert!(kdl.contains("keybinds clear-defaults=true {"), "{kdl}");
        assert!(
            kdl.contains("bind \"Alt Up\" { MoveFocus \"Up\"; }"),
            "{kdl}"
        );
        assert!(kdl.contains("unbind \"Ctrl w\"\n"), "{kdl}");
        assert!(kdl.contains("name=\"m\""));
    }

    /// A template whose `keybinds` has modes but no global `unbind`
    /// gets one, first; the modes are as written.
    #[test]
    fn a_templates_keybinds_without_a_global_unbind_gets_one() {
        let template = r##"layout {
    clank_agents
}
keybinds {
    normal {
        bind "Ctrl x" { Quit; }
    }
}
"##;
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap();
        assert_eq!(pane_keys_of(&kdl), (1, pane_keys()), "{kdl}");
        let doc: kdl::KdlDocument = kdl.parse().unwrap();
        let children = doc.get("keybinds").unwrap().children().unwrap().nodes();
        assert_eq!(children[0].name().value(), "unbind");
        assert_eq!(children.len(), 2, "{kdl}");
        assert!(
            kdl.contains(
                "keybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\"\n    normal {\n        bind \"Ctrl x\" { Quit; }\n    }\n}\n"
            ),
            "{kdl}"
        );
    }

    /// Already unbinding the four is nothing to change: byte for byte.
    #[test]
    fn a_template_already_unbinding_the_keys_is_unchanged() {
        let text = "layout {\n    pane\n}\nkeybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\" \"Ctrl q\"\n}\n";
        assert_eq!(with_pane_keys(text).unwrap(), text);
    }

    /// The appended `keybinds` separates itself from a template's
    /// last node, which may end at EOF or with `;` and no newline
    /// (codex on 9081ceb); one that ends with a newline gets no blank
    /// line.
    #[test]
    fn an_appended_keybinds_needs_no_newline_from_the_template() {
        for template in [
            "layout {\n    pane\n}",
            "layout { pane; }",
            "layout { pane; };",
            "layout {\n    pane\n} // the end",
        ] {
            let out = with_pane_keys(template).unwrap();
            assert_eq!(pane_keys_of(&out), (1, pane_keys()), "{template:?}\n{out}");
            let doc: kdl::KdlDocument = out.parse().unwrap();
            assert_eq!(doc.nodes()[0].name().value(), "layout", "{out}");
        }
        assert_eq!(
            with_pane_keys("layout {\n    pane\n}").unwrap(),
            "layout {\n    pane\n}\nkeybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\"\n}\n"
        );
        assert_eq!(
            with_pane_keys("layout {\n    pane\n}\n").unwrap(),
            "layout {\n    pane\n}\nkeybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\"\n}\n"
        );
    }

    /// The inserted `unbind` ends itself: an empty block, a block
    /// whose children sit on one line, and a bare node all come back
    /// as KDL zellij can read, with what they had (codex on 49fc3d8).
    #[test]
    fn an_inserted_unbind_needs_no_newline_from_its_neighbours() {
        for (template, keeps) in [
            ("layout {\n    pane\n}\nkeybinds {}\n", ""),
            ("layout {\n    pane\n}\nkeybinds {\n}\n", ""),
            (
                "layout {\n    pane\n}\nkeybinds { normal { bind \"Ctrl x\" { Quit; }; }; }\n",
                "bind \"Ctrl x\" { Quit; }",
            ),
            ("layout {\n    pane\n}\nkeybinds\n", ""),
        ] {
            let out = with_pane_keys(template).unwrap();
            assert_eq!(pane_keys_of(&out), (1, pane_keys()), "{template:?}\n{out}");
            assert!(out.contains(keeps), "{template:?}\n{out}");
            let doc: kdl::KdlDocument = out.parse().unwrap();
            assert_eq!(doc.nodes()[0].name().value(), "layout");
        }
        assert_eq!(
            with_pane_keys("layout {\n    pane\n}\nkeybinds {}\n").unwrap(),
            "layout {\n    pane\n}\nkeybinds {\n    unbind \"Alt Up\" \"Alt Down\" \"Alt Left\" \"Alt Right\"\n}\n"
        );
    }

    #[test]
    fn template_without_marker_errors_naming_it() {
        let template = "layout {\n    tab {\n        pane\n    }\n}\n";
        let err = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap_err();
        assert!(
            err.to_string().contains("clank_agents"),
            "error must name the marker; got: {err}"
        );
    }

    #[test]
    fn template_with_invalid_kdl_errors_at_compose_not_zellij() {
        let template = "layout { tab { pane "; // unclosed
        let err = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap_err();
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
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], Some(template), LAND).unwrap();
        // The comment + the tab NAME survive untouched; the node is
        // replaced.
        assert!(kdl.contains("// put clank_agents here someday"));
        assert!(kdl.contains("tab name=\"clank_agents\""));
        assert!(kdl.contains("name=\"m\""));
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
            LAND,
        )
        .unwrap();
        // Stage: master gets the big pane.
        assert!(
            kdl.contains("pane size=\"65%\" name=\"alice\""),
            "master is the 65% stage:\n{kdl}"
        );
        // Reviewers stack (one visible, rest collapsed bars).
        assert!(kdl.contains("pane stacked=true"), "reviewer stack:\n{kdl}");
        // The instrument pane is repo-pinned BOTH ways (codex
        // 7d3b5d1): cwd AND --repo, against a repo that is NOT the
        // test process cwd.
        assert!(
            kdl.contains(&format!("name=\"\" cwd=\"{TEST_REPO}\"")),
            "tui pane cwd pinned:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!("args \"tui\" \"--repo\" \"{TEST_REPO}\"")),
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
            PORT,
        )
        .unwrap();
        assert!(
            kdl.contains(&format!("name=\"\" cwd=\"{TEST_REPO}\"")),
            "portrait tui cwd pinned:\n{kdl}"
        );
        assert!(
            kdl.contains(&format!("args \"tui\" \"--repo\" \"{TEST_REPO}\"")),
            "portrait tui --repo pinned:\n{kdl}"
        );
        let _: kdl::KdlDocument = kdl.parse().expect("valid KDL");
    }

    #[test]
    fn orientation_detected_from_terminal_dims() {
        // Landscape dims → outer split is COLUMNS (vertical);
        // portrait dims → ROWS (horizontal). The base tab carries
        // the detected arrangement.
        let land = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
        let port = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, PORT).unwrap();
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
        let built_in = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
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
            LAND,
        )
        .unwrap();
        assert!(
            !user.contains("swap_tiled_layout"),
            "user templates own their swaps:\n{user}"
        );
    }

    #[test]
    fn zero_reviewers_skips_stack_keeps_tui() {
        let kdl = compose_kdl(TEST_TAB, TEST_REPO, "m", &[], None, LAND).unwrap();
        let doc: kdl::KdlDocument = kdl.parse().expect("valid KDL");
        assert!(
            stacked_panes(layout_child(&doc, "tab")).is_empty(),
            "no empty stack region in the tab:\n{kdl}"
        );
        assert!(
            kdl.contains("\"tui\""),
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
            LAND,
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
                text.contains("\"tui\""),
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

    /// An exited pane is still its agent's, and live presence says it is
    /// not running.
    #[test]
    fn an_exited_pane_keeps_its_identity_but_is_not_live() {
        let panes: Vec<ZellijPane> = serde_json::from_str(
            r#"[{"id":4,"exited":true,"title":"💤 codex","tab_id":0,"tab_name":"r"}]"#,
        )
        .unwrap();
        let ws = every(&panes);
        assert_eq!(ws.agent_of(&panes[0]).as_deref(), Some("codex"));
        assert!(live_agent_labels(&panes, &ws).is_empty());
        assert_eq!(panes[0].tab_name(), "r");
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

    /// A tab's name is taken, glyphs aside, whichever repo it is; the
    /// parent-named variant is used, and numbered when that is taken too.
    #[test]
    fn an_unused_tab_name_skips_every_taken_variant() {
        let repo = Path::new("/src/b/clank");
        let taken = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(unused_tab_name(repo, &taken(&["other"])).unwrap(), "clank");
        assert_eq!(
            unused_tab_name(repo, &taken(&["🔨 clank"])).unwrap(),
            "clank (b)"
        );
        assert_eq!(
            unused_tab_name(repo, &taken(&["clank", "clank (b)"])).unwrap(),
            "clank (b) 2"
        );
        let listing = "clank-clank [Created 1m ago]\nclank-clank-b [Created 2m ago] (EXITED)\n";
        assert_eq!(
            unused_session_name(repo, listing).unwrap(),
            "clank-clank-b-2"
        );
    }

    /// A session's name is everything before its `[Created …]`: one with
    /// spaces is kept whole when listed, scanned and matched.
    #[test]
    fn a_session_name_with_spaces_is_kept_whole() {
        let listing = "my work [Created 1m ago] (current)\nclank-x [Created 2m ago]\n";
        assert_eq!(live_session_names(listing), ["my work", "clank-x"]);
        assert_eq!(decide_session(listing, "my work"), SessionPlan::Attach);
        assert_eq!(decide_session(listing, "my"), SessionPlan::Create);
    }

    /// `--all` outside adds to the conventional session only when this
    /// repo lives there; a live one holding another repo is never used.
    #[test]
    fn plan_all_routes_by_session_state_and_ownership() {
        let none = "";
        let live = "clank-proj [Created 1m ago]\n";
        let dead = "clank-proj [Created 1m ago] (EXITED)\n";
        let ours = ["clank-proj".to_string()];
        assert_eq!(
            plan_all(none, "clank-proj", &[]),
            (
                true,
                AllPlan::CreateFresh {
                    delete_first: false
                }
            )
        );
        assert_eq!(
            plan_all(dead, "clank-proj", &[]),
            (true, AllPlan::CreateFresh { delete_first: true })
        );
        assert_eq!(
            plan_all(live, "clank-proj", &ours),
            (true, AllPlan::AddTabs)
        );
        assert_eq!(
            plan_all(live, "clank-proj", &[]),
            (
                false,
                AllPlan::CreateFresh {
                    delete_first: false
                }
            )
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

    // ── agent-promote-zellij-relocation: compose_promote_layout ──

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

        // Geometry absent (older/odd list-panes) → (0,0); the callers
        // fall back to the size rule rather than mis-orienting.
        let no_geo: Vec<ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":false,"is_focused":true,"terminal_command":"x","tab_id":0}
            ]"#,
        )
        .unwrap();
        assert_eq!(tab_dims(&no_geo, 0), (0, 0));
    }

    // ── the live layout (placement-is-a-layout-applied-not-panes-shuffled) ──

    fn panes_of(json: &str) -> Vec<ZellijPane> {
        serde_json::from_str(json).unwrap()
    }

    /// The web page's pane table: the workspace's agents only, by name,
    /// with the width a subscribed viewport does not carry.
    #[test]
    fn the_pane_table_is_this_repos_agents_with_their_sizes() {
        let panes = panes_of(
            r#"[
          {"id":1,"title":"🔨 claude","tab_id":6,"pane_columns":120,"pane_rows":40},
          {"id":2,"title":"codex","tab_id":6,"pane_columns":60,"pane_rows":40,"exited":true},
          {"id":3,"title":"claude","tab_id":7,"pane_columns":60,"pane_rows":40},
          {"id":4,"title":"","terminal_command":"clank status --repo /a --tui","tab_id":6,"pane_columns":60,"pane_rows":10},
          {"id":0,"title":"zellij:tab-bar","is_plugin":true,"tab_id":6}
        ]"#,
        );
        let ws = Workspace::anchored(&panes, Path::new("/a"), set(&["claude", "codex"])).unwrap();
        let table = agent_pane_table(&panes, &ws);
        assert_eq!(
            table,
            vec![
                PaneMeta {
                    id: "terminal_1".into(),
                    label: "claude".into(),
                    columns: 120,
                    rows: 40,
                    exited: false
                },
                PaneMeta {
                    id: "terminal_2".into(),
                    label: "codex".into(),
                    columns: 60,
                    rows: 40,
                    exited: true
                },
            ],
            "another tab's agent and the instrument pane are not agents of this repo"
        );
    }

    /// The subscribe argv names every pane and asks for the escapes;
    /// no panes is no subscription.
    #[test]
    fn the_subscribe_argv_names_every_pane_or_nothing() {
        let argv = subscribe_argv("clank-a", &["terminal_1".into(), "terminal_7".into()]).unwrap();
        assert_eq!(
            argv,
            vec![
                "--session",
                "clank-a",
                "subscribe",
                "--format",
                "json",
                "--ansi",
                "-p",
                "terminal_1",
                "-p",
                "terminal_7"
            ]
        );
        assert_eq!(subscribe_argv("clank-a", &[]), None);
    }

    /// Text, then Enter as byte 13 — two actions — and a message that
    /// starts with a dash is still a message. Empty text says nothing.
    #[test]
    fn saying_something_is_the_text_then_return() {
        let [chars, enter] = say_argvs("terminal_3", "-n hello there").unwrap();
        assert_eq!(
            chars,
            vec![
                "action",
                "write-chars",
                "-p",
                "terminal_3",
                "--",
                "-n hello there"
            ]
        );
        assert_eq!(enter, vec!["action", "write", "-p", "terminal_3", "13"]);
        assert_eq!(say_argvs("terminal_3", ""), None);
        assert_eq!(
            codex_submit_argv("terminal_3"),
            vec![
                "action",
                "write",
                "-p",
                "terminal_3",
                "27",
                "91",
                "49",
                "51",
                "59",
                "49",
                "58",
                "49",
                "117"
            ]
        );
    }

    #[test]
    fn the_stop_key_follows_the_panes_tool() {
        use clank_core::vocab::Tool;
        assert_eq!(
            stop_argv("terminal_15", Tool::Grok),
            ["action", "write", "-p", "terminal_15", "3"]
        );
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
            assert_eq!(
                stop_argv("terminal_16", tool),
                ["action", "write", "-p", "terminal_16", "27"],
                "{tool:?} still receives Escape"
            );
        }
    }

    /// The convention is deterministic per repo, and it is only a
    /// convention: the caller verifies where the panes actually are.
    #[test]
    fn the_repo_session_name_is_the_open_convention() {
        assert_eq!(
            repo_session_name(Path::new("/x/y/penlock-experiment")),
            "clank-penlock-experiment"
        );
    }

    /// The stage is read from GEOMETRY — the biggest agent pane in its
    /// tab — never from a title. A pane titled `(master)` that sits in
    /// the stack is not the stage, and a promoted pane on the stage is,
    /// whatever its title still says.
    #[test]
    fn the_stage_is_the_biggest_agent_pane_not_the_titled_one() {
        let panes = panes_of(
            r#"[
          {"id":1,"title":"claude (master)","terminal_command":"clank agent start claude --repo /a","tab_id":6,"pane_x":100,"pane_columns":60,"pane_y":0,"pane_rows":20},
          {"id":2,"title":"codex (reviewer)","terminal_command":"clank agent start codex --repo /a","tab_id":6,"pane_x":0,"pane_columns":100,"pane_y":0,"pane_rows":40},
          {"id":3,"title":"other (master)","terminal_command":"clank agent start other --repo /b","tab_id":9,"pane_x":0,"pane_columns":100,"pane_y":0,"pane_rows":40}
        ]"#,
        );
        let ws = Workspace {
            tab: 6,
            candidates: set(&["claude", "codex", "other"]),
        };
        assert_eq!(
            agent_pane_pairs(&panes, &ws),
            vec![("claude".to_string(), false), ("codex".to_string(), true)]
        );
        // Two panes of equal size stage nobody: the answer must be
        // positive evidence, never a tie broken by listing order.
        let tie = panes_of(
            r#"[
          {"id":1,"title":"claude","tab_id":6,"pane_columns":50,"pane_rows":20},
          {"id":2,"title":"codex","tab_id":6,"pane_columns":50,"pane_rows":20}
        ]"#,
        );
        assert!(agent_pane_pairs(&tie, &ws).iter().all(|(_, s)| !s));
    }

    #[test]
    fn the_repo_tab_is_where_its_instrument_pane_is() {
        let panes = panes_of(LANDSCAPE_HEALTHY_JSON);
        assert_eq!(repo_tab_id(&panes, Path::new("/a")), Some(6));
        assert_eq!(repo_tab_id(&panes, Path::new("/elsewhere")), None);
        let agents_only = panes_of(
            r#"[{"id":1,"terminal_command":"clank agent start claude --repo /a","tab_id":4}]"#,
        );
        assert_eq!(repo_tab_id(&agents_only, Path::new("/a")), None);
    }

    /// The tab's SHAPE answers, and the arrangement inside it does
    /// not get a vote — these two fixtures are each arranged AGAINST
    /// their extent, which is exactly how the old reading justified
    /// itself and exactly what made a swap flip the layout.
    #[test]
    fn a_tabs_extent_decides_its_orientation_whatever_the_panes_say() {
        // 100x60 — taller than 2:1, so portrait — arranged landscape:
        // the instrument pane beside the stage.
        let arranged_landscape = panes_of(
            r#"[
          {"id":1,"terminal_command":"clank agent start claude --repo /a","tab_id":6,"pane_x":0,"pane_columns":60,"pane_y":0,"pane_rows":60},
          {"id":2,"terminal_command":"clank agent start codex --repo /a","tab_id":6,"pane_x":60,"pane_columns":40,"pane_y":0,"pane_rows":30},
          {"id":3,"terminal_command":"clank status --repo /a --tui","tab_id":6,"pane_x":60,"pane_columns":40,"pane_y":30,"pane_rows":30}
        ]"#,
        );
        assert_eq!(
            tab_orientation(&arranged_landscape, 6),
            Orientation::Portrait,
            "a tall tab is portrait however its panes are sitting"
        );
        // 200x50 — wider than 2:1, so landscape — arranged portrait.
        let arranged_portrait = panes_of(
            r#"[
          {"id":1,"terminal_command":"clank agent start claude --repo /a","tab_id":3,"pane_x":0,"pane_columns":200,"pane_y":0,"pane_rows":30},
          {"id":2,"terminal_command":"clank agent start codex --repo /a","tab_id":3,"pane_x":0,"pane_columns":120,"pane_y":30,"pane_rows":20},
          {"id":3,"terminal_command":"clank status --repo /a --tui","tab_id":3,"pane_x":120,"pane_columns":80,"pane_y":30,"pane_rows":20}
        ]"#,
        );
        assert_eq!(
            tab_orientation(&arranged_portrait, 3),
            Orientation::Landscape,
            "and a wide one is landscape"
        );
        // The boundary detect draws, in the units it draws it in:
        // cells are ~2:1, so 2:1 columns:rows is the first landscape.
        assert_eq!(Orientation::detect((100, 50)), Orientation::Landscape);
        assert_eq!(Orientation::detect((99, 50)), Orientation::Portrait);
    }

    /// THE reported failure. A master swap adds a pane before any
    /// layout places it, and `new-pane` puts it where zellij likes —
    /// here beside the stage, which the old reading called landscape
    /// on a portrait tab. The answer must not move: a re-layout
    /// provoked by a roster change composes the same shape the tab
    /// already had.
    #[test]
    fn adding_a_pane_does_not_change_the_tabs_orientation() {
        // 80x137: portrait, stage over instrument.
        let before = panes_of(
            r#"[
          {"id":1,"terminal_command":"clank agent start kimi --repo /a","tab_id":6,"pane_x":0,"pane_columns":80,"pane_y":0,"pane_rows":90},
          {"id":3,"terminal_command":"clank status --repo /a --tui","tab_id":6,"pane_x":0,"pane_columns":80,"pane_y":90,"pane_rows":47}
        ]"#,
        );
        // The swap: kimi's pane is gone, claude's is new and unplaced —
        // smaller than the instrument pane, and beside it.
        let after = panes_of(
            r#"[
          {"id":9,"terminal_command":"clank agent start claude --repo /a","tab_id":6,"pane_x":0,"pane_columns":40,"pane_y":0,"pane_rows":90},
          {"id":3,"terminal_command":"clank status --repo /a --tui","tab_id":6,"pane_x":40,"pane_columns":40,"pane_y":0,"pane_rows":137}
        ]"#,
        );
        assert_eq!(tab_orientation(&before, 6), Orientation::Portrait);
        assert_eq!(
            tab_orientation(&after, 6),
            tab_orientation(&before, 6),
            "the tab is the same size, so it is the same shape — the \
             arrangement mid-swap is not evidence of anything"
        );
    }

    /// Drive the transaction with scripted outcomes and record what it
    /// ran: `(actions, result)`.
    fn transact(
        tab: u32,
        actives: Vec<Option<u32>>,
        outcomes: Vec<bool>,
    ) -> (Vec<Vec<String>>, bool) {
        let mut ran: Vec<Vec<String>> = Vec::new();
        let mut actives = actives.into_iter();
        let mut outcomes = outcomes.into_iter();
        let result = override_transaction(
            tab,
            "/l.kdl",
            |args| {
                ran.push(args.iter().map(|a| a.to_string()).collect());
                outcomes.next().unwrap_or(true)
            },
            || actives.next().unwrap_or(None),
        );
        (ran, result)
    }

    fn go_to(tab: u32) -> Vec<String> {
        vec!["go-to-tab-by-id".to_string(), tab.to_string()]
    }

    /// The tab is focused BY ID — names may repeat across worktrees —
    /// only when it is not already active, the override carries every
    /// flag (each a measured necessity), and the previous tab is
    /// restored after.
    #[test]
    fn the_override_focuses_by_id_overrides_the_active_tab_only_and_restores() {
        let (ran, ok) = transact(6, vec![Some(2), Some(6)], vec![]);
        assert!(ok);
        assert_eq!(
            ran,
            vec![
                go_to(6),
                override_layout_argv("/l.kdl")
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>(),
                go_to(2),
            ]
        );
        assert!(
            ran.iter().flatten().all(|a| a != "go-to-tab-name"),
            "never by name"
        );
        let (ran, ok) = transact(6, vec![Some(6)], vec![]);
        assert!(ok);
        assert_eq!(ran.len(), 1, "already active: no focus dance");
        assert_eq!(ran[0][0], "override-layout");
    }

    /// Nothing here is fail-open (codex on d758045): `override-layout`
    /// acts on whatever tab is active, so an active tab that cannot be
    /// identified, a focus that fails, or a focus that does not read
    /// back as the target each refuse the override — and a focus that
    /// moved is undone even when the override is refused or fails.
    #[test]
    fn the_override_is_a_checked_transaction_that_never_lands_on_the_wrong_tab() {
        // The active tab is unknown: refuse, touch nothing.
        let (ran, ok) = transact(6, vec![None], vec![]);
        assert!(!ok && ran.is_empty(), "{ran:?}");

        // The focus action fails: no override, nothing to restore.
        let (ran, ok) = transact(6, vec![Some(2)], vec![false]);
        assert!(!ok);
        assert_eq!(ran, vec![go_to(6)]);

        // The focus "succeeded" but the active tab reads back as
        // another: no override, and the previous tab is restored.
        let (ran, ok) = transact(6, vec![Some(2), Some(3)], vec![true, true]);
        assert!(!ok);
        assert_eq!(ran, vec![go_to(6), go_to(2)]);

        // The override itself fails: reported, and the previous tab is
        // restored all the same.
        let (ran, ok) = transact(6, vec![Some(2), Some(6)], vec![true, false, true]);
        assert!(!ok);
        assert_eq!(ran.len(), 3);
        assert_eq!(ran[1][0], "override-layout");
        assert_eq!(ran[2], go_to(2));
    }

    /// The live layout is the SAME composition `clank open` launches
    /// from — same slots, same commands, both swap variants — and reads
    /// the same template source, so a workspace opened from a marker
    /// template is re-laid with it.
    #[test]
    fn the_live_layout_is_opens_composition_from_the_same_template_source() {
        let repo = Path::new("/tmp/live-repo");
        let built_in = compose_live_layout(
            repo,
            None,
            "codex",
            &["claude".to_string()],
            Orientation::Landscape,
        )
        .unwrap();
        let expected = compose_kdl(
            "live-repo",
            "/tmp/live-repo",
            "codex",
            &["claude".to_string()],
            None,
            LAND,
        )
        .unwrap();
        assert_eq!(built_in, expected);
        assert_eq!(built_in.matches("swap_tiled_layout").count(), 2);

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            serde_json::json!({"zellij": {"layout": "layout {\n    tab name=\"MINE\" {\n        clank_agents\n    }\n}\n"}})
                .to_string(),
        )
        .unwrap();
        let templated = compose_live_layout(
            repo,
            Some(home.path()),
            "codex",
            &["claude".to_string()],
            Orientation::Landscape,
        )
        .unwrap();
        assert!(templated.contains("MINE"));
        assert!(
            !templated.contains("swap_tiled_layout"),
            "a template's contract: no generated swaps"
        );
        assert!(
            templated.contains("\"codex\""),
            "the roster still lands at the marker"
        );
    }

    /// A pane of the scan, in `session`, a tab with `id` at `position`.
    fn pane_in(
        session: &str,
        id: u32,
        position: usize,
        tab: &str,
        command: &str,
        exited: bool,
    ) -> SessionPane {
        SessionPane {
            session: session.into(),
            pane: serde_json::from_value(serde_json::json!({
                "id": 1,
                "terminal_command": command,
                "tab_id": id,
                "tab_position": position,
                "tab_name": tab,
                "exited": exited,
            }))
            .unwrap(),
        }
    }

    fn agent(repo: &Path, label: &str) -> String {
        format!("clank agent start {label} --repo {}", repo.display())
    }

    fn scan_of(panes: Vec<SessionPane>) -> SessionScan {
        SessionScan {
            panes,
            ..Default::default()
        }
    }

    /// Open targets are found by the path their panes name, whatever the
    /// agent's label or the status pane's spelling, never by an exited pane
    /// or a same-named repo elsewhere; and each is described where it is,
    /// by the tab's position, with a way there from wherever the caller is.
    #[test]
    fn open_targets_are_found_by_path_and_described_from_the_caller() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("a/proj");
        let twin = root.path().join("b/proj");
        for dir in [&repo, &twin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let scan = scan_of(vec![
            pane_in(
                "work space",
                7,
                2,
                "🔨 proj",
                &agent(&repo, "renamed"),
                false,
            ),
            pane_in(
                "work space",
                7,
                2,
                "🔨 proj",
                &format!("clank status --repo {} --tui", repo.display()),
                false,
            ),
            pane_in("other", 1, 0, "proj", &agent(&repo, "codex"), false),
            pane_in("other", 2, 1, "dead", &agent(&twin, "codex"), true),
        ]);
        let found = locations(&scan, &repo);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            (
                found[0].session.as_str(),
                found[0].position,
                found[0].agents.clone()
            ),
            ("work space", Some(2), vec!["renamed".to_string()])
        );
        assert!(
            locations(&scan, &twin).is_empty(),
            "exited, and the twin is not the repo"
        );

        let outside = describe(&repo, &found, false, None);
        assert!(
            outside.contains("tab 3 `proj`"),
            "the position, not the id: {outside}"
        );
        assert!(outside.contains("zellij attach 'work space'"), "{outside}");
        let same = describe(&repo, &found[..1], true, Some("work space"));
        assert!(
            same.contains("switch to tab 3 `proj` in this session"),
            "{same}"
        );
        let foreign = describe(&repo, &found[..1], true, Some("elsewhere"));
        assert!(
            foreign.contains("another session") && foreign.contains("zellij attach 'work space'"),
            "the session first: {foreign}"
        );

        let caller = SessionTarget::Current {
            name: Some("other".into()),
        };
        let sorted = sort_targets(&scan, vec![repo.clone(), twin.clone()], Some(&caller));
        assert_eq!(sorted.missing, [twin]);
        assert_eq!(sorted.reports.len(), 1);
        assert!(
            sorted.reports[0].contains("session `work space`")
                && sorted.reports[0].contains("session `other`")
        );
    }

    fn args(repo: &Path) -> OpenZellijArgs {
        OpenZellijArgs {
            repo: Some(repo.to_path_buf()),
            fork: None,
            pr: None,
            all: false,
            print: false,
        }
    }

    /// The production entry point: a repo open anywhere is reported and
    /// nothing is queried, written or spawned; a scan that did not see every
    /// session refuses, naming what it missed.
    #[tokio::test]
    async fn open_reports_an_open_repo_and_refuses_an_incomplete_scan() {
        let root = tempfile::tempdir().unwrap();
        let repo = openable_repo(root.path(), "proj");
        let mut io = FakeOpenIo {
            scan: scan_of(vec![pane_in(
                "elsewhere",
                4,
                0,
                "proj",
                &agent(&repo, "codex"),
                false,
            )]),
            ..Default::default()
        };
        run_with(&mut io, &args(&repo), Some(current_session()))
            .await
            .unwrap();
        assert_eq!(
            io.calls.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["scan"]
        );
        assert!(
            io.reports[0].contains("session `elsewhere`"),
            "{:?}",
            io.reports
        );
        assert!(!layout_file_path(&repo).exists());

        let mut io = FakeOpenIo {
            scan: SessionScan {
                unanswered: vec!["stale one".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let error = run_with(&mut io, &args(&repo), None).await.unwrap_err();
        assert!(format!("{error:#}").contains("`stale one`"), "{error:#}");
        assert!(io.spawned().is_empty());

        let mut io = FakeOpenIo {
            scan: SessionScan {
                listing_failed: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(run_with(&mut io, &args(&repo), None).await.is_err());
        assert!(io.spawned().is_empty());
    }

    /// `/a/proj` open, `/b/proj` requested: in the caller's session, the
    /// same-named tab does not stop it, and it opens in a tab of its own;
    /// outside, `clank-proj` holding `/a/proj` is never attached to.
    #[tokio::test]
    async fn a_same_named_repo_elsewhere_never_stands_for_the_target() {
        let root = tempfile::tempdir().unwrap();
        let a = openable_repo(&root.path().join("a"), "proj");
        let b = openable_repo(&root.path().join("b"), "proj");
        let open_a = || {
            scan_of(vec![pane_in(
                "user-workspace",
                1,
                0,
                "proj",
                &agent(&a, "codex"),
                false,
            )])
        };

        let mut io = FakeOpenIo::with([query_output(0, "proj\n", "")]);
        io.scan = open_a();
        run_with(&mut io, &args(&b), Some(current_session()))
            .await
            .unwrap();
        assert!(io.reports.is_empty());
        let b = dunce::canonicalize(&b).unwrap();
        assert_eq!(
            io.spawned(),
            [in_session_tab_argv(&layout_file_path(&b)).as_slice()]
        );
        let layout = std::fs::read_to_string(layout_file_path(&b)).unwrap();
        assert!(layout.contains("tab name=\"proj (b)\""), "{layout}");
        std::fs::remove_file(layout_file_path(&b)).unwrap();

        let mut io = FakeOpenIo::with([query_output(0, "clank-proj [Created 1m ago]\n", "")]);
        io.scan = scan_of(vec![pane_in(
            "clank-proj",
            1,
            0,
            "proj",
            &agent(&a, "codex"),
            false,
        )]);
        run_with(&mut io, &args(&b), None).await.unwrap();
        assert_eq!(
            io.spawned(),
            [create_argv(&layout_file_path(&b), "clank-proj-b").as_slice()]
        );
    }

    /// `--all` targets sharing a basename are each their own: both open,
    /// in tabs of their own names.
    #[tokio::test]
    async fn all_targets_sharing_a_basename_each_get_a_tab() {
        let root = tempfile::tempdir().unwrap();
        let source = openable_repo(root.path(), "proj");
        let a = openable_repo(&root.path().join("a"), "fork");
        let b = openable_repo(&root.path().join("b"), "fork");
        let mut io = FakeOpenIo::with([query_output(1, NO_SESSIONS, "")]);
        open_all(&mut io, &source, &[a, b], &[], false)
            .await
            .unwrap();
        let layout = std::fs::read_to_string(layout_file_path(&source)).unwrap();
        assert!(
            layout.contains("tab name=\"fork\"") && layout.contains("tab name=\"fork (b)\""),
            "{layout}"
        );
    }

    /// A session's state is read from its listing's suffix, never its
    /// name: one named with `EXITED` in it is live and scanned; a line that
    /// cannot be read is a session not seen, never an absent one.
    #[test]
    fn a_sessions_state_is_its_suffix_not_its_name() {
        let listing = "EXITED research [Created 1m ago] (current)\n\
                       research [Created (EXITED note [Created 1m ago] (current)\n\
                       gone [Created 2d ago] (EXITED - attach to resurrect)\n\
                       garbled\n\
                       unclosed [Created 1m ago\n";
        assert_eq!(
            live_session_names(listing),
            ["EXITED research", "research [Created (EXITED note"]
        );
        assert_eq!(
            unreadable_sessions(listing),
            ["garbled", "unclosed [Created 1m ago"]
        );
        assert_eq!(
            decide_session(listing, "EXITED research"),
            SessionPlan::Attach
        );
        assert_eq!(
            decide_session(listing, "gone"),
            SessionPlan::DeleteDeadThenCreate
        );
        let scan = scan_sessions(
            std::time::Duration::from_millis(10),
            || Some(listing.to_string()),
            |_| None,
        );
        assert!(
            scan.unanswered.contains(&"garbled".to_string())
                && scan.unanswered.contains(&"EXITED research".to_string()),
            "{scan:?}"
        );
    }

    /// A pane is the target's whatever its label or path holds: spaces, the
    /// ` --repo ` delimiter itself, or a different spelling of the same
    /// directory.
    #[test]
    fn a_pane_is_matched_to_its_repo_whatever_the_path_or_label_holds() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("my dir/proj --repo part");
        std::fs::create_dir_all(&repo).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();
        let pane = |command: String| -> ZellijPane {
            serde_json::from_value(serde_json::json!({"id": 1, "terminal_command": command}))
                .unwrap()
        };
        let a = pane(agent(&repo, "two words"));
        assert_eq!(pane_for(&a, &repo), Some(Some("two words")));
        assert_eq!(pane_for(&a, &alias), Some(Some("two words")), "canonically");
        let tui = pane(format!("clank tui --repo {}", alias.display()));
        assert_eq!(pane_for(&tui, &repo), Some(None));
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(pane_for(&a, &other), None);

        let scan = scan_of(vec![pane_in(
            "away",
            1,
            0,
            "proj",
            &agent(&repo, "codex"),
            false,
        )]);
        assert_eq!(locations(&scan, &repo).len(), 1);
    }

    /// `--all` through the production entry point: a worktree already open
    /// in another session is reported, and only the missing one is composed
    /// and opened.
    #[tokio::test]
    async fn all_reports_what_is_open_and_opens_only_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let source = root.path().join("proj");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        git(
            &source,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = root.path().join("wt");
        git(
            &source,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
        );
        for dir in [&source, &wt] {
            openable_repo(
                dir.parent().unwrap(),
                dir.file_name().unwrap().to_str().unwrap(),
            );
        }
        let mut io = FakeOpenIo::with([query_output(1, "", NO_SESSIONS)]);
        io.scan = scan_of(vec![pane_in(
            "away",
            1,
            0,
            "proj",
            &agent(&source, "codex"),
            false,
        )]);
        let args = OpenZellijArgs {
            all: true,
            ..args(&source)
        };
        run_with(&mut io, &args, None).await.unwrap();
        assert_eq!(io.reports.len(), 1);
        assert!(io.reports[0].contains("session `away`"), "{:?}", io.reports);
        let source = dunce::canonicalize(&source).unwrap();
        assert_eq!(
            io.spawned(),
            [create_argv(&layout_file_path(&source), "clank-proj").as_slice()]
        );
        let layout = std::fs::read_to_string(layout_file_path(&source)).unwrap();
        assert!(
            layout.contains("tab name=\"wt\"") && !layout.contains("tab name=\"proj\""),
            "{layout}"
        );
    }
}
