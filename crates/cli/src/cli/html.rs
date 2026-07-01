//! `clank html` — render the event log + current status to a
//! static HTML site at `.clank/html/`.
//!
//! Single-shot build: gather status, fold the timeline, render
//! one page per commit. No server, no watcher (yet — see the
//! plan's "Out of scope"). With `--open`, launches the host's
//! file:// opener against the generated `index.html`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use clank_core::feedback_body::FeedbackBody;
use clank_core::ids::CommitSha;
use clank_core::repo_state::LogEvent;
use clank_core::vocab::Verdict;
use clank_core::wait::PlanWorkState;

use super::{HtmlArgs, HtmlCmd, repo_basename, resolve_repo};
use crate::cli::status::StatusSnapshot;

/// Bump when the rendered markup, CSS, JS, or output
/// directory layout under `.clank/html/` changes shape. A
/// bump forces a full rebuild of every per-commit and
/// per-plan page on the next `clank html` invocation.
const BUILDER_VERSION: &str = "2";
const TOP_N_FEEDBACK_RECHECK: usize = 10;

pub async fn run(args: HtmlArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let out_dir = repo.join(".clank/html");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let progress = Progress::new(args.quiet);

    // `clank html open <target>` (without --print-path) launches the browser.
    // Route it through the loading-page flow so a slow first build shows the
    // browser a live progress page IMMEDIATELY instead of nothing.
    if let Some(HtmlCmd::Open(open_args)) = &args.command
        && !open_args.print_path
    {
        return run_open(
            &repo,
            &basename,
            &out_dir,
            home.as_deref(),
            args.rebuild,
            &progress,
            open_args,
        )
        .await;
    }

    // Otherwise: build first, then note the output (bare `clank html`) or
    // print the resolved target path (`open --print-path`).
    build_site(
        &repo,
        &basename,
        &out_dir,
        home.as_deref(),
        args.rebuild,
        &progress,
        None,
    )
    .await?;
    progress.finish();

    // Under `--print-path`, stdout must contain ONLY the resolved path so
    // callers can use `$(clank html open <plan> --print-path)` verbatim; the
    // build-info line goes to stderr (codex dfe596d).
    if let Some(HtmlCmd::Open(open_args)) = &args.command {
        eprintln!("wrote {}", out_dir.display());
        let target = resolve_open_target(&repo, &basename, &out_dir, open_args).await?;
        println!("{}", target.display());
    } else {
        println!("wrote {}", out_dir.display());
    }
    Ok(())
}

/// The browser-launch path of `clank html open`. Fast when the target page
/// already exists (build → open); for a missing target (the slow first-time
/// case) it opens the browser on a live [`LoadingPage`] FIRST, then builds
/// behind it — the page auto-redirects into the report when the build lands,
/// or shows an error state if it fails (the only feedback when the TUI spawns
/// this detached).
async fn run_open(
    repo: &Path,
    basename: &str,
    out_dir: &Path,
    home: Option<&Path>,
    rebuild: bool,
    progress: &Progress,
    open_args: &crate::cli::HtmlOpenArgs,
) -> anyhow::Result<()> {
    let target = resolve_open_target_path(repo, basename, out_dir, open_args).await?;

    if target.exists() {
        build_site(repo, basename, out_dir, home, rebuild, progress, None).await?;
        progress.finish();
        println!("wrote {}", out_dir.display());
        return launch_opener(&target);
    }

    let loading = LoadingPage::new(
        out_dir.join("_loading.html"),
        target_rel_url(out_dir, &target),
        open_target_label(open_args),
    );
    launch_opener(&loading.path)?;
    println!("wrote {}", out_dir.display());
    match build_site(
        repo,
        basename,
        out_dir,
        home,
        rebuild,
        progress,
        Some(&loading),
    )
    .await
    {
        Ok(()) if target.exists() => {
            progress.finish();
            loading.finish_ok();
            Ok(())
        }
        Ok(()) => {
            progress.finish();
            loading.finish_err(
                "that page isn't part of the built site yet — run `clank html --rebuild`",
            );
            Ok(())
        }
        Err(e) => {
            progress.finish();
            loading.finish_err(&format!("build failed: {e}"));
            Err(e)
        }
    }
}

/// The target page RELATIVE to the html root, for the loading page's
/// redirect meta — `_loading.html` and the target both live under `out_dir`,
/// so the redirect must be `commit/<sha>.html` (relative), never absolute or
/// wrong-based (a classic file:// bug). Forward slashes on every platform.
fn target_rel_url(out_dir: &Path, target: &Path) -> String {
    target
        .strip_prefix(out_dir)
        .unwrap_or(target)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A short label for the open target: `commit a1b2c3d` / `plan foo`.
fn open_target_label(args: &crate::cli::HtmlOpenArgs) -> String {
    if let Some(sha) = &args.commit {
        format!("commit {}", &sha[..sha.len().min(9)])
    } else if let Some(plan) = &args.plan {
        format!("plan {plan}")
    } else {
        "the report".to_string()
    }
}

/// Generate the HTML site into `out_dir` (quietly), via the same
/// `build_site` core `run()` uses. `home` is explicit so
/// in-process callers (tests) control user-scope resolution
/// without reading `$HOME`. Plan: dogfood-init-setup-in-tests
/// (Phase B).
pub async fn generate(
    repo: &Path,
    out_dir: &Path,
    home: Option<&Path>,
    rebuild: bool,
) -> anyhow::Result<()> {
    let basename = repo_basename(repo)?;
    let progress = Progress::new(true);
    build_site(repo, &basename, out_dir, home, rebuild, &progress, None).await
}

/// Resolve the target file the browser (or `--print-path`) lands on.
/// When `plan` is set, returns `<out_dir>/plan/<stem>.html` after
/// resolving the plan name via the shared `plan_resolve` helper —
/// errors with the established "not a known plan" diagnostic on
/// miss (parity with `clank diff <plan>`). When `plan` is None,
/// returns `<out_dir>/index.html` (current backward-compat).
async fn resolve_open_target(
    repo: &Path,
    basename: &str,
    out_dir: &Path,
    args: &crate::cli::HtmlOpenArgs,
) -> anyhow::Result<std::path::PathBuf> {
    let page = resolve_open_target_path(repo, basename, out_dir, args).await?;
    // Commit target outside the incremental window has no built page — error
    // with a rebuild hint rather than opening a 404 (the browser-launch path
    // handles the same case via the loading page's error state instead).
    if args.commit.is_some() && !page.exists() {
        let name = page
            .file_stem()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        anyhow::bail!(
            "no HTML page for commit {} yet (outside the incremental window) — run `clank html --rebuild`",
            &name[..name.len().min(12)]
        );
    }
    Ok(page)
}

/// Resolve the target page PATH without checking that it exists yet (the
/// loading-page flow may be about to build it). Short shas / plan names are
/// resolved to their full page path.
async fn resolve_open_target_path(
    repo: &Path,
    basename: &str,
    out_dir: &Path,
    args: &crate::cli::HtmlOpenArgs,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(raw) = args.commit.as_deref() {
        let sha = crate::git_io::resolve_commit(repo, raw)
            .ok_or_else(|| anyhow::anyhow!("not a known commit: `{raw}`"))?;
        return Ok(out_dir.join(format!("commit/{}.html", sha.as_str())));
    }
    match args.plan.as_deref() {
        Some(raw) => {
            let state =
                crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display())
                    })?;
            let key = crate::cli::plan_resolve::resolve_plan(&state, basename, Some(raw))?;
            Ok(out_dir.join(format!("plan/{}.html", key.as_str())))
        }
        None => Ok(out_dir.join("index.html")),
    }
}

