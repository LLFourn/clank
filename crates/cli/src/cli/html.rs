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
    build_site(
        &repo,
        &basename,
        &out_dir,
        home.as_deref(),
        args.rebuild,
        &progress,
    )
    .await?;
    progress.finish();

    // Under `--print-path`, stdout must contain ONLY the resolved
    // path so callers can use `$(clank html open <plan> --print-path)`
    // verbatim. Route the build-info line to stderr in that mode.
    // Codex caught the dual-line stdout on dfe596d.
    let print_path = matches!(
        args.command,
        Some(HtmlCmd::Open(crate::cli::HtmlOpenArgs {
            print_path: true,
            ..
        }))
    );
    if print_path {
        eprintln!("wrote {}", out_dir.display());
    } else {
        println!("wrote {}", out_dir.display());
    }

    if let Some(HtmlCmd::Open(open_args)) = args.command {
        let target = resolve_open_target(&repo, &basename, &out_dir, &open_args).await?;
        if open_args.print_path {
            println!("{}", target.display());
        } else {
            launch_opener(&target)?;
        }
    }
    Ok(())
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
    build_site(repo, &basename, out_dir, home, rebuild, &progress).await
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

async fn build_site(
    repo: &Path,
    basename: &str,
    out_dir: &Path,
    home: Option<&Path>,
    mut force_rebuild: bool,
    progress: &Progress,
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
    progress.begin("writing pages", writes_needed.len());
    for (i, event) in writes_needed.iter().enumerate() {
        let sha = event_sha(event);
        let path = out_dir.join(format!("commit/{}.html", sha.as_str()));
        let page = render_commit_page(repo, event, &reviews);
        std::fs::write(&path, page)?;
        progress.tick(i + 1);
    }
    progress.end();

    // Plan pages. One per active or finished plan.
    //
    // On full rebuild every page is rewritten. On incremental,
    // affected = (slice plans) ∪ (plans whose commits are in
    // writes_needed). The second term covers feedback-only
    // rebuilds at the same HEAD: a new review on a top-N
    // commit changes the verdict marks that the plan page
    // also renders, so the page must refresh too. The cost is
    // that an unrelated slice still re-renders plan pages
    // whose commits happen to be in top-N — acceptable; they
    // age out of top-N quickly and the rendered output is
    // identical when reviews haven't actually changed.
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
    for (key, run) in clank_core::repo_state::umbrella_sections(&newest_first) {
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
        PlanLifecycle::Active => format!(".clank/plans/{stem}.md"),
        PlanLifecycle::Finished => format!(".clank/finished/{stem}.md"),
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
        G::Approved => "approved",
        G::Finished => "finished",
        G::ChangesRequested => "changes-requested",
        G::Unreviewed => "unreviewed",
        G::Blocked => "blocked",
        G::ApprovedPendingGate => "approved-pending-gate",
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
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", "--no-color", "--pretty=format:", sha.as_str()])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
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
        Verdict::Approve => "APPROVE",
        Verdict::Finished => "FINISHED",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Unmarked => "(no verdict)",
    }
}

fn verdict_slug(v: Verdict) -> &'static str {
    match v {
        Verdict::Approve => "approve",
        Verdict::Finished => "finished",
        Verdict::RequestChanges => "request-changes",
        Verdict::Unmarked => "unmarked",
    }
}

fn verdict_marks_html(reviews: &[Review]) -> String {
    let mut s = String::new();
    for r in reviews {
        let (mark, title) = match r.verdict {
            Verdict::Approve => ("✓", format!("APPROVE by {}", r.author)),
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
    crate::git_io::commit_subject(repo, sha)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The commit message body (everything after the subject line
/// and its trailing blank). Empty when the commit has only a
/// subject.
fn commit_body(repo: &Path, sha: &CommitSha) -> String {
    crate::git_io::commit_body(repo, sha)
        .map(|b| b.trim().to_string())
        .unwrap_or_default()
}

/// Batch-fetch commit subjects via a single `git log` so the
/// index doesn't shell out per row.
fn collect_subjects(repo: &Path, head: Option<&CommitSha>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(head) = head else { return out };
    let res = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--format=%H%x09%s", head.as_str()])
        .output();
    let Ok(res) = res else { return out };
    if !res.status.success() {
        return out;
    }
    for line in String::from_utf8_lossy(&res.stdout).lines() {
        if let Some((sha, subj)) = line.split_once('\t') {
            out.insert(sha.to_string(), subj.to_string());
        }
    }
    out
}

fn plan_body_at_commit(repo: &Path, event: &LogEvent) -> Option<String> {
    let sha = event_sha(event);
    let path = match event {
        LogEvent::PlanIntro { plan, .. }
        | LogEvent::PlanCommit { plan, .. }
        | LogEvent::PlanDeleted { plan, .. } => {
            format!(".clank/plans/{}.md", plan.as_str())
        }
        LogEvent::PlanFinalized { plan, .. } => {
            format!(".clank/finished/{}.md", plan.as_str())
        }
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
.gate-approved { color: var(--approve); }
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
.mark-approve { color: var(--approve); }
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
.review.verdict-approve .verdict { color: var(--approve); background: rgba(21, 122, 62, 0.1); }
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