/// The command-line target for opening a plan/commit page in the browser.
pub enum HtmlOpenTarget<'a> {
    Plan(&'a str),
    Commit(&'a str),
}

/// Build the argv (after the program name) the TUI spawns to open a
/// plan/commit page: `html --repo <repo> --quiet open <target>`. The parent
/// `html` flags (`--repo`, `--quiet`) MUST precede the `open` subcommand —
/// clap parses them at the parent, so `open <plan> --quiet` fails. Pure and
/// pinned by `html_open_argv_parses` so a flags-after-subcommand regression
/// fails at build time rather than silently at runtime (the TUI spawns
/// detached with nulled output). See `status_tui`'s overlay handler.
/// `rebuild` forces a full re-render (parent `--rebuild`) — used when the
/// target's incremental page is missing so the open still succeeds.
pub fn html_open_argv(repo: &Path, target: HtmlOpenTarget, rebuild: bool) -> Vec<String> {
    let mut argv = vec![
        "html".to_string(),
        "--repo".to_string(),
        repo.to_string_lossy().into_owned(),
        "--quiet".to_string(),
    ];
    if rebuild {
        argv.push("--rebuild".to_string());
    }
    argv.push("open".to_string());
    match target {
        HtmlOpenTarget::Plan(stem) => argv.push(stem.to_string()),
        HtmlOpenTarget::Commit(sha) => {
            argv.push("--commit".to_string());
            argv.push(sha.to_string());
        }
    }
    argv
}

async fn build_site(
    repo: &Path,
    basename: &str,
    out_dir: &Path,
    home: Option<&Path>,
    mut force_rebuild: bool,
    progress: &Progress,
    loading: Option<&LoadingPage>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(out_dir.join("commit"))?;
    std::fs::create_dir_all(out_dir.join("plan"))?;

    let status = StatusSnapshot::build_async(
        repo,
        basename,
        home,
        crate::rebuild::CachePolicy::Use,
        None,
        false,
    )
    .await?;

    let head_sha = head_sha(repo)?;

    // Decide whether we can do an incremental fold from the
    // prior built head. Requirements: (1) not --rebuild, (2)
    // existing index has a marker, (3) cached event log on
    // disk, (4) prev_head is a strict ancestor of head_sha.
    //
    // A stale prior build (version mismatch) is upgraded to
    // a full rebuild here, so every per-commit and per-plan
    // page on disk is rewritten under the new builder
    // version's markup/CSS/JS/layout.
    let prior_head = if force_rebuild {
        None
    } else {
        match read_prior_build(out_dir) {
            PriorBuild::Fresh { sha } => Some(sha),
            PriorBuild::Stale => {
                force_rebuild = true;
                None
            }
            PriorBuild::None => None,
        }
    };
    // Three cases for the event log:
    //
    // 1. Cache hit, prev == head: feedback-only rebuild
    //    (e.g. a new FINISHED landed but no new commit).
    //    Reuse the cached events verbatim; no fold needed.
    // 2. Cache hit, prev is a strict ancestor of head:
    //    incremental slice via rebuild_from(prev, head),
    //    appended to the cached log.
    // 3. Otherwise (cache miss / --rebuild / not an
    //    ancestor): full fold from root.
    //
    // We also retain the PRIOR event list so the top-N
    // feedback re-check below targets the prior timeline's
    // top — not the current one. After a slice that adds
    // more than TOP_N commits, the prior top commits drop
    // out of the new top-N, but their feedback may still
    // have changed and we need to refresh those pages.
    let cached_prior = read_events_cache(out_dir);
    let (events, prior_events): (Vec<LogEvent>, Option<Vec<LogEvent>>) =
        match (prior_head.as_deref(), head_sha.as_ref(), cached_prior) {
            (Some(prev), Some(head), Some(prior_events)) if prev == head.as_str() => {
                (prior_events.clone(), Some(prior_events))
            }
            (Some(prev), Some(head), Some(prior_events))
                if prev_is_ancestor(repo, prev, head.as_str()) =>
            {
                let prev_sha = clank_core::ids::CommitSha::parse(prev)
                    .map_err(|e| anyhow::anyhow!("parse prior head sha `{prev}`: {e}"))?;
                let (_state, slice) =
                    crate::rebuild::rebuild_from(repo, Some(&prev_sha), head).await?;
                let mut combined = prior_events.clone();
                combined.extend(slice);
                (combined, Some(prior_events))
            }
            (_, Some(head), _) => {
                let (_state, events) = crate::rebuild::rebuild_from(repo, None, head).await?;
                (events, None)
            }
            _ => (Vec::new(), None),
        };

    let event_shas: Vec<CommitSha> = events.iter().map(event_sha).cloned().collect();
    let reviews = collect_reviews(repo, &event_shas);
    let subjects = collect_subjects(repo, head_sha.as_ref());

    // Common shared CSS file.
    std::fs::write(out_dir.join("style.css"), CSS)?;

    // Persist the event log so the next incremental run can
    // skip the full fold.
    write_events_cache(out_dir, &events)?;

    // Index page — always full re-render so the status header,
    // verdict marks, and umbrella grouping reflect current
    // state regardless of slice contents.
    let head_str = head_sha.as_ref().map(|s| s.as_str());
    let index_html = render_index(&status, &events, &reviews, &subjects, head_str);
    std::fs::write(out_dir.join("index.html"), index_html)?;

    // Per-commit pages: write every commit whose page is
    // missing on disk + the top-N most-recent events of the
    // PRIOR timeline (their feedback may have updated even
    // if a fat slice has since pushed them out of the
    // current top-N). On a full rebuild (no prior cache) the
    // top-N of the current events is used — equivalent to
    // the prior, since they overlap.
    let top_n_source: &[LogEvent] = prior_events.as_deref().unwrap_or(&events);
    let top_n_sha_set: std::collections::HashSet<String> = top_n_source
        .iter()
        .rev()
        .take(TOP_N_FEEDBACK_RECHECK)
        .map(|e| event_sha(e).as_str().to_string())
        .collect();
    let writes_needed: Vec<&LogEvent> = events
        .iter()
        .filter(|e| {
            if force_rebuild {
                return true;
            }
            let s = event_sha(e).as_str();
            let exists = out_dir.join(format!("commit/{s}.html")).exists();
            !exists || top_n_sha_set.contains(s)
        })
        .collect();

    // Plan-page set. Hoisted above the commit write loop (it only needs
    // `events` + `writes_needed`) so the loading page can announce a single
    // accurate page total up front.
    //
    // On full rebuild every page is rewritten. On incremental, affected =
    // (slice plans) ∪ (plans whose commits are in writes_needed). The second
    // term covers feedback-only rebuilds at the same HEAD: a new review on a
    // top-N commit changes the verdict marks that the plan page also renders,
    // so the page must refresh too. Cost: an unrelated slice re-renders plan
    // pages whose commits happen to be in top-N — acceptable; they age out of
    // top-N quickly and the output is identical when reviews didn't change.
    let plan_buckets = collect_plan_buckets(&events);
    let affected: std::collections::HashSet<String> = if force_rebuild {
        plan_buckets.keys().cloned().collect()
    } else {
        writes_needed
            .iter()
            .filter_map(|e| event_plan(e).map(str::to_string))
            .collect()
    };
    let plan_writes: Vec<(&String, &PlanLifecycle)> = plan_buckets
        .iter()
        .filter(|(stem, _)| affected.contains(stem.as_str()))
        .collect();

    // Total pages = index (already written above) + commit pages + plan
    // pages. Count the index as the first tick.
    if let Some(l) = loading {
        l.set_total(1 + writes_needed.len() + plan_writes.len());
        l.tick();
    }

    progress.begin("writing pages", writes_needed.len());
    for (i, event) in writes_needed.iter().enumerate() {
        let sha = event_sha(event);
        let path = out_dir.join(format!("commit/{}.html", sha.as_str()));
        let page = render_commit_page(repo, event, &reviews);
        std::fs::write(&path, page)?;
        progress.tick(i + 1);
        if let Some(l) = loading {
            l.tick();
        }
    }
    progress.end();

    // Plan pages. One per active or finished plan.
    let work_by_stem: BTreeMap<String, &PlanWorkState> = status
        .plans
        .iter()
        .map(|p| (p.plan.as_str().to_string(), p))
        .collect();
    progress.begin("writing plan pages", plan_writes.len());
    for (i, (stem, lifecycle)) in plan_writes.iter().enumerate() {
        let plan_events: Vec<LogEvent> = events
            .iter()
            .filter(|e| event_plan(e) == Some(stem.as_str()))
            .cloned()
            .collect();
        let page = render_plan_page(
            repo,
            stem.as_str(),
            **lifecycle,
            &plan_events,
            &reviews,
            &subjects,
            work_by_stem.get(stem.as_str()).copied(),
        );
        std::fs::write(out_dir.join(format!("plan/{stem}.html")), page)?;
        progress.tick(i + 1);
        if let Some(l) = loading {
            l.tick();
        }
    }
    progress.end();

    Ok(())
}

fn events_cache_path(out_dir: &Path) -> std::path::PathBuf {
    out_dir.join("events.json")
}

fn read_events_cache(out_dir: &Path) -> Option<Vec<LogEvent>> {
    let body = std::fs::read_to_string(events_cache_path(out_dir)).ok()?;
    serde_json::from_str(&body).ok()
}

fn write_events_cache(out_dir: &Path, events: &[LogEvent]) -> anyhow::Result<()> {
    let body = serde_json::to_string(events)?;
    std::fs::write(events_cache_path(out_dir), body)?;
    Ok(())
}

/// What the prior `clank html` build left on disk, from
/// `build_site`'s point of view.
enum PriorBuild {
    /// No prior index.html, or its meta tags are unreadable
    /// — cold build.
    None,
    /// Prior index exists but its builder version doesn't
    /// match ours. The rendered markup/CSS/JS/layout has
    /// since changed shape; existing per-commit and per-plan
    /// pages on disk are stale and must be rewritten.
    Stale,
    /// Prior index matches our builder version. `sha` is the
    /// last-built head — eligible for incremental fold if
    /// it's an ancestor of the current head.
    Fresh { sha: String },
}

/// Inspect `.clank/html/index.html` and classify the prior
/// build.
fn read_prior_build(out_dir: &Path) -> PriorBuild {
    let Ok(body) = std::fs::read_to_string(out_dir.join("index.html")) else {
        return PriorBuild::None;
    };
    let Some(version) = meta_value(&body, "clank:builder-version") else {
        return PriorBuild::None;
    };
    if version != BUILDER_VERSION {
        return PriorBuild::Stale;
    }
    match meta_value(&body, "clank:last-built-sha") {
        Some(sha) => PriorBuild::Fresh { sha },
        None => PriorBuild::None,
    }
}

fn meta_value(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let pos = html.find(&needle)?;
    let tag_start = html[..pos].rfind('<')?;
    let tag_end = html[tag_start..].find('>')?;
    let tag = &html[tag_start..tag_start + tag_end];
    let content_marker = "content=\"";
    let content_pos = tag.find(content_marker)?;
    let after = &tag[content_pos + content_marker.len()..];
    let close = after.find('"')?;
    Some(after[..close].to_string())
}

fn prev_is_ancestor(repo: &Path, prev: &str, head: &str) -> bool {
    // Unparseable shas → not-ancestor, matching the old merge-base
    // exit-1 behavior.
    let (Ok(prev), Ok(head)) = (CommitSha::parse(prev), CommitSha::parse(head)) else {
        return false;
    };
    crate::git_io::is_ancestor(repo, &prev, &head).unwrap_or(false)
}

/// Stderr progress bar. No-op when stderr isn't a TTY or
/// `--quiet` was passed.
struct Progress {
    enabled: bool,
}

impl Progress {
    fn new(quiet: bool) -> Self {
        use std::io::IsTerminal;
        Self {
            enabled: !quiet && std::io::stderr().is_terminal(),
        }
    }
    fn begin(&self, label: &str, total: usize) {
        if !self.enabled || total == 0 {
            return;
        }
        eprint!("clank html: {label} 0/{total}\r");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
    fn tick(&self, done: usize) {
        if !self.enabled {
            return;
        }
        eprint!("clank html: writing pages {done}\r");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
    fn end(&self) {
        if !self.enabled {
            return;
        }
        eprint!("\x1b[2K\r");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
    fn finish(&self) {
        if !self.enabled {
            return;
        }
        eprint!("\x1b[2K\r");
    }
}

/// The state the loading page renders. `clank html open` opens the browser
/// on this page before the (slow, first-time) build and rewrites it as the
/// build advances, so the browser has immediate feedback and auto-redirects
/// into the finished report.
enum LoadingState<'a> {
    /// Build hasn't reported a page total yet.
    Preparing,
    /// `done` of `total` pages written.
    Building { done: usize, total: usize },
    /// Build finished; `url` is the target page RELATIVE to the html root.
    Done { url: &'a str },
    /// Build failed / the target page was never produced.
    Error { message: &'a str },
}

/// Number of cells in the block progress bar.
const BAR_CELLS: usize = 28;

/// Render the self-contained loading page (inline CSS, system monospace —
/// file:// is offline). A "phosphor console" watching the build happen live:
/// warm near-black ground, one amber-phosphor accent, a block progress bar,
/// a ring spinner. Dark bg is set inline on `<html>` + `color-scheme:dark`
/// so the 1s meta-refresh never white-flashes; every animation is a clean 1s
/// loop so a reload restarts it seamlessly. Pure — unit-tested.
fn render_loading_page(label: &str, state: LoadingState) -> String {
    // State drives the `<head>` refresh directive + the accent colour.
    let (head_refresh, accent, is_error) = match &state {
        LoadingState::Preparing | LoadingState::Building { .. } => (
            "<meta http-equiv=\"refresh\" content=\"1\">".to_string(),
            "#ffb454",
            false,
        ),
        LoadingState::Done { url } => (
            format!(
                "<meta http-equiv=\"refresh\" content=\"0; url={}\">",
                esc(url)
            ),
            "#ffb454",
            false,
        ),
        // Error: NO refresh — stop the loop so it isn't an infinite spinner.
        LoadingState::Error { .. } => (String::new(), "#f26d6d", true),
    };

    // The bar + count + status line differ per state.
    let (filled, empty, count, status) = match &state {
        LoadingState::Preparing => (
            0usize,
            BAR_CELLS,
            "preparing…".to_string(),
            "reading the repository",
        ),
        LoadingState::Building { done, total } => {
            let total = (*total).max(1);
            let done = (*done).min(total);
            let filled = (done * BAR_CELLS).div_ceil(total).min(BAR_CELLS);
            (
                filled,
                BAR_CELLS - filled,
                format!("{done} / {total} pages"),
                "building the site",
            )
        }
        LoadingState::Done { .. } => (BAR_CELLS, 0, "done".to_string(), "opening your report"),
        LoadingState::Error { message } => (0, BAR_CELLS, "failed".to_string(), *message),
    };
    let bar = format!(
        "<span class=\"fill\">{}</span><span class=\"track\">{}</span>",
        "\u{2593}".repeat(filled),
        "\u{2591}".repeat(empty),
    );
    // The spinner glyph: a rotating ring while working; ✓ done; ✗ error.
    let glyph = match &state {
        LoadingState::Done { .. } => "<span class=\"done-mark\">\u{2713}</span>",
        LoadingState::Error { .. } => "<span class=\"err-mark\">\u{2717}</span>",
        _ => "<span class=\"ring\"></span>",
    };

    format!(
        r##"<!doctype html>
<html lang="en" style="background:#0b0b0d">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
{head_refresh}
<title>clank · building…</title>
<style>
:root {{
  --bg:#0b0b0d; --fg:#e8e4d8; --dim:#6b6660; --accent:{accent}; --track:#1c1a17;
}}
* {{ box-sizing:border-box; }}
html,body {{ height:100%; margin:0; background:var(--bg); }}
body {{
  color:var(--fg);
  font:15px/1.5 ui-monospace,"SF Mono","JetBrains Mono","Cascadia Code",Menlo,Consolas,monospace;
  display:grid; place-items:center;
  /* faint phosphor vignette + grain */
  background:
    radial-gradient(120% 90% at 50% 0%, rgba(255,180,84,.05), transparent 60%),
    var(--bg);
}}
body::after {{ /* subtle grain */
  content:""; position:fixed; inset:0; pointer-events:none; opacity:.035;
  background-image:url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='120' height='120'><filter id='n'><feTurbulence type='fractalNoise' baseFrequency='.9' numOctaves='2'/></filter><rect width='100%' height='100%' filter='url(%23n)'/></svg>");
}}
.card {{ width:min(90vw,560px); padding:8px 4px; }}
.brand {{ color:var(--dim); letter-spacing:.28em; text-transform:uppercase; font-size:11px; }}
.brand b {{ color:var(--accent); font-weight:600; }}
.brand .cur {{ display:inline-block; width:.5em; height:1em; vertical-align:-.15em;
  background:var(--accent); margin-left:.15em; animation:blink 1s steps(2) infinite;
  box-shadow:0 0 8px var(--accent); }}
.target {{ margin:26px 0 22px; font-size:14px; color:var(--dim); }}
.target b {{ color:var(--fg); font-weight:500; }}
.meter {{ display:flex; align-items:center; gap:16px; }}
.ring {{ width:20px; height:20px; border-radius:50%; flex:0 0 auto;
  background:conic-gradient(var(--accent) 0 90deg, transparent 90deg 360deg);
  -webkit-mask:radial-gradient(circle 6px, transparent 98%, #000 100%);
          mask:radial-gradient(circle 6px, transparent 98%, #000 100%);
  animation:spin 1s linear infinite; filter:drop-shadow(0 0 5px var(--accent)); }}
.done-mark,.err-mark {{ width:20px; text-align:center; font-size:18px; }}
.done-mark {{ color:var(--accent); text-shadow:0 0 8px var(--accent); }}
.err-mark {{ color:var(--accent); text-shadow:0 0 8px var(--accent); }}
.bar {{ font-size:15px; letter-spacing:1px; white-space:nowrap; overflow:hidden; }}
.bar .fill {{ color:var(--accent); text-shadow:0 0 6px var(--accent);
  animation:pulse 1s ease-in-out infinite; }}
.bar .track {{ color:var(--track); }}
.count {{ margin-top:14px; font-size:26px; color:var(--fg); font-weight:500;
  text-shadow:0 0 10px rgba(255,180,84,.15); }}
.count.err {{ color:var(--accent); }}
.status {{ margin-top:6px; color:var(--dim); font-size:13px; }}
.foot {{ margin-top:34px; color:var(--dim); font-size:11px; opacity:.7; }}
@keyframes spin {{ to {{ transform:rotate(360deg); }} }}
@keyframes pulse {{ 0%,100%{{opacity:.72}} 50%{{opacity:1}} }}
@keyframes blink {{ 50%{{opacity:0}} }}
</style>
</head>
<body>
<main class="card">
  <div class="brand"><b>clank</b> · html report<span class="cur"></span></div>
  <div class="target">rendering&nbsp;&nbsp;<b>{label}</b></div>
  <div class="meter">{glyph}<div class="bar">{bar}</div></div>
  <div class="count{count_err}">{count}</div>
  <div class="status">{status}</div>
  <div class="foot">this page turns into your report automatically — no need to reload.</div>
</main>
<script>
// Watchdog: if the build dies without writing an error state, stop spinning
// after 60s and tell the reader where to look (the process is detached).
setTimeout(function(){{
  if(!document.querySelector('meta[http-equiv="refresh"]')) return;
  document.title='clank · still building';
}}, 60000);
</script>
</body>
</html>
"##,
        head_refresh = head_refresh,
        accent = accent,
        label = esc(label),
        glyph = glyph,
        bar = bar,
        count = esc(&count),
        count_err = if is_error { " err" } else { "" },
        status = esc(status),
    )
}

/// The live loading page for one `clank html open`: opened in the browser
/// before the build, rewritten (throttled to ~1s, matching the page's own
/// meta-refresh) as the build advances, then finalized to a redirect (or an
/// error). Interior mutability so `build_site` can drive it through a shared
/// reference. Writes are atomic (temp + rename) so a refresh never catches a
/// half-written file.
pub(crate) struct LoadingPage {
    path: std::path::PathBuf,
    /// The target page, RELATIVE to the html root (e.g. `commit/<sha>.html`).
    target_rel: String,
    label: String,
    total: std::cell::Cell<usize>,
    done: std::cell::Cell<usize>,
    last_write: std::cell::Cell<Option<std::time::Instant>>,
}

impl LoadingPage {
    /// Create + write the initial "preparing" page.
    fn new(path: std::path::PathBuf, target_rel: String, label: String) -> Self {
        let lp = LoadingPage {
            path,
            target_rel,
            label,
            total: std::cell::Cell::new(0),
            done: std::cell::Cell::new(0),
            last_write: std::cell::Cell::new(None),
        };
        lp.write(render_loading_page(&lp.label, LoadingState::Preparing));
        lp
    }

    /// Record the page total once the build knows it (writes immediately).
    fn set_total(&self, total: usize) {
        self.total.set(total);
        self.render_progress(std::time::Instant::now());
    }

    /// One page written. Rewrites the loading page at most once per second
    /// (its own refresh cadence) — see [`Self::tick_at`].
    fn tick(&self) {
        self.tick_at(std::time::Instant::now());
    }

    fn tick_at(&self, now: std::time::Instant) {
        self.done.set(self.done.get() + 1);
        let due = self
            .last_write
            .get()
            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1));
        if due {
            self.render_progress(now);
        }
    }

    fn render_progress(&self, now: std::time::Instant) {
        self.last_write.set(Some(now));
        let state = if self.total.get() == 0 {
            LoadingState::Preparing
        } else {
            LoadingState::Building {
                done: self.done.get(),
                total: self.total.get(),
            }
        };
        self.write(render_loading_page(&self.label, state));
    }

    /// Build succeeded — redirect the browser to the finished page.
    fn finish_ok(&self) {
        self.write(render_loading_page(
            &self.label,
            LoadingState::Done {
                url: &self.target_rel,
            },
        ));
    }

    /// Build failed / target never produced — show the error, stop the loop.
    fn finish_err(&self, message: &str) {
        self.write(render_loading_page(
            &self.label,
            LoadingState::Error { message },
        ));
    }

    /// Atomic write (temp + rename) so a concurrent meta-refresh can't read a
    /// half-written page.
    fn write(&self, html: String) {
        let tmp = self.path.with_extension("html.tmp");
        if std::fs::write(&tmp, html).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

// ─────────────────────────── data ───────────────────────────

struct Review {
    author: String,
    verdict: Verdict,
    summary: String,
    body: String,
}

fn event_sha(e: &LogEvent) -> &CommitSha {
    match e {
        LogEvent::PlanIntro { sha, .. }
        | LogEvent::PlanCommit { sha, .. }
        | LogEvent::PlanFinalized { sha, .. }
        | LogEvent::PlanDeleted { sha, .. }
        | LogEvent::AdHoc { sha, .. } => sha,
    }
}

fn event_ts(e: &LogEvent) -> i64 {
    match e {
        LogEvent::PlanIntro { ts, .. }
        | LogEvent::PlanCommit { ts, .. }
        | LogEvent::PlanFinalized { ts, .. }
        | LogEvent::PlanDeleted { ts, .. }
        | LogEvent::AdHoc { ts, .. } => *ts,
    }
}

fn event_plan(e: &LogEvent) -> Option<&str> {
    match e {
        LogEvent::PlanIntro { plan, .. }
        | LogEvent::PlanCommit { plan, .. }
        | LogEvent::PlanFinalized { plan, .. }
        | LogEvent::PlanDeleted { plan, .. } => Some(plan.as_str()),
        LogEvent::AdHoc { .. } => None,
    }
}

fn event_kind_label(e: &LogEvent) -> &'static str {
    match e {
        LogEvent::PlanIntro { .. } => "intro",
        LogEvent::PlanCommit {
            touched_plan,
            touched_code,
            ..
        } => match (touched_plan, touched_code) {
            (true, false) => "plan",
            (false, true) => "code",
            (true, true) => "mixed",
            (false, false) => "commit",
        },
        LogEvent::PlanFinalized { .. } => "finish",
        LogEvent::PlanDeleted { .. } => "delete",
        LogEvent::AdHoc { .. } => "ad-hoc",
    }
}

fn collect_reviews(repo: &Path, shas: &[CommitSha]) -> BTreeMap<String, Vec<Review>> {
    let mut out: BTreeMap<String, Vec<Review>> = BTreeMap::new();
    if shas.is_empty() {
        return out;
    }
    let Ok(fv) = crate::feedback_scan::scan_feedback(repo, shas) else {
        return out;
    };
    for cf in &fv.per_commit {
        for (author, entry) in &cf.entries {
            let (summary, body) = std::fs::read_to_string(repo.join(&entry.source_path))
                .ok()
                .map(|raw| {
                    let fb = FeedbackBody::parse(&raw);
                    (fb.summary(), fb.details())
                })
                .unwrap_or_default();
            out.entry(cf.sha.as_str().to_string())
                .or_default()
                .push(Review {
                    author: author.as_str().to_string(),
                    verdict: entry.verdict,
                    summary,
                    body,
                });
        }
    }
    out
}

// ─────────────────────────── index ──────────────────────────

fn render_index(
    status: &StatusSnapshot,
    events: &[LogEvent],
    reviews: &BTreeMap<String, Vec<Review>>,
    subjects: &BTreeMap<String, String>,
    head_sha: Option<&str>,
) -> String {
    let mut out = String::new();
    let meta = format!(
        "<meta name=\"clank:builder-version\" content=\"{BUILDER_VERSION}\">\n\
         <meta name=\"clank:last-built-sha\" content=\"{}\">\n",
        head_sha.unwrap_or("")
    );
    write_doc_open_with_meta(
        &mut out,
        &format!("clank · {}", status.basename),
        ".",
        &meta,
    );
    out.push_str("<header class=\"status\">\n");
    out.push_str(&format!("  <h1>{}</h1>\n", esc(&status.basename)));
    let branch = status.branch.as_deref().unwrap_or("(detached)");
    let head_short = status.head_sha.as_deref().map(short).unwrap_or("(no head)");
    let dirty = if status.dirty.is_some() {
        " · dirty"
    } else {
        ""
    };
    out.push_str(&format!(
        "  <div class=\"status-meta\"><span class=\"branch\">{}</span> · <code>{}</code>{}</div>\n",
        esc(branch),
        esc(head_short),
        dirty
    ));
    if !status.plans.is_empty() {
        out.push_str("  <ul class=\"plan-list\">\n");
        for p in &status.plans {
            out.push_str(&format!(
                "    <li><span class=\"plan-pill\">{}</span> <span class=\"gate gate-{}\">{}</span> <span class=\"waiting\">{:?}</span></li>\n",
                esc(p.plan.as_str()),
                p.gate.as_str(),
                esc(p.gate.as_str()),
                p.waiting_on
            ));
        }
        out.push_str("  </ul>\n");
    } else if let Some(fp) = &status.last_finished {
        out.push_str(&format!(
            "  <div class=\"last-finished\">last finished: <span class=\"plan-pill\">{}</span> at <code>{}</code></div>\n",
            esc(fp.plan.as_str()),
            esc(short(fp.finalized_at.as_str()))
        ));
    } else {
        out.push_str("  <div class=\"empty\">No active plans — repo is idle.</div>\n");
    }
    if !status.queue.is_empty() {
        out.push_str(&format!(
            "  <div class=\"queue\">queue: {} item{}</div>\n",
            status.queue.len(),
            if status.queue.len() == 1 { "" } else { "s" }
        ));
    }
    if !status.blocks.is_empty() {
        out.push_str("  <div class=\"blocks\">blocks:\n    <ul>\n");
        for b in &status.blocks {
            out.push_str(&format!(
                "      <li><code>{}</code>/{}{} — {}</li>\n",
                esc(&b.agent),
                esc(&b.name),
                b.plan
                    .as_deref()
                    .map(|p| format!(" · plan {p}"))
                    .unwrap_or_default(),
                esc(&b.question)
            ));
        }
        out.push_str("    </ul>\n  </div>\n");
    }
    out.push_str("</header>\n");

    out.push_str("<main>\n");
    out.push_str("<h2>Timeline</h2>\n");
    out.push_str(&render_timeline(
        events,
        reviews,
        subjects,
        PlanLinkMode::LinkRelativeToIndex,
        "",
    ));
    out.push_str("</main>\n");
    write_doc_close(&mut out);
    out
}

/// How umbrella plan-pills link out.
#[derive(Copy, Clone)]
enum PlanLinkMode {
    /// `<a class="plan-pill" href="plan/<stem>.html">…</a>`
    /// — used on the index page.
    LinkRelativeToIndex,
    /// Plain `<span class="plan-pill">` — used on a plan page
    /// itself (self-link is noise).
    NoLink,
}

/// Render the `<div class="timeline">` block: newest-first
/// umbrella sections grouping contiguous same-plan events.
/// Returns the full block (or an empty-state paragraph when
/// there are no events).
///
/// `commit_href_prefix` makes per-row commit links resolve
/// correctly from non-root pages: `""` from the index,
/// `"../"` from `.clank/html/plan/<stem>.html`.
fn render_timeline(
    events: &[LogEvent],
    reviews: &BTreeMap<String, Vec<Review>>,
    subjects: &BTreeMap<String, String>,
    plan_link_mode: PlanLinkMode,
    commit_href_prefix: &str,
) -> String {
    if events.is_empty() {
        return "<p class=\"empty\">No events yet.</p>\n".to_string();
    }
    let mut out = String::from("<div class=\"timeline\">\n");
    // Group newest-first via the SHARED contiguous-run rule.
    let newest_first: Vec<&LogEvent> = events.iter().rev().collect();
    for (key, run) in clank_core::repo_state::umbrella_sections(&newest_first, true) {
        out.push_str(&render_umbrella(
            &key,
            &run,
            reviews,
            subjects,
            plan_link_mode,
            commit_href_prefix,
        ));
    }
    out.push_str("</div>\n");
    out
}

/// One umbrella section: header + the rows it groups, in
/// newest-first order (the caller already ordered the run).
fn render_umbrella(
    key: &UmbrellaKey,
    rows: &[&LogEvent],
    reviews: &BTreeMap<String, Vec<Review>>,
    subjects: &BTreeMap<String, String>,
    plan_link_mode: PlanLinkMode,
    commit_href_prefix: &str,
) -> String {
    let mut out = format!(
        "<section class=\"umbrella umbrella-{kind}\" data-umbrella-key=\"{key_attr}\">\n",
        kind = umbrella_kind_class(key),
        key_attr = esc(&umbrella_attr_value(key))
    );
    out.push_str("  <header class=\"umbrella-header\">");
    match key {
        UmbrellaKey::Plan(p) => match plan_link_mode {
            PlanLinkMode::LinkRelativeToIndex => out.push_str(&format!(
                "<a class=\"plan-pill\" href=\"plan/{}.html\">{}</a>",
                esc(p.as_str()),
                esc(p.as_str())
            )),
            PlanLinkMode::NoLink => out.push_str(&format!(
                "<span class=\"plan-pill\">{}</span>",
                esc(p.as_str())
            )),
        },
        UmbrellaKey::AdHoc => out.push_str("<span class=\"adhoc-label\">ad-hoc</span>"),
    }
    out.push_str("</header>\n");
    for event in rows {
        out.push_str(&render_row(event, reviews, subjects, commit_href_prefix));
    }
    out.push_str("</section>\n");
    out
}

/// One timeline row. Tagged with `data-sha=<full-sha>` so the
/// incremental splicer can target it by selector.
fn render_row(
    event: &LogEvent,
    reviews: &BTreeMap<String, Vec<Review>>,
    subjects: &BTreeMap<String, String>,
    commit_href_prefix: &str,
) -> String {
    let sha = event_sha(event);
    let kind = event_kind_label(event);
    let marks = reviews
        .get(sha.as_str())
        .map(|v| verdict_marks_html(v.as_slice()))
        .unwrap_or_default();
    let raw_subject = subjects.get(sha.as_str()).map(String::as_str).unwrap_or("");
    let body = clank_core::repo_state::parse_subject(raw_subject).body;
    let mut out = format!("<div class=\"row\" data-sha=\"{}\">\n", esc(sha.as_str()));
    out.push_str(&format!(
        "  <button class=\"sha-copy\" type=\"button\" data-sha=\"{full}\" title=\"{full}\">{short}</button>\n",
        full = esc(sha.as_str()),
        short = esc(short(sha.as_str()))
    ));
    out.push_str(&format!(
        "  <a class=\"row-link\" href=\"{}commit/{}.html\">\n",
        commit_href_prefix,
        esc(sha.as_str())
    ));
    out.push_str(&format!(
        "    <span class=\"kind kind-{}\">{}</span>\n",
        kind, kind
    ));
    out.push_str(&format!(
        "    <span class=\"subject\">{}</span>\n",
        esc(body.trim())
    ));
    out.push_str(&format!("    <span class=\"marks\">{marks}</span>\n"));
    let iso = fmt_ts(event_ts(event));
    out.push_str(&format!(
        "    <time class=\"ts\" data-iso=\"{iso}\">{iso}</time>\n",
        iso = esc(&iso)
    ));
    out.push_str("  </a>\n");
    out.push_str("</div>\n");
    out
}

// Display adapters over the SHARED umbrella key (core owns the
// grouping rule — log-plan-umbrellas factored it so html and the
// oneline renderers can't drift on "what counts as one umbrella").
use clank_core::repo_state::UmbrellaKey;

fn umbrella_kind_class(key: &UmbrellaKey) -> &'static str {
    match key {
        UmbrellaKey::Plan(_) => "plan",
        UmbrellaKey::AdHoc => "adhoc",
    }
}

fn umbrella_attr_value(key: &UmbrellaKey) -> String {
    match key {
        UmbrellaKey::Plan(p) => p.as_str().to_string(),
        UmbrellaKey::AdHoc => "ad-hoc".to_string(),
    }
}

// ──────────────────────── commit page ───────────────────────

fn render_commit_page(
    repo: &Path,
    event: &LogEvent,
    reviews: &BTreeMap<String, Vec<Review>>,
) -> String {
    let sha = event_sha(event);
    let plan = event_plan(event);
    let kind = event_kind_label(event);
    let subject = commit_subject(repo, sha);

    let mut out = String::new();
    write_doc_open(
        &mut out,
        &format!("commit {} · clank", short(sha.as_str())),
        "..",
    );
    out.push_str("<header class=\"commit-header\">\n");
    out.push_str("  <p class=\"crumb\"><a href=\"../index.html\">← timeline</a></p>\n");
    out.push_str(&format!(
        "  <h1><button class=\"sha-copy\" type=\"button\" data-sha=\"{full}\" title=\"{full}\">{short}</button> <span class=\"kind kind-{}\">{}</span></h1>\n",
        kind,
        kind,
        full = esc(sha.as_str()),
        short = esc(short(sha.as_str()))
    ));
    if let Some(p) = plan {
        out.push_str(&format!(
            "  <div class=\"plan-line\">plan: <a class=\"plan-pill\" href=\"../plan/{}.html\">{}</a></div>\n",
            esc(p),
            esc(p)
        ));
    }
    let subject_body = clank_core::repo_state::parse_subject(&subject).body;
    out.push_str(&format!(
        "  <h2 class=\"subject\">{}</h2>\n",
        esc(subject_body)
    ));
    let body = commit_body(repo, sha);
    if !body.is_empty() {
        out.push_str(&format!(
            "  <pre class=\"commit-body\">{}</pre>\n",
            esc(&body)
        ));
    }
    out.push_str(&format!(
        "  <p class=\"sha-full\"><button class=\"sha-copy\" type=\"button\" data-sha=\"{full}\" title=\"{full}\">{full}</button></p>\n",
        full = esc(sha.as_str())
    ));
    out.push_str("</header>\n");

    let plan_md = plan_body_at_commit(repo, event);
    let plan_is_centerpiece = matches!(
        event,
        LogEvent::PlanIntro { .. }
            | LogEvent::PlanFinalized { .. }
            | LogEvent::PlanCommit {
                touched_plan: true,
                touched_code: false,
                ..
            }
    );

    out.push_str("<main>\n");
    if plan_is_centerpiece {
        if let Some(md) = &plan_md {
            out.push_str("<section class=\"plan-body\">\n");
            out.push_str("  <h3>Plan at this commit</h3>\n");
            out.push_str("  <article class=\"md\">\n");
            out.push_str(&render_markdown(md));
            out.push_str("  </article>\n");
            out.push_str("</section>\n");
        }
    }

    // Feedback.
    let empty: Vec<Review> = Vec::new();
    let reviews_here = reviews.get(sha.as_str()).unwrap_or(&empty);
    out.push_str("<section class=\"feedback\">\n");
    out.push_str("  <h3>Reviews</h3>\n");
    if reviews_here.is_empty() {
        out.push_str("  <p class=\"empty\">No reviewer has weighed in.</p>\n");
    } else {
        for r in reviews_here {
            out.push_str(&format!(
                "  <article class=\"review verdict-{}\">\n",
                verdict_slug(r.verdict)
            ));
            out.push_str(&format!(
                "    <header><strong>{}</strong> <span class=\"verdict\">{}</span></header>\n",
                esc(&r.author),
                verdict_label(r.verdict)
            ));
            if !r.summary.is_empty() {
                out.push_str(&format!(
                    "    <p class=\"summary\">{}</p>\n",
                    esc(&r.summary)
                ));
            }
            if !r.body.is_empty() {
                out.push_str("    <div class=\"md\">\n");
                out.push_str(&render_markdown(&r.body));
                out.push_str("    </div>\n");
            }
            out.push_str("  </article>\n");
        }
    }
    out.push_str("</section>\n");

    // Diff.
    let diff = commit_diff(repo, sha);
    out.push_str("<section class=\"diff\">\n");
    out.push_str(&format!(
        "  <details{}><summary>Diff ({} file{})</summary>\n",
        if plan_is_centerpiece { "" } else { " open" },
        diff.len(),
        if diff.len() == 1 { "" } else { "s" }
    ));
    if diff.is_empty() {
        out.push_str("  <p class=\"empty\">No file changes.</p>\n");
    } else {
        for fp in &diff {
            out.push_str(&render_file_patch(fp));
        }
    }
    out.push_str("  </details>\n");
    out.push_str("</section>\n");
    out.push_str("</main>\n");
    write_doc_close(&mut out);
    out
}

// ───────────────────────── plan page ────────────────────────

/// Lifecycle slot of a plan as inferred from its events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanLifecycle {
    Active,
    Finished,
}

/// Walk `events` and group plan-tagged ones by stem along
/// with the lifecycle implied by the latest plan-touching
/// event. Plans whose latest event is `PlanDeleted` are
/// excluded.
fn collect_plan_buckets(events: &[LogEvent]) -> BTreeMap<String, PlanLifecycle> {
    let mut latest: BTreeMap<String, &LogEvent> = BTreeMap::new();
    for e in events {
        if let Some(stem) = event_plan(e) {
            // Last write wins → latest event per stem.
            latest.insert(stem.to_string(), e);
        }
    }
    let mut out = BTreeMap::new();
    for (stem, e) in latest {
        match e {
            LogEvent::PlanDeleted { .. } => continue,
            LogEvent::PlanFinalized { .. } => {
                out.insert(stem, PlanLifecycle::Finished);
            }
            _ => {
                out.insert(stem, PlanLifecycle::Active);
            }
        }
    }
    out
}

/// `git show HEAD:<.clank/{plans,finished}/<stem>.md>` —
/// empty `None` if either git or the path resolves
/// unsuccessfully.
fn plan_body_at_head(repo: &Path, stem: &str, lifecycle: PlanLifecycle) -> Option<String> {
    let head = head_sha(repo).ok().flatten()?;
    let path = match lifecycle {
        PlanLifecycle::Active => crate::init_facts::plan_md_rel(stem),
        PlanLifecycle::Finished => crate::init_facts::finished_md_rel(stem),
    };
    crate::git_io::show_blob(repo, &head, std::path::Path::new(&path)).ok()
}

fn render_plan_page(
    repo: &Path,
    stem: &str,
    lifecycle: PlanLifecycle,
    plan_events: &[LogEvent],
    reviews: &BTreeMap<String, Vec<Review>>,
    subjects: &BTreeMap<String, String>,
    work: Option<&PlanWorkState>,
) -> String {
    let mut out = String::new();
    write_doc_open(&mut out, &format!("plan {stem} · clank"), "..");
    out.push_str("<header class=\"plan-header\">\n");
    out.push_str("  <p class=\"crumb\"><a href=\"../index.html\">← timeline</a></p>\n");
    out.push_str(&format!(
        "  <h1><span class=\"plan-pill\">{}</span></h1>\n",
        esc(stem)
    ));
    let state_line = match lifecycle {
        PlanLifecycle::Active => {
            let mut bits = vec!["active".to_string()];
            if let Some(w) = work {
                bits.push(format!("gate: {}", verdict_gate_label(w.gate)));
                let waiting = waiting_on_label(&w.waiting_on);
                if !waiting.is_empty() {
                    bits.push(format!("waiting on {waiting}"));
                }
            }
            bits.join(" · ")
        }
        PlanLifecycle::Finished => "finished".to_string(),
    };
    out.push_str(&format!(
        "  <div class=\"plan-state\">{}</div>\n",
        esc(&state_line)
    ));
    out.push_str("</header>\n");

    out.push_str("<main>\n");
    out.push_str("<h2>Timeline</h2>\n");
    out.push_str(&render_timeline(
        plan_events,
        reviews,
        subjects,
        PlanLinkMode::NoLink,
        "../",
    ));

    if let Some(md) = plan_body_at_head(repo, stem, lifecycle) {
        out.push_str("<section class=\"plan-body\">\n");
        out.push_str("  <h3>Plan (latest revision)</h3>\n");
        out.push_str("  <article class=\"md\">\n");
        out.push_str(&render_markdown(&md));
        out.push_str("  </article>\n");
        out.push_str("</section>\n");
    }

    out.push_str("</main>\n");
    write_doc_close(&mut out);
    out
}

fn verdict_gate_label(g: clank_core::vocab::CommitGateState) -> &'static str {
    use clank_core::vocab::CommitGateState as G;
    match g {
        G::Continued => "continued",
        G::Finished => "finished",
        G::ChangesRequested => "changes-requested",
        G::Unreviewed => "unreviewed",
        G::Blocked => "blocked",
        G::ContinuedPendingGate => "continued-pending-gate",
    }
}

fn waiting_on_label(w: &clank_core::plan_view::WaitingOn) -> String {
    use clank_core::plan_view::WaitingOn as W;
    match w {
        W::Blocked { block } => format!("blocked ({})", block.creator.as_str()),
        W::ReviewerApprovalsMissing { missing } => {
            let mut names: Vec<String> = missing
                .as_slice()
                .iter()
                .map(|a| a.as_str().to_string())
                .collect();
            names.sort();
            format!("reviewers ({})", names.join(", "))
        }
        W::GateReviewersMissing { missing } => {
            let mut names: Vec<String> = missing
                .as_slice()
                .iter()
                .map(|a| a.as_str().to_string())
                .collect();
            names.sort();
            format!("gate reviewers ({})", names.join(", "))
        }
        W::MasterToRevise { .. } => "master to revise".to_string(),
        W::MasterToContinue => "master to continue".to_string(),
        W::MasterToFinalize => "master to finalize".to_string(),
        W::MasterToCommit => "master to commit".to_string(),
        W::MasterToFixCommitTag => "master to fix commit tag".to_string(),
    }
}

// ─────────────────────────── diff ───────────────────────────

struct FilePatch {
    header: String,
    hunks: Vec<Hunk>,
}

struct Hunk {
    /// Verbatim hunk header line including `@@ ... @@`.
    header_line: String,
    /// Starting old/new line numbers from `@@ -A,B +C,D @@`.
    old_start: u32,
    new_start: u32,
    /// Body lines (without trailing `\n`).
    body: Vec<String>,
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    // `@@ -A,B +C,D @@ optional context`
    let stripped = line.strip_prefix("@@ ")?;
    let close = stripped.find("@@")?;
    let nums = &stripped[..close];
    let mut parts = nums.split_whitespace();
    let old_chunk = parts.next()?.strip_prefix('-')?;
    let new_chunk = parts.next()?.strip_prefix('+')?;
    let old_start: u32 = old_chunk.split(',').next()?.parse().ok()?;
    let new_start: u32 = new_chunk.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

fn commit_diff(repo: &Path, sha: &CommitSha) -> Vec<FilePatch> {
    let Ok(out) = crate::git_io::commit_diff_text(repo, sha.as_str()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out).to_string();
    parse_unified_diff(&text)
}

fn parse_unified_diff(text: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut cur_header: Option<String> = None;
    let mut cur_hunks: Vec<Hunk> = Vec::new();
    let mut cur_hunk: Option<Hunk> = None;
    for line in text.lines() {
        if line.starts_with("diff --git") {
            if let Some(h) = cur_hunk.take() {
                cur_hunks.push(h);
            }
            if let Some(header) = cur_header.take() {
                files.push(FilePatch {
                    header,
                    hunks: std::mem::take(&mut cur_hunks),
                });
            }
            cur_header = Some(line.to_string());
        } else if line.starts_with("@@") {
            if let Some(h) = cur_hunk.take() {
                cur_hunks.push(h);
            }
            let (old_start, new_start) = parse_hunk_header(line).unwrap_or((0, 0));
            cur_hunk = Some(Hunk {
                header_line: line.to_string(),
                old_start,
                new_start,
                body: Vec::new(),
            });
        } else if let Some(h) = cur_hunk.as_mut() {
            h.body.push(line.to_string());
        } else if let Some(h) = cur_header.as_mut() {
            // Pre-hunk metadata lines (index, ---, +++).
            h.push('\n');
            h.push_str(line);
        }
    }
    if let Some(h) = cur_hunk.take() {
        cur_hunks.push(h);
    }
    if let Some(header) = cur_header.take() {
        files.push(FilePatch {
            header,
            hunks: cur_hunks,
        });
    }
    files
}

fn render_file_patch(fp: &FilePatch) -> String {
    // Extract a + b path from the `diff --git a/<a> b/<b>` line for
    // the file heading.
    let first = fp.header.lines().next().unwrap_or("").to_string();
    let path = first
        .strip_prefix("diff --git ")
        .and_then(|rest| rest.split_once(' '))
        .map(|(_a, b)| b.trim_start_matches("b/").to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_string());
    let mut s = String::new();
    s.push_str(&format!(
        "  <details open class=\"file\"><summary>{}</summary>\n",
        esc(&path)
    ));
    for hunk in &fp.hunks {
        s.push_str("    <div class=\"hunk\">\n");
        // Hunk header line: full-width band, no line numbers.
        s.push_str(&format!(
            "      <div class=\"line hunk-hdr\"><span class=\"src\">{}</span></div>\n",
            esc(&hunk.header_line)
        ));
        let mut old_no = hunk.old_start;
        let mut new_no = hunk.new_start;
        for line in &hunk.body {
            let (cls, sign, source) = classify_line(line);
            let (old_label, new_label) = match cls {
                "add" => {
                    let l = (String::new(), new_no.to_string());
                    new_no += 1;
                    l
                }
                "del" => {
                    let l = (old_no.to_string(), String::new());
                    old_no += 1;
                    l
                }
                "ctx" => {
                    let l = (old_no.to_string(), new_no.to_string());
                    old_no += 1;
                    new_no += 1;
                    l
                }
                _ => (String::new(), String::new()),
            };
            let highlighted = if source.is_empty() {
                String::new()
            } else {
                crate::cli::html_highlight::highlight_line(ext.as_deref(), source)
            };
            s.push_str(&format!(
                "      <div class=\"line {cls}\"><span class=\"ln old\">{}</span><span class=\"ln new\">{}</span><span class=\"sign\">{}</span><span class=\"src\">{}</span></div>\n",
                esc(&old_label),
                esc(&new_label),
                esc(sign),
                highlighted
            ));
        }
        s.push_str("    </div>\n");
    }
    s.push_str("  </details>\n");
    s
}

fn classify_line(line: &str) -> (&'static str, &'static str, &str) {
    // `parse_unified_diff` puts file-header lines (`+++ b/...`
    // and `--- a/...`) on FilePatch::header, not in any hunk
    // body. Inside a hunk body, the leading `+`/`-` is always
    // the diff sign — content that itself starts with `+`/`-`
    // (e.g. an added `++x` line lands as `+++x`) still has
    // the diff sign as its first byte. Classify on that byte
    // alone; never re-special-case multi-byte prefixes.
    if let Some(rest) = line.strip_prefix('+') {
        ("add", "+", rest)
    } else if let Some(rest) = line.strip_prefix('-') {
        ("del", "-", rest)
    } else if let Some(rest) = line.strip_prefix(' ') {
        ("ctx", " ", rest)
    } else if line.starts_with('\\') {
        // "\ No newline at end of file" — context annotation;
        // show verbatim with empty line numbers.
        ("meta", " ", line)
    } else {
        ("ctx", " ", line)
    }
}

// ─────────────────────────── helpers ────────────────────────

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // Strip raw HTML events — feedback bodies and plan markdown
    // can contain user-controlled HTML that we don't want to
    // pass through verbatim. pulldown-cmark's `Html` and
    // `InlineHtml` events represent literal HTML tags from the
    // source.
    //
    // Within a fenced code block: buffer the text events,
    // pass the buffered source through syntect, and emit the
    // resulting <pre><code class="hl">...</code></pre> directly.
    // Outside fenced blocks: hand events to pulldown-cmark's
    // `push_html` for the normal HTML rendering.
    let mut out = String::new();
    let mut buffer: Vec<Event<'_>> = Vec::new();
    let mut in_fenced: Option<String> = None; // Some(lang) while inside a fenced block
    let mut code_buf = String::new();
    for ev in Parser::new_ext(md, opts) {
        if matches!(&ev, Event::Html(_) | Event::InlineHtml(_)) {
            continue;
        }
        match (&in_fenced, &ev) {
            (None, Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))) => {
                // Flush pending non-code events to push_html.
                if !buffer.is_empty() {
                    html::push_html(&mut out, buffer.drain(..));
                }
                in_fenced = Some(info.to_string());
                code_buf.clear();
            }
            (Some(lang), Event::Text(text)) => {
                code_buf.push_str(text);
                let _ = lang; // lang consumed at End below
            }
            (Some(lang), Event::End(TagEnd::CodeBlock)) => {
                let lang_hint = if lang.trim().is_empty() {
                    None
                } else {
                    Some(lang.trim())
                };
                let highlighted = crate::cli::html_highlight::highlight_block(lang_hint, &code_buf);
                out.push_str("<pre><code class=\"hl\">");
                out.push_str(&highlighted);
                out.push_str("</code></pre>");
                in_fenced = None;
                code_buf.clear();
            }
            (Some(_), _) => {
                // Other events inside a fenced block (rare —
                // pulldown emits only Text inside fences) are
                // dropped from the highlighted output. The
                // text accumulator above is the source of truth.
            }
            (None, _) => {
                buffer.push(ev);
            }
        }
    }
    if !buffer.is_empty() {
        html::push_html(&mut out, buffer.drain(..));
    }
    out
}

fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Continue => "CONTINUE",
        Verdict::Finished => "FINISHED",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Unmarked => "(no verdict)",
    }
}

fn verdict_slug(v: Verdict) -> &'static str {
    match v {
        Verdict::Continue => "continue",
        Verdict::Finished => "finished",
        Verdict::RequestChanges => "request-changes",
        Verdict::Unmarked => "unmarked",
    }
}

fn verdict_marks_html(reviews: &[Review]) -> String {
    let mut s = String::new();
    for r in reviews {
        let (mark, title) = match r.verdict {
            Verdict::Continue => ("✓", format!("CONTINUE by {}", r.author)),
            Verdict::Finished => ("✓✓", format!("FINISHED by {}", r.author)),
            Verdict::RequestChanges => ("✗", format!("REQUEST_CHANGES by {}", r.author)),
            Verdict::Unmarked => ("●", format!("unmarked by {}", r.author)),
        };
        s.push_str(&format!(
            "<span class=\"mark mark-{}\" title=\"{}\">{}</span>",
            verdict_slug(r.verdict),
            esc(&title),
            mark
        ));
    }
    s
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn head_sha(repo: &Path) -> anyhow::Result<Option<CommitSha>> {
    Ok(crate::git_io::rev_parse_head(repo)?)
}

fn commit_subject(repo: &Path, sha: &CommitSha) -> String {
    crate::git_io::commit_subject_at(repo, sha)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The commit message body (everything after the subject line
/// and its trailing blank). Empty when the commit has only a
/// subject.
fn commit_body(repo: &Path, sha: &CommitSha) -> String {
    crate::git_io::commit_body_at(repo, sha)
        .map(|b| b.trim().to_string())
        .unwrap_or_default()
}

/// Batch-fetch commit subjects via a single `git log` so the
/// index doesn't shell out per row.
fn collect_subjects(repo: &Path, head: Option<&CommitSha>) -> BTreeMap<String, String> {
    match head {
        Some(head) => crate::git_io::ancestor_subjects_at(repo, head).unwrap_or_default(),
        None => BTreeMap::new(),
    }
}

fn plan_body_at_commit(repo: &Path, event: &LogEvent) -> Option<String> {
    let sha = event_sha(event);
    let path = match event {
        LogEvent::PlanIntro { plan, .. }
        | LogEvent::PlanCommit { plan, .. }
        | LogEvent::PlanDeleted { plan, .. } => crate::init_facts::plan_md_rel(plan.as_str()),
        LogEvent::PlanFinalized { plan, .. } => crate::init_facts::finished_md_rel(plan.as_str()),
        LogEvent::AdHoc { .. } => return None,
    };
    crate::git_io::show_blob(repo, sha, std::path::Path::new(&path)).ok()
}

fn fmt_ts(ts: i64) -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

fn launch_opener(path: &Path) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(target_os = "linux")]
    let prog = "xdg-open";
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        anyhow::bail!(
            "no known opener for this platform; open `{}` manually",
            path_str
        );
    }
    #[allow(unreachable_code)]
    {
        let status = Command::new(prog).arg(&path_str).status()?;
        if !status.success() {
            anyhow::bail!("`{prog} {path_str}` exited non-zero");
        }
        Ok(())
    }
}

// ─────────────────────── document chrome ────────────────────

fn write_doc_open_with_meta(out: &mut String, title: &str, css_rel: &str, extra_head: &str) {
    out.push_str("<!doctype html>\n<html lang=\"en\"><head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    out.push_str(extra_head);
    out.push_str(&format!("<title>{}</title>\n", esc(title)));
    out.push_str(&format!(
        "<link rel=\"stylesheet\" href=\"{}/style.css\">\n",
        esc(css_rel)
    ));
    out.push_str("</head>\n<body>\n");
}

fn write_doc_open(out: &mut String, title: &str, css_rel: &str) {
    out.push_str("<!doctype html>\n<html lang=\"en\"><head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n");
    out.push_str(&format!("<title>{}</title>\n", esc(title)));
    out.push_str(&format!(
        "<link rel=\"stylesheet\" href=\"{}/style.css\">\n",
        esc(css_rel)
    ));
    out.push_str("</head>\n<body>\n");
}

fn write_doc_close(out: &mut String) {
    // Optional JS: walks `[data-iso]` and rewrites text to a
    // relative phrasing ("4h ago"). If JS is disabled the raw
    // ISO stays visible — load-bearing fallback.
    out.push_str("<script>");
    out.push_str(RELATIVE_TIME_JS);
    out.push_str("</script>\n");
    out.push_str("</body></html>\n");
}

const RELATIVE_TIME_JS: &str = r#"
(function () {
  function rel(iso) {
    var t = Date.parse(iso);
    if (isNaN(t)) return iso;
    var s = Math.round((Date.now() - t) / 1000);
    var sign = s >= 0 ? '' : 'in ';
    var abs = Math.abs(s);
    if (s < 0) s = abs;
    var unit;
    if (abs < 60) { unit = abs + 's'; }
    else if (abs < 3600) { unit = Math.round(abs/60) + 'm'; }
    else if (abs < 86400) { unit = Math.round(abs/3600) + 'h'; }
    else if (abs < 86400*30) { unit = Math.round(abs/86400) + 'd'; }
    else if (abs < 86400*365) { unit = Math.round(abs/(86400*30)) + 'mo'; }
    else { unit = Math.round(abs/(86400*365)) + 'y'; }
    return sign + unit + (s >= 0 ? ' ago' : '');
  }
  var nodes = document.querySelectorAll('[data-iso]');
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i];
    var iso = n.getAttribute('data-iso');
    n.title = iso;
    n.textContent = rel(iso);
  }
  document.querySelectorAll('.sha-copy').forEach(function (btn) {
    btn.addEventListener('click', function (ev) {
      ev.preventDefault();
      ev.stopPropagation();
      var full = btn.getAttribute('data-sha') || btn.textContent;
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(full).then(function () {
          btn.classList.add('copied');
          setTimeout(function () { btn.classList.remove('copied'); }, 1000);
        });
      }
    });
  });
})();
"#;

// ─────────────────────────── CSS ────────────────────────────

const CSS: &str = r#":root {
  color-scheme: light dark;
  --fg: #1a1a1a;
  --fg-dim: #5a5a5a;
  --bg: #fbfbf9;
  --rule: #e5e2dd;
  --pill-bg: #ece7da;
  --pill-fg: #5a4a1c;
  --link: #0a5b8a;
  --approve: #157a3e;
  --finished: #3a4cc8;
  --changes: #b13e2c;
  --add-bg: #e5f5e9;
  --del-bg: #fde7e3;
  --hunk-hdr: #a0a4ac;
  --mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --fg: #e9e6df;
    --fg-dim: #9b958a;
    --bg: #181715;
    --rule: #2c2a26;
    --pill-bg: #3a3527;
    --pill-fg: #d8c89c;
    --link: #6fb5e0;
    --approve: #6fc28e;
    --finished: #99a8ff;
    --changes: #ef8a78;
    --add-bg: rgba(40, 120, 60, 0.18);
    --del-bg: rgba(180, 60, 50, 0.18);
    --hunk-hdr: #6f6a60;
  }
}
* { box-sizing: border-box; }
html { font-feature-settings: "tnum"; }
body {
  margin: 0;
  font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  color: var(--fg);
  background: var(--bg);
}
main, header.status {
  max-width: 880px;
  margin: 0 auto;
  padding: 1.25rem 1.5rem;
}
header.status { border-bottom: 1px solid var(--rule); }
header.status h1 { margin: 0 0 .25rem; font-size: 1.2rem; }
.status-meta { color: var(--fg-dim); font-size: .9rem; margin-bottom: .75rem; }
.branch { color: var(--fg); font-weight: 600; }
.plan-list { list-style: none; margin: 0; padding: 0; }
.plan-list li { padding: .35rem 0; border-top: 1px solid var(--rule); }
.plan-list li:first-child { border-top: 0; }
.last-finished, .queue, .blocks { color: var(--fg-dim); font-size: .9rem; margin-top: .5rem; }
.empty { color: var(--fg-dim); font-style: italic; }
.plan-pill {
  display: inline-block;
  font: 600 .8rem/1 var(--mono);
  background: var(--pill-bg); color: var(--pill-fg);
  padding: .15rem .45rem; border-radius: 999px;
}
a.plan-pill { text-decoration: none; }
a.plan-pill:hover { filter: brightness(0.96); }
.plan-header { max-width: 880px; margin: 0 auto; padding: 1rem 1.5rem; border-bottom: 1px solid var(--rule); }
.plan-header h1 { margin: .25rem 0; }
.plan-state { color: var(--fg-dim); font-size: .9rem; margin-top: .25rem; }
.gate { font-size: .8rem; padding: 0 .35rem; border-radius: 4px; }
.gate-continued { color: var(--approve); }
.gate-finished { color: var(--finished); }
.gate-changes_requested { color: var(--changes); }
h2 { font-size: 1rem; text-transform: uppercase; letter-spacing: .05em; color: var(--fg-dim); margin: 1rem 0 .5rem; }
.timeline { margin: 0; padding: 0; }
.umbrella { margin: .65rem 0; border-left: 3px solid var(--rule); padding-left: .6rem; }
.umbrella + .umbrella { margin-top: 1rem; }
.umbrella-header { font: 600 .8rem/1 var(--mono); margin-bottom: .25rem; }
.umbrella-adhoc > .umbrella-header { color: var(--fg-dim); }
.adhoc-label { display: inline-block; padding: .15rem .45rem; border-radius: 999px; background: transparent; color: var(--fg-dim); text-transform: lowercase; letter-spacing: .04em; }
.row {
  display: grid;
  grid-template-columns: 5rem 4.5rem 1fr auto auto;
  gap: .6rem;
  align-items: center;
  padding: .3rem .15rem;
  line-height: 1.25;
  border-top: 1px solid var(--rule);
}
.umbrella > .row:first-of-type { border-top: 0; }
.row:hover { background: var(--pill-bg); }
.row-link {
  display: contents;
  text-decoration: none; color: inherit;
}
.row > .sha-copy { justify-self: start; }
.sha { font: 500 .85rem/1 var(--mono); color: var(--fg-dim); }
.sha-copy {
  font: 500 .85rem/1 var(--mono);
  background: transparent;
  border: 0;
  color: var(--fg-dim);
  padding: .15rem .3rem;
  border-radius: 3px;
  cursor: pointer;
}
.sha-copy:hover { background: var(--pill-bg); color: var(--fg); }
.row:hover .sha-copy { color: var(--fg); }
.sha-copy.copied { color: var(--approve); }
.sha-copy.copied::after { content: " copied"; font-size: .75em; }
.commit-body {
  font: .9rem/1.45 var(--mono);
  background: var(--pill-bg);
  padding: .65rem .85rem;
  border-radius: 4px;
  margin: .5rem 0;
  white-space: pre-wrap;
  overflow-x: auto;
}
.kind { font: 600 .75rem/1 var(--mono); text-transform: uppercase; letter-spacing: .04em; color: var(--fg-dim); }
.kind-finish, .kind-intro { color: var(--finished); }
.kind-code, .kind-mixed { color: var(--approve); }
.kind-delete { color: var(--changes); }
.subject { color: var(--fg); }
.marks { display: inline-flex; gap: .25rem; }
.mark { font: 600 .8rem/1 var(--mono); padding: 0 .2rem; border-radius: 3px; }
.mark-continue { color: var(--approve); }
.mark-finished { color: var(--finished); }
.mark-request-changes { color: var(--changes); }
.mark-unmarked { color: var(--fg-dim); }
.ts { font: 400 .8rem/1 var(--mono); color: var(--fg-dim); }
.crumb a { color: var(--link); text-decoration: none; font-size: .9rem; }
.commit-header { padding: 1rem 1.5rem; border-bottom: 1px solid var(--rule); max-width: 880px; margin: 0 auto; }
.commit-header h1 { margin: .25rem 0; font: 500 1.4rem/1 var(--mono); }
.commit-header .subject { font-size: 1.05rem; margin: .25rem 0; }
.sha-full code { font-size: .8rem; color: var(--fg-dim); }
section { padding: 1rem 0; border-top: 1px solid var(--rule); }
section:first-of-type { border-top: 0; }
section h3 { font-size: .9rem; text-transform: uppercase; letter-spacing: .05em; color: var(--fg-dim); margin: 0 0 .75rem; }
.md { line-height: 1.55; }
.md h1, .md h2, .md h3 { margin: 1.25rem 0 .5rem; }
.md h1 { font-size: 1.4rem; }
.md h2 { font-size: 1.15rem; text-transform: none; color: inherit; letter-spacing: 0; }
.md h3 { font-size: 1rem; text-transform: none; color: inherit; letter-spacing: 0; }
.md p { margin: .65rem 0; }
.md code { font: .9em var(--mono); background: var(--pill-bg); padding: 0 .25rem; border-radius: 3px; }
.md pre { background: var(--pill-bg); padding: .75rem; border-radius: 4px; overflow-x: auto; }
.md pre code { background: transparent; padding: 0; }
.md ul, .md ol { padding-left: 1.5rem; }
.md blockquote { border-left: 3px solid var(--rule); margin: .5rem 0; padding: .1rem .75rem; color: var(--fg-dim); }
.review { padding: .75rem 1rem; border: 1px solid var(--rule); border-radius: 6px; margin-bottom: .75rem; }
.review header { margin-bottom: .35rem; }
.review .verdict { font: 600 .75rem/1 var(--mono); margin-left: .35rem; padding: .15rem .35rem; border-radius: 3px; }
.review.verdict-continue .verdict { color: var(--approve); background: rgba(21, 122, 62, 0.1); }
.review.verdict-finished .verdict { color: var(--finished); background: rgba(58, 76, 200, 0.1); }
.review.verdict-request-changes .verdict { color: var(--changes); background: rgba(177, 62, 44, 0.1); }
.review .summary { margin: .25rem 0 .5rem; font-weight: 500; }
.file { margin: .5rem 0; }
.file summary { font: 500 .9rem/1.4 var(--mono); cursor: pointer; padding: .25rem .35rem; background: var(--pill-bg); border-radius: 3px; }
.hunk { font: .8rem/1.25 var(--mono); margin: .35rem 0; background: var(--bg); border: 1px solid var(--rule); border-radius: 3px; overflow-x: auto; }
.line {
  display: grid;
  grid-template-columns: 3.2em 3.2em 1.1em 1fr;
  white-space: pre;
}
.line .ln {
  text-align: right;
  padding: 0 .35em;
  color: var(--hunk-hdr);
  user-select: none;
  font-feature-settings: "tnum";
}
.line .sign { text-align: center; color: var(--fg-dim); }
.line .src { padding-right: .35em; }
.line.add { background: var(--add-bg); }
.line.del { background: var(--del-bg); }
.line.hunk-hdr { background: var(--pill-bg); color: var(--hunk-hdr); padding: .1rem 0; grid-template-columns: 1fr; }
.line.hunk-hdr .src { padding-left: .5rem; }
.line.meta { color: var(--fg-dim); }
/* syntect class colors (light) */
.hl-keyword { color: #a626a4; }
.hl-storage { color: #a626a4; }
.hl-constant { color: #986801; }
.hl-string { color: #50a14f; }
.hl-comment { color: var(--fg-dim); font-style: italic; }
.hl-entity { color: #4078f2; }
.hl-support { color: #0184bc; }
.hl-variable { color: #e45649; }
.hl-punctuation { color: var(--fg-dim); }
@media (prefers-color-scheme: dark) {
  .hl-keyword { color: #c678dd; }
  .hl-storage { color: #c678dd; }
  .hl-constant { color: #d19a66; }
  .hl-string { color: #98c379; }
  .hl-comment { color: #7c7c7c; font-style: italic; }
  .hl-entity { color: #61afef; }
  .hl-support { color: #56b6c2; }
  .hl-variable { color: #e06c75; }
  .hl-punctuation { color: #8e8c89; }
}
@media print {
  .row-link:hover { background: transparent; }
  details > summary { list-style: none; }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_page_building_state_shows_count_and_refreshes() {
        let html = render_loading_page(
            "commit a1b2c3d",
            LoadingState::Building { done: 7, total: 16 },
        );
        assert!(html.contains("7 / 16 pages"), "count: {html}");
        assert!(html.contains("commit a1b2c3d"), "target label");
        // Self-refreshing while building.
        assert!(html.contains(r#"<meta http-equiv="refresh" content="1">"#));
        // No white flash: dark bg set inline + color-scheme.
        assert!(html.contains(r#"<html lang="en" style="background:#0b0b0d">"#));
        assert!(html.contains(r#"content="dark""#));
        // Some cells filled, some track.
        assert!(html.contains('\u{2593}') && html.contains('\u{2591}'));
    }

    #[test]
    fn loading_page_done_state_redirects_to_the_relative_target() {
        let html = render_loading_page(
            "commit a1b2c3d",
            LoadingState::Done {
                url: "commit/abcdef.html",
            },
        );
        // EXACT relative redirect — never absolute / wrong-based (file://).
        assert!(
            html.contains(r#"<meta http-equiv="refresh" content="0; url=commit/abcdef.html">"#),
            "redirect: {html}"
        );
    }

    #[test]
    fn loading_page_error_state_stops_refreshing() {
        let html = render_loading_page(
            "commit a1b2c3d",
            LoadingState::Error {
                message: "build failed: boom",
            },
        );
        // No refresh META element (the watchdog script may still name the
        // selector, so match the actual tag, not the bare string).
        assert!(
            !html.contains(r#"<meta http-equiv="refresh""#),
            "no refresh meta in error state"
        );
        assert!(html.contains("build failed: boom"));
        assert!(html.contains("#f26d6d"), "error accent");
    }

    #[test]
    fn target_rel_url_is_relative_to_the_html_root() {
        let out = Path::new("/x/.clank/html");
        assert_eq!(
            target_rel_url(out, Path::new("/x/.clank/html/commit/abc.html")),
            "commit/abc.html"
        );
        assert_eq!(
            target_rel_url(out, Path::new("/x/.clank/html/plan/foo.html")),
            "plan/foo.html"
        );
    }

    #[test]
    fn loading_page_throttles_rewrites_to_once_per_second() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("_loading.html");
        let lp = LoadingPage::new(path.clone(), "commit/x.html".into(), "commit x".into());
        lp.set_total(10);
        let t0 = std::time::Instant::now();
        // First tick just under 1s after the set_total write → throttled (no
        // rewrite), so the file still shows 0.
        lp.tick_at(t0);
        lp.tick_at(t0 + std::time::Duration::from_millis(500));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("0 / 10 pages"),
            "throttled: still 0; got count line"
        );
        // A tick past 1s rewrites with the accumulated count (3 so far).
        lp.tick_at(t0 + std::time::Duration::from_secs(1));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("3 / 10 pages"), "rewrote after 1s: {body}");
    }

    #[test]
    fn render_markdown_highlights_fenced_block() {
        let md = "\n```rust\nfn main() {}\n```\n";
        let html = render_markdown(md);
        assert!(
            html.contains("<pre><code class=\"hl\">"),
            "expected highlighted code wrapper; got: {html}"
        );
        assert!(
            html.contains("class=\"hl-"),
            "expected syntect hl- token classes; got: {html}"
        );
    }

    #[test]
    fn render_markdown_untyped_fence_is_plain_safe() {
        let md = "\n```\n<div>raw</div>\n```\n";
        let html = render_markdown(md);
        assert!(
            html.contains("<pre><code class=\"hl\">"),
            "untyped fences still go through the highlight wrapper; got: {html}"
        );
        // No raw HTML passthrough — the <div> is escaped.
        assert!(
            html.contains("&lt;div&gt;"),
            "raw HTML inside untyped fence must be escaped; got: {html}"
        );
        assert!(
            !html.contains("<div>raw</div>"),
            "raw HTML must not pass through; got: {html}"
        );
    }

    #[test]
    fn render_markdown_html_filter_still_runs() {
        // Regression guard for the inline-HTML stripper that
        // already filtered <script> etc out of non-fenced
        // markdown. The new event walker must preserve this.
        let md = "<script>alert(1)</script>\n\nhello";
        let html = render_markdown(md);
        assert!(
            !html.contains("<script>"),
            "raw <script> must be stripped from non-code markdown; got: {html}"
        );
        assert!(
            html.contains("hello"),
            "regular markdown content must still render; got: {html}"
        );
    }

    #[test]
    fn render_markdown_unknown_lang_falls_back_to_plain_text() {
        let md = "\n```not-a-real-lang\nsome content <here>\n```\n";
        let html = render_markdown(md);
        // Plain-text fallback still escapes.
        assert!(html.contains("&lt;here&gt;"), "got: {html}");
        assert!(
            html.contains("<pre><code class=\"hl\">"),
            "wrapper still applied; got: {html}"
        );
    }
}
