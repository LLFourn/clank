//! `clank log` — chronological timeline of commits and reviews.
//!
//! Default: last 30 commits from HEAD. `--plan` filters to one plan.
//! Explicit `<range>` overrides the default window.

use std::path::Path;

use super::{LogArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::feedback_scan::scan_feedback;
use crate::lifecycle::{CommitSha, PlanKey};
use clank_core::repo_state::LogEvent;
use clank_core::vocab::Verdict;

pub async fn run(args: LogArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;

    let head = git_rev_parse(&repo, "HEAD").ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;

    let plan_filter: Option<PlanKey> = match args.plan.as_deref() {
        Some(raw) => {
            let stem = parse_arg(raw, &basename)?;
            Some(
                PlanKey::parse(&stem)
                    .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?,
            )
        }
        None => None,
    };

    let (from, to) = match &args.range {
        Some(range) => parse_range(&repo, range)?,
        None => {
            let from = if args.limit > 0 {
                git_rev_parse(&repo, &format!("HEAD~{}", args.limit))
            } else {
                None
            };
            (from, head)
        }
    };

    let (_state, log_events) = crate::rebuild::rebuild_from(&repo, from.as_ref(), &to)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold range: {e}"))?;

    let filtered: Vec<&LogEvent> = log_events
        .iter()
        .filter(|e| match e {
            LogEvent::AdHoc { .. } => plan_filter.is_none(),
            LogEvent::PlanIntro { plan, .. }
            | LogEvent::PlanCommit { plan, .. }
            | LogEvent::PlanFinalized { plan, .. }
            | LogEvent::PlanDeleted { plan, .. } => plan_filter.as_ref().is_none_or(|f| f == plan),
        })
        .collect();

    // Github events interleave by time (log-timeline-github-events)
    // unless excluded; a plan-scoped view is plan work only, so the
    // repo-wide github stream stays out of it too.
    let gh_events: Vec<crate::cli::github_timeline::MergedEvent> =
        if args.no_github || plan_filter.is_some() {
            Vec::new()
        } else {
            let snap = crate::cli::github_timeline::timeline_snapshot(&repo);
            for n in &snap.notices {
                eprintln!("log: {n}");
            }
            snap.events
        };

    if filtered.is_empty() && gh_events.is_empty() {
        println!("no log events");
        return Ok(());
    }

    // Most recent first (git log convention).
    let filtered: Vec<&LogEvent> = filtered.into_iter().rev().collect();
    let items = interleave(&filtered, &gh_events);

    let reviewable_shas: Vec<CommitSha> = filtered
        .iter()
        .filter_map(|e| match e {
            LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanIntro { sha, .. }
            | LogEvent::AdHoc { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .collect();

    if args.json {
        print_json(&items, &repo, &reviewable_shas)?;
    } else if args.oneline {
        print_oneline(&items, &repo, &reviewable_shas)?;
    } else {
        print_human(&items, &repo, &reviewable_shas)?;
    }
    Ok(())
}

/// One interleaved timeline entry: a fold event or a merged github
/// event (log-timeline-github-events).
pub(crate) enum TimelineItem<'a> {
    Repo(&'a LogEvent),
    Github(&'a crate::cli::github_timeline::MergedEvent),
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

/// Two-pointer newest-first merge preserving each list's own order.
/// Github events OLDER than the oldest displayed commit fall outside
/// the folded range and are excluded (when any commits are shown at
/// all); newer-than-newest ones lead the view — a just-arrived event
/// is exactly what the reader wants on top. `gh` arrives
/// oldest-first (the snapshot's order).
pub(crate) fn interleave<'a>(
    events: &[&'a LogEvent],
    gh: &'a [crate::cli::github_timeline::MergedEvent],
) -> Vec<TimelineItem<'a>> {
    let floor: Option<i64> = events.last().map(|e| event_ts(e));
    let mut gh_desc: Vec<&crate::cli::github_timeline::MergedEvent> = gh
        .iter()
        .filter(|g| {
            let t = i64::try_from(g.at).unwrap_or(i64::MAX);
            floor.is_none_or(|f| t >= f)
        })
        .collect();
    gh_desc.reverse();
    let mut out = Vec::with_capacity(events.len() + gh_desc.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < events.len() || j < gh_desc.len() {
        let take_gh = match (events.get(i), gh_desc.get(j)) {
            (Some(e), Some(g)) => i64::try_from(g.at).unwrap_or(i64::MAX) >= event_ts(e),
            (None, Some(_)) => true,
            _ => false,
        };
        if take_gh {
            out.push(TimelineItem::Github(gh_desc[j]));
            j += 1;
        } else {
            out.push(TimelineItem::Repo(events[i]));
            i += 1;
        }
    }
    out
}

/// One-line description shared by the oneline and human renderers —
/// mirrors the `clank events` CLI's shape so the two surfaces read
/// the same.
fn gh_describe(g: &crate::cli::github_timeline::MergedEvent) -> String {
    let mut out = g.event.clone();
    if let Some(d) = &g.detail {
        out.push_str(&format!("/{d}"));
    }
    out.push_str(&format!("  {}", g.repo));
    if let Some(n) = g.number {
        out.push_str(&format!("#{n}"));
    }
    if let Some(t) = &g.title {
        out.push_str(&format!("  “{t}”"));
    }
    if let Some(a) = &g.actor {
        out.push_str(&format!("  by {a}"));
    }
    out
}

// ── git helpers ──────────────────────────────────────────────

pub(crate) fn git_rev_parse(repo: &Path, rev: &str) -> Option<CommitSha> {
    crate::git_io::resolve_commit(repo, rev)
}

fn parse_range(repo: &Path, range: &str) -> anyhow::Result<(Option<CommitSha>, CommitSha)> {
    if let Some((from_str, to_str)) = range.split_once("..") {
        let from = git_rev_parse(repo, from_str)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve `{from_str}`"))?;
        let to = git_rev_parse(repo, to_str)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve `{to_str}`"))?;
        Ok((Some(from), to))
    } else {
        let _sha = git_rev_parse(repo, range)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve `{range}`"))?;
        let parent = git_rev_parse(repo, &format!("{range}^"));
        let head =
            git_rev_parse(repo, "HEAD").ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;
        Ok((parent, head))
    }
}

fn commit_info(repo: &Path, sha: &CommitSha) -> (String, String, String) {
    crate::git_io::commit_meta(repo, sha)
}

pub(crate) fn short(sha: &CommitSha) -> &str {
    &sha.as_str()[..sha.as_str().len().min(7)]
}

// ── reviews ─────────────────────────────────────────────────

pub(crate) struct Review {
    pub(crate) author: String,
    pub(crate) verdict: Verdict,
    pub(crate) summary: String,
    pub(crate) body: String,
}

pub(crate) fn collect_reviews(
    repo: &Path,
    reviewable_shas: &[CommitSha],
) -> std::collections::BTreeMap<String, Vec<Review>> {
    use clank_core::feedback_body::FeedbackBody;
    let mut out = std::collections::BTreeMap::new();
    if let Ok(fv) = scan_feedback(repo, reviewable_shas) {
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
                    .or_insert_with(Vec::new)
                    .push(Review {
                        author: author.as_str().to_string(),
                        verdict: entry.verdict,
                        summary,
                        body,
                    });
            }
        }
    }
    out
}

// ── ANSI ────────────────────────────────────────────────────

const Y: &str = "\x1b[33m";
const C: &str = "\x1b[36m";
const G: &str = "\x1b[32m";
const R: &str = "\x1b[31m";
const Z: &str = "\x1b[0m";
fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn kind_label(tp: bool, tc: bool) -> &'static str {
    match (tp, tc) {
        (true, true) => "plan+code",
        (true, false) => "plan",
        (false, true) => "code",
        (false, false) => "",
    }
}

pub(crate) fn verdict_mark(v: Verdict, c: bool) -> String {
    let (mark, col) = match v {
        Verdict::Continue => ("✓", G),
        Verdict::Finished => ("✓✓", C),
        Verdict::RequestChanges => ("✗", R),
        Verdict::Unmarked => ("?", Z),
    };
    if c {
        format!("{col}{mark}{Z}")
    } else {
        mark.to_string()
    }
}

// ── renderers ───────────────────────────────────────────────

fn print_human(items: &[TimelineItem], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let c = color();
    let mut started = false;
    let mut chunk: Vec<&LogEvent> = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(e) => chunk.push(e),
            TimelineItem::Github(g) => {
                print_human_events(&chunk, repo, &reviews, c, &mut started)?;
                chunk.clear();
                if started {
                    println!();
                }
                started = true;
                let line = gh_describe(g);
                let mark = if g.unhandled { "  [UNHANDLED]" } else { "" };
                if c {
                    println!("{C}gh{Z} {line}{mark}");
                } else {
                    println!("gh {line}{mark}");
                }
                if !g.seen_by.is_empty() {
                    println!("Seen-by: {}", g.seen_by.join(", "));
                }
                if let Some(url) = &g.url {
                    println!("{url}");
                }
            }
        }
    }
    print_human_events(&chunk, repo, &reviews, c, &mut started)
}

fn print_human_events(
    events: &[&LogEvent],
    repo: &Path,
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    c: bool,
    started: &mut bool,
) -> anyhow::Result<()> {
    for event in events.iter() {
        if *started {
            println!();
        }
        *started = true;
        let (plan, sha, kind) = match event {
            LogEvent::PlanIntro { plan, sha, .. } => (plan, sha, "plan"),
            LogEvent::PlanCommit {
                plan,
                sha,
                touched_plan,
                touched_code,
                ..
            } => (plan, sha, kind_label(*touched_plan, *touched_code)),
            LogEvent::PlanFinalized { plan, sha, .. } => {
                if c {
                    println!(
                        "{Y}finalize {}{Z} {C}(plan: {}){Z}",
                        short(sha),
                        plan.as_str()
                    );
                } else {
                    println!("finalize {} (plan: {})", short(sha), plan.as_str());
                }
                continue;
            }
            LogEvent::PlanDeleted { plan, sha, .. } => {
                println!("delete {} (plan: {})", short(sha), plan.as_str());
                continue;
            }
            LogEvent::AdHoc { sha, .. } => {
                let (author, date, body) = commit_info(repo, sha);
                if c {
                    println!("{Y}commit {}{Z}", sha.as_str());
                } else {
                    println!("commit {}", sha.as_str());
                }
                println!("Author: {author}");
                println!("Date:   {date}");
                println!();
                for line in body.lines() {
                    println!("    {line}");
                }
                // Ad-hoc commits carry reviews too
                // (review.adhoc_feedback — adhoc-reviews-in-log).
                print_reviews(reviews, sha.as_str(), c);
                continue;
            }
        };
        let (author, date, body) = commit_info(repo, sha);
        if c {
            println!(
                "{Y}commit {}{Z} {C}(plan: {}, {kind}){Z}",
                sha.as_str(),
                plan.as_str()
            );
        } else {
            println!("commit {} (plan: {}, {kind})", sha.as_str(), plan.as_str());
        }
        println!("Author: {author}");
        println!("Date:   {date}");
        println!();
        for line in body.lines() {
            println!("    {line}");
        }
        print_reviews(reviews, sha.as_str(), c);
    }
    Ok(())
}

fn print_reviews(reviews: &std::collections::BTreeMap<String, Vec<Review>>, sha: &str, c: bool) {
    if let Some(rs) = reviews.get(sha) {
        for line in human_review_lines(rs, c) {
            println!("{line}");
        }
    }
}

/// The `clank log` review block under a commit, as lines — pure so the
/// shape is testable without capturing stdout (adhoc-reviews-in-log).
fn human_review_lines(rs: &[Review], c: bool) -> Vec<String> {
    let mut out = Vec::new();
    for r in rs {
        let m = verdict_mark(r.verdict, c);
        let snip = if r.summary.is_empty() {
            String::new()
        } else {
            format!(": {}", r.summary)
        };
        if c {
            out.push(format!("    {m} {C}{}{Z} {}{snip}", r.author, r.verdict));
        } else {
            out.push(format!("    {m} {} {}{snip}", r.author, r.verdict));
        }
        if !r.body.is_empty() {
            out.push(String::new());
            for line in r.body.lines() {
                out.push(format!("        {line}"));
            }
        }
    }
    out
}

/// The leading 1-col gutter marker on a commit row — mutually exclusive by
/// construction (a commit is exactly one of these), replacing the old
/// `ad_hoc: bool`. The icon LEADS the row (before the sha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowMarker {
    /// No icon (space) — a plan-delete row.
    Plain,
    /// Ad-hoc commit (no `[plan]` tag).
    AdHoc,
    /// Plan commit that only touched the plan doc (intro, or a plan-only
    /// revise) — planning, not code.
    Planning,
    /// Plan commit that touched code — implementation.
    Impl,
    /// The plan's finalize commit (its message is now a real whole-plan
    /// summary, so the flag is what identifies it at a glance).
    Finish,
}

impl RowMarker {
    /// Classify a timeline event. Implementation (touched code) vs planning
    /// (plan-doc-only) is read from `PlanCommit::touched_code`; an intro is
    /// always planning.
    fn of(event: &LogEvent) -> RowMarker {
        match event {
            LogEvent::AdHoc { .. } => RowMarker::AdHoc,
            LogEvent::PlanFinalized { .. } => RowMarker::Finish,
            LogEvent::PlanIntro { .. } => RowMarker::Planning,
            LogEvent::PlanCommit { touched_code, .. } => {
                if *touched_code {
                    RowMarker::Impl
                } else {
                    RowMarker::Planning
                }
            }
            LogEvent::PlanDeleted { .. } => RowMarker::Plain,
        }
    }

    /// The 1-col gutter glyph. Every glyph is East-Asian-Width Neutral/Narrow
    /// → 1 column, preserving the fixed 1-col gutter that keeps rows aligned
    /// in the narrow TUI pane (a test pins each width at 1). Emoji (🔨/📜/🏁)
    /// are 2-col and would break alignment — deliberately avoided.
    pub(crate) fn glyph(self) -> char {
        match self {
            RowMarker::Plain => ' ',
            RowMarker::AdHoc => '~',
            RowMarker::Planning => '✎',
            RowMarker::Impl => '⚒',
            RowMarker::Finish => '⚑',
        }
    }

    /// ANSI color for the glyph in the colored CLI renderer (`""` = none).
    fn ansi(self) -> &'static str {
        match self {
            RowMarker::Finish => C, // cyan, matching the Finished verdict
            RowMarker::AdHoc => Y,  // yellow (unchanged from the old `~`)
            _ => "",
        }
    }
}

/// One `--oneline` display row: data only, no formatting — shared
/// by the CLI renderer (which colors it) and the status TUI's log
/// pane (which dims it). Plan: status-tui-live-log.
#[derive(Debug)]
pub(crate) enum OnelineRow {
    /// Umbrella header: the plan that groups the following commit
    /// rows, or `None` for the ad-hoc bucket
    /// (log-plan-umbrellas).
    Header { plan: Option<String> },
    Commit {
        sha: CommitSha,
        /// Subject with the `[plan]` prefix STRIPPED when it
        /// matches the enclosing umbrella's plan (redundant under
        /// the header); verbatim otherwise.
        subject: String,
        /// Gutter marker: `AdHoc` (no `[plan]` tag, folded into the
        /// surrounding umbrella — `~`), `Finish` (the plan's finalize
        /// commit — `⚑`), or `Plain`.
        marker: RowMarker,
    },
    Review {
        verdict: Verdict,
        author: String,
        summary: String,
    },
    /// A merged github event interleaved into the timeline
    /// (log-timeline-github-events); `line` is [`gh_describe`]'s
    /// shape, shared with the CLI renderers.
    Github { line: String, unhandled: bool },
    /// A timeline-read notice (corrupt/foreign logs) surfaced as a
    /// dim row — the TUI's rendering of the snapshot's notices.
    Notice(String),
}

/// The interleaved oneline row sequence — pure, shared by the status
/// TUI's log pane (and testable without a repo): repo chunks through
/// [`oneline_rows`], github entries as [`OnelineRow::Github`] rows in
/// timeline position.
pub(crate) fn oneline_items_rows(
    items: &[TimelineItem],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
) -> Vec<OnelineRow> {
    let mut out = Vec::new();
    let mut chunk: Vec<&LogEvent> = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(e) => chunk.push(e),
            TimelineItem::Github(g) => {
                out.extend(oneline_rows(&chunk, reviews));
                chunk.clear();
                out.push(OnelineRow::Github {
                    line: gh_describe(g),
                    unhandled: g.unhandled,
                });
            }
        }
    }
    out.extend(oneline_rows(&chunk, reviews));
    out
}

/// Pure row producer for the oneline view. Subjects come from the
/// fold's `LogEvent`s (carried since status-tui-live-log) — no
/// per-event `git log -1` shelling, which matters for the live TUI
/// pane re-rendering every refresh.
///
/// `events` are NEWEST-FIRST (the git-log convention every caller —
/// `print_oneline`, the status TUI's `tui_log_rows` — uses); that
/// order is what `umbrella_sections` folds ad-hoc commits by.
pub(crate) fn oneline_rows(
    events: &[&LogEvent],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
) -> Vec<OnelineRow> {
    use clank_core::repo_state::{UmbrellaKey, parse_subject, umbrella_sections};
    let mut out = Vec::new();
    for (key, run) in umbrella_sections(events, true) {
        let umbrella_plan = match &key {
            UmbrellaKey::Plan(p) => Some(p.as_str().to_string()),
            UmbrellaKey::AdHoc => None,
        };
        // Ad-hoc commits fold into the surrounding plan and are marked
        // per-row, so there is no "adhoc" header. A header prints only
        // for a real plan umbrella; a leading ad-hoc run (no plan)
        // renders its marked rows with no header (adhoc-commit-marker).
        if umbrella_plan.is_some() {
            out.push(OnelineRow::Header {
                plan: umbrella_plan.clone(),
            });
        }
        for event in run {
            let (sha, subject) = match event {
                LogEvent::PlanIntro { sha, subject, .. }
                | LogEvent::PlanCommit { sha, subject, .. }
                | LogEvent::AdHoc { sha, subject, .. } => (sha, subject.clone()),
                LogEvent::PlanFinalized {
                    plan, sha, subject, ..
                } => {
                    // The real whole-plan finish message. Fall back to the
                    // synthesized label for pre-subject cached checkpoints
                    // (the CACHE_FORMAT_VERSION bump re-folds them, but
                    // degrade gracefully if one slips through).
                    let s = if subject.trim().is_empty() {
                        format!("[{}] finish", plan.as_str())
                    } else {
                        subject.clone()
                    };
                    (sha, s)
                }
                LogEvent::PlanDeleted { plan, sha, .. } => {
                    (sha, format!("Delete {}", plan.as_str()))
                }
            };
            // Strip the `[plan]` prefix only when it names EXACTLY
            // the umbrella's plan — a real parse
            // (core::parse_subject), never string-munging. Foreign
            // or multi-plan prefixes still carry information and
            // stay verbatim.
            let parsed = parse_subject(&subject);
            let strip = matches!(
                (&umbrella_plan, &parsed.prefix),
                (Some(up), Some(clank_core::repo_state::TitlePrefix::Plans(ps)))
                    if ps.len() == 1 && &ps[0] == up
            );
            let subject = if strip {
                parsed.body.to_string()
            } else {
                subject.clone()
            };
            // Reviews render ABOVE their commit: a review happens AFTER
            // its commit, so in a newest-first list it belongs above it
            // (time order — status-timeline-progress). Ad-hoc commits
            // carry reviews too (review.adhoc_feedback —
            // adhoc-reviews-in-log). Multiple reviews of one commit keep
            // `collect_reviews`' deterministic by-author order — `Review`
            // carries no timestamp to sort chronologically.
            if let Some(rs) = reviews.get(sha.as_str()) {
                for r in rs {
                    out.push(OnelineRow::Review {
                        verdict: r.verdict,
                        author: r.author.clone(),
                        summary: r.summary.clone(),
                    });
                }
            }
            out.push(OnelineRow::Commit {
                sha: sha.clone(),
                subject,
                marker: RowMarker::of(event),
            });
        }
    }
    out
}

/// Plain-text lines for one row set (the TUI's log pane). NO ANSI —
/// the consumer styles (the TUI dims via its span model).
pub(crate) fn oneline_plain_lines(rows: &[OnelineRow]) -> Vec<String> {
    rows.iter()
        .map(|row| match row {
            OnelineRow::Header { plan } => plan.clone().unwrap_or_else(|| "adhoc".to_string()),
            OnelineRow::Github { line, unhandled } => {
                format!("gh {line}{}", if *unhandled { " ⚠" } else { "" })
            }
            OnelineRow::Notice(n) => format!("({n})"),
            // The 1-col marker icon LEADS every commit row (finish/impl/
            // planning/adhoc), then the sha, then the subject. Fixed width
            // keeps subjects column-aligned.
            OnelineRow::Commit {
                sha,
                subject,
                marker,
            } => {
                format!("{} {} {subject}", marker.glyph(), short(sha))
            }
            OnelineRow::Review {
                verdict,
                author,
                summary,
            } => {
                let m = verdict_mark(*verdict, false);
                let snip = if summary.is_empty() {
                    String::new()
                } else {
                    format!(": {summary}")
                };
                format!("    {m} {author}{snip}")
            }
        })
        .collect()
}

fn print_oneline(items: &[TimelineItem], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let c = color();
    let mut chunk: Vec<&LogEvent> = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(e) => chunk.push(e),
            TimelineItem::Github(g) => {
                print_oneline_events(&chunk, &reviews, c);
                chunk.clear();
                // One row, marker-led like commits; unhandled events
                // carry the open-work mark.
                let line = gh_describe(g);
                let mark = if g.unhandled { " ⚠" } else { "" };
                if c {
                    println!("{C}gh{Z} {line}{mark}");
                } else {
                    println!("gh {line}{mark}");
                }
            }
        }
    }
    print_oneline_events(&chunk, &reviews, c);
    Ok(())
}

fn print_oneline_events(
    events: &[&LogEvent],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    c: bool,
) {
    for row in oneline_rows(events, reviews) {
        match row {
            // oneline_rows never produces these; the interleaving
            // caller prints github rows itself.
            OnelineRow::Github { .. } | OnelineRow::Notice(_) => {}
            OnelineRow::Header { plan } => {
                let name = plan.unwrap_or_else(|| "adhoc".to_string());
                if c {
                    println!("{C}{name}{Z}");
                } else {
                    println!("{name}");
                }
            }
            OnelineRow::Commit {
                sha,
                subject,
                marker,
            } => {
                // The marker icon LEADS the row (before the sha), then the
                // sha, then the subject. Fixed 1-col icon keeps subjects
                // aligned. Color is TTY-gated via `c` (no ANSI when piped).
                let g = marker.glyph();
                if c {
                    let icon = match marker.ansi() {
                        "" => g.to_string(),
                        col => format!("{col}{g}{Z}"),
                    };
                    println!("{icon} {Y}{}{Z} {subject}", short(&sha));
                } else {
                    println!("{g} {} {subject}", short(&sha));
                }
            }
            OnelineRow::Review {
                verdict,
                author,
                summary,
            } => {
                let m = verdict_mark(verdict, c);
                let snip = if summary.is_empty() {
                    String::new()
                } else {
                    format!(": {summary}")
                };
                if c {
                    println!("    {m} {C}{author}{Z}{snip}");
                } else {
                    println!("    {m} {author}{snip}");
                }
            }
        }
    }
}

/// Typed rows for `clank log --json` (typed-json-not-json-macro,
/// replacing ad-hoc `json!`). `#[serde(tag = "kind")]` emits the
/// discriminant alongside the fields, matching the prior shape's keys +
/// values (key order is irrelevant — consumers parse the JSON).
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
enum LogJsonRow<'a> {
    #[serde(rename = "intro")]
    Intro {
        plan: &'a str,
        sha: &'a str,
        ts: i64,
        subject: &'a str,
    },
    #[serde(rename = "commit")]
    Commit {
        plan: &'a str,
        sha: &'a str,
        ts: i64,
        touched_plan: bool,
        touched_code: bool,
        subject: &'a str,
    },
    #[serde(rename = "finalized")]
    Finalized {
        plan: &'a str,
        sha: &'a str,
        ts: i64,
        subject: &'a str,
    },
    #[serde(rename = "deleted")]
    Deleted {
        plan: &'a str,
        sha: &'a str,
        ts: i64,
    },
    #[serde(rename = "ad-hoc")]
    AdHoc {
        sha: &'a str,
        ts: i64,
        subject: &'a str,
    },
    #[serde(rename = "review")]
    Review {
        sha: &'a str,
        author: &'a str,
        verdict: Verdict,
    },
    #[serde(rename = "github_event")]
    Github {
        ts: i64,
        repo: &'a str,
        event: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        actor: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<&'a str>,
        seen_by: &'a [String],
        unhandled: bool,
    },
}

fn print_json(items: &[TimelineItem], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let out = json_items(items, &reviews);
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// The interleaved `--json` sequence: repo chunks through
/// [`json_rows`], github entries as typed `github_event` rows in
/// their timeline position. Pure, like `json_rows`.
fn json_items<'a>(
    items: &[TimelineItem<'a>],
    reviews: &'a std::collections::BTreeMap<String, Vec<Review>>,
) -> Vec<LogJsonRow<'a>> {
    let mut out = Vec::new();
    let mut chunk: Vec<&LogEvent> = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(e) => chunk.push(e),
            TimelineItem::Github(g) => {
                out.extend(json_rows(&chunk, reviews));
                chunk.clear();
                out.push(LogJsonRow::Github {
                    ts: i64::try_from(g.at).unwrap_or(i64::MAX),
                    repo: &g.repo,
                    event: &g.event,
                    detail: g.detail.as_deref(),
                    number: g.number,
                    title: g.title.as_deref(),
                    actor: g.actor.as_deref(),
                    url: g.url.as_deref(),
                    seen_by: &g.seen_by,
                    unhandled: g.unhandled,
                });
            }
        }
    }
    out.extend(json_rows(&chunk, reviews));
    out
}

/// The `clank log --json` row sequence — pure, so the review
/// attachment (EVERY commit kind carries its reviews, ad-hoc included
/// — adhoc-reviews-in-log) is pinned without capturing stdout.
fn json_rows<'a>(
    events: &[&'a LogEvent],
    reviews: &'a std::collections::BTreeMap<String, Vec<Review>>,
) -> Vec<LogJsonRow<'a>> {
    let mut out: Vec<LogJsonRow> = Vec::new();
    for event in events {
        out.push(match event {
            LogEvent::PlanIntro {
                plan,
                sha,
                ts,
                subject,
            } => LogJsonRow::Intro {
                plan: plan.as_str(),
                sha: sha.as_str(),
                ts: *ts,
                subject,
            },
            LogEvent::PlanCommit {
                plan,
                sha,
                ts,
                touched_plan,
                touched_code,
                subject,
            } => LogJsonRow::Commit {
                plan: plan.as_str(),
                sha: sha.as_str(),
                ts: *ts,
                touched_plan: *touched_plan,
                touched_code: *touched_code,
                subject,
            },
            LogEvent::PlanFinalized {
                plan,
                sha,
                ts,
                subject,
            } => LogJsonRow::Finalized {
                plan: plan.as_str(),
                sha: sha.as_str(),
                ts: *ts,
                subject,
            },
            LogEvent::PlanDeleted { plan, sha, ts } => LogJsonRow::Deleted {
                plan: plan.as_str(),
                sha: sha.as_str(),
                ts: *ts,
            },
            LogEvent::AdHoc { sha, ts, subject } => LogJsonRow::AdHoc {
                sha: sha.as_str(),
                ts: *ts,
                subject,
            },
        });
        let sha_str = match event {
            LogEvent::PlanIntro { sha, .. }
            | LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanFinalized { sha, .. }
            | LogEvent::PlanDeleted { sha, .. }
            | LogEvent::AdHoc { sha, .. } => sha.as_str(),
        };
        if let Some(rs) = reviews.get(sha_str) {
            for r in rs {
                out.push(LogJsonRow::Review {
                    sha: sha_str,
                    author: &r.author,
                    verdict: r.verdict,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod json_output_tests {
    use super::*;

    fn gh(at: u64, key_hint: &str, unhandled: bool) -> crate::cli::github_timeline::MergedEvent {
        crate::cli::github_timeline::MergedEvent {
            at,
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(12),
            title: Some(key_hint.into()),
            actor: Some("alice".into()),
            url: None,
            seen_by: vec!["claude".into()],
            unhandled,
        }
    }

    fn adhoc(ts: i64, subject: &str) -> LogEvent {
        use crate::lifecycle::CommitSha;
        LogEvent::AdHoc {
            sha: CommitSha::parse(&format!("{:0<40x}", ts.max(1))).unwrap(),
            ts,
            subject: subject.into(),
        }
    }

    #[test]
    fn interleave_orders_by_time_and_respects_the_range_floor() {
        // Commits at ts 100 and 200 (newest first); gh events at 250
        // (newer than newest — leads), 150 (between), and 50 (older
        // than the folded range — excluded).
        let e_new = adhoc(200, "new");
        let e_old = adhoc(100, "old");
        let events: Vec<&LogEvent> = vec![&e_new, &e_old];
        let gh_events = vec![
            gh(50, "too-old", false),
            gh(150, "between", false),
            gh(250, "lead", true),
        ];
        let items = interleave(&events, &gh_events);
        let shape: Vec<&str> = items
            .iter()
            .map(|i| match i {
                TimelineItem::Github(g) => g.title.as_deref().unwrap(),
                TimelineItem::Repo(LogEvent::AdHoc { subject, .. }) => subject.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(shape, vec!["lead", "new", "between", "old"]);
        // No commits at all → every gh event shows.
        let items = interleave(&[], &gh_events);
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn oneline_items_rows_splice_github_rows_for_the_tui() {
        let e = adhoc(100, "c");
        let events: Vec<&LogEvent> = vec![&e];
        let gh_events = vec![gh(150, "arrived", true)];
        let items = interleave(&events, &gh_events);
        let reviews = std::collections::BTreeMap::new();
        let rows = oneline_items_rows(&items, &reviews);
        assert!(
            matches!(&rows[0], OnelineRow::Github { unhandled: true, line } if line.contains("o/r#12")),
            "{rows:?}"
        );
        // The repo chunk follows (umbrella header + commit).
        assert!(rows.iter().any(|r| matches!(r, OnelineRow::Commit { .. })));
        // Plain lines render the badge + open-work mark.
        let lines = oneline_plain_lines(&rows);
        assert!(
            lines[0].starts_with("gh ") && lines[0].ends_with(" ⚠"),
            "{lines:?}"
        );
    }

    #[test]
    fn json_items_splice_typed_github_rows_in_position() {
        let e = adhoc(100, "c");
        let events: Vec<&LogEvent> = vec![&e];
        let gh_events = vec![gh(150, "arrived", true)];
        let items = interleave(&events, &gh_events);
        let reviews = std::collections::BTreeMap::new();
        let rows = json_items(&items, &reviews);
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(v[0]["kind"], "github_event");
        assert_eq!(v[0]["ts"], 150);
        assert_eq!(v[0]["repo"], "o/r");
        assert_eq!(v[0]["number"], 12);
        assert_eq!(v[0]["unhandled"], true);
        assert_eq!(v[0]["seen_by"][0], "claude");
        assert_eq!(v[1]["kind"], "ad-hoc");
    }

    #[test]
    fn json_rows_attach_reviews_to_adhoc_commits_too() {
        // adhoc-reviews-in-log: the JSON surface already attached
        // reviews to every commit kind — pinned so the three renderers
        // can't diverge again.
        use crate::lifecycle::CommitSha;
        let sha = CommitSha::parse(&format!("{:0<40}", "cc")).unwrap();
        let event = LogEvent::AdHoc {
            sha: sha.clone(),
            ts: 5,
            subject: "drive-by".into(),
        };
        let mut reviews = std::collections::BTreeMap::new();
        reviews.insert(
            sha.as_str().to_string(),
            vec![Review {
                author: "codex".into(),
                verdict: Verdict::RequestChanges,
                summary: "s".into(),
                body: String::new(),
            }],
        );
        let rows = json_rows(&[&event], &reviews);
        assert_eq!(rows.len(), 2, "the ad-hoc row plus its review row");
        assert!(matches!(rows[0], LogJsonRow::AdHoc { .. }));
        assert!(matches!(
            rows[1],
            LogJsonRow::Review {
                author: "codex",
                ..
            }
        ));
    }

    #[test]
    fn log_json_rows_serialize_to_the_stable_shape() {
        // typed-json-not-json-macro: each typed row must serialize to
        // the same keys+values the old `json!` produced — the `clank log
        // --json` wire contract. (`json!` here expresses the EXPECTED
        // value; the ban is on production output code. Key order is
        // irrelevant — `to_value` equality is order-independent.)
        let cases = [
            (
                serde_json::to_value(LogJsonRow::Intro {
                    plan: "p",
                    sha: "abc",
                    ts: 5,
                    subject: "hi",
                })
                .unwrap(),
                serde_json::json!({"kind":"intro","plan":"p","sha":"abc","ts":5,"subject":"hi"}),
            ),
            (
                serde_json::to_value(LogJsonRow::Commit {
                    plan: "p",
                    sha: "abc",
                    ts: 5,
                    touched_plan: true,
                    touched_code: false,
                    subject: "x",
                })
                .unwrap(),
                serde_json::json!({"kind":"commit","plan":"p","sha":"abc","ts":5,"touched_plan":true,"touched_code":false,"subject":"x"}),
            ),
            (
                serde_json::to_value(LogJsonRow::Finalized {
                    plan: "p",
                    sha: "abc",
                    ts: 5,
                    subject: "[p] wrap it up",
                })
                .unwrap(),
                serde_json::json!({"kind":"finalized","plan":"p","sha":"abc","ts":5,"subject":"[p] wrap it up"}),
            ),
            (
                serde_json::to_value(LogJsonRow::Deleted {
                    plan: "p",
                    sha: "abc",
                    ts: 5,
                })
                .unwrap(),
                serde_json::json!({"kind":"deleted","plan":"p","sha":"abc","ts":5}),
            ),
            (
                serde_json::to_value(LogJsonRow::AdHoc {
                    sha: "abc",
                    ts: 5,
                    subject: "x",
                })
                .unwrap(),
                serde_json::json!({"kind":"ad-hoc","sha":"abc","ts":5,"subject":"x"}),
            ),
            (
                serde_json::to_value(LogJsonRow::Review {
                    sha: "abc",
                    author: "codex",
                    verdict: Verdict::Continue,
                })
                .unwrap(),
                serde_json::json!({"kind":"review","sha":"abc","author":"codex","verdict":Verdict::Continue}),
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_core::ids::PlanKey;

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }

    #[test]
    fn oneline_rows_round_trip_shapes() {
        // Concern 2 (ruthless 54c37f6): the factoring must preserve
        // the shipped --oneline shapes. Pin the plain forms (the
        // colored path wraps the same fields in Y/C/Z, visible in
        // print_oneline).
        let events = [
            LogEvent::PlanIntro {
                plan: PlanKey::parse("foo").unwrap(),
                sha: sha("aa"),
                ts: 1,
                subject: "[foo] intro".into(),
            },
            LogEvent::PlanFinalized {
                plan: PlanKey::parse("foo").unwrap(),
                sha: sha("bb"),
                ts: 2,
                subject: "[foo] wrap up".into(),
            },
            LogEvent::AdHoc {
                sha: sha("cc"),
                ts: 3,
                subject: "drive-by".into(),
            },
        ];
        let refs: Vec<&LogEvent> = events.iter().collect();
        let mut reviews = std::collections::BTreeMap::new();
        reviews.insert(
            sha("aa").as_str().to_string(),
            vec![Review {
                author: "codex".into(),
                verdict: Verdict::Continue,
                summary: "lgtm".into(),
                body: String::new(),
            }],
        );
        // AdHoc commits render their reviews too — ad-hoc feedback is
        // a first-class review surface (review.adhoc_feedback,
        // adhoc-reviews-in-log).
        reviews.insert(
            sha("cc").as_str().to_string(),
            vec![Review {
                author: "codex".into(),
                verdict: Verdict::Continue,
                summary: "x".into(),
                body: String::new(),
            }],
        );
        let lines = oneline_plain_lines(&oneline_rows(&refs, &reviews));
        // Umbrella shape (log-plan-umbrellas + adhoc-commit-marker): one
        // plan header at col 0; each commit LEADS with its 1-col marker icon
        // (planning `✎`, finish `⚑`, ad-hoc `~`), then the sha, then the
        // subject (finish now shows its real whole-plan message). The ad-hoc
        // commit folds UNDER the plan umbrella (no separate "adhoc" header).
        assert_eq!(
            lines,
            vec![
                "foo".to_string(),
                // review renders ABOVE its commit (newest-first time
                // order — status-timeline-progress)
                "    ✓ codex: lgtm".to_string(),
                "✎ aa00000 intro".to_string(),
                "⚑ bb00000 wrap up".to_string(),
                "    ✓ codex: x".to_string(),
                "~ cc00000 drive-by".to_string(),
            ]
        );
    }

    #[test]
    fn human_review_lines_shape_for_adhoc_reviews() {
        // adhoc-reviews-in-log: the pure line builder `clank log`'s
        // AdHoc arm now shares with the plan path — verdict mark,
        // author, summary snip, indented body.
        let rs = vec![Review {
            author: "codex".into(),
            verdict: Verdict::RequestChanges,
            summary: "make it consistent".into(),
            body: "line one\nline two".into(),
        }];
        let lines = human_review_lines(&rs, false);
        assert_eq!(
            lines,
            vec![
                "    ✗ codex request_changes: make it consistent".to_string(),
                String::new(),
                "        line one".to_string(),
                "        line two".to_string(),
            ]
        );
        assert!(human_review_lines(&[], false).is_empty());
    }

    #[test]
    fn oneline_rows_marks_adhoc_and_folds_under_plan() {
        // adhoc-commit-marker: an ad-hoc commit folds under the
        // surrounding plan's umbrella (NO separate "adhoc" header) and
        // is flagged `ad_hoc`; a plan commit is not.
        let events = [
            LogEvent::PlanCommit {
                plan: PlanKey::parse("foo").unwrap(),
                sha: sha("aa"),
                ts: 1,
                touched_plan: false,
                touched_code: true,
                subject: "[foo] work".into(),
            },
            LogEvent::AdHoc {
                sha: sha("bb"),
                ts: 2,
                subject: "drive-by".into(),
            },
        ];
        let refs: Vec<&LogEvent> = events.iter().collect();
        let rows = oneline_rows(&refs, &Default::default());
        // Exactly one header (foo) — no "adhoc" header.
        let headers: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r, OnelineRow::Header { .. }))
            .collect();
        assert_eq!(headers.len(), 1, "no separate adhoc header: {rows:?}");
        assert!(matches!(headers[0], OnelineRow::Header { plan: Some(p) } if p == "foo"));
        // The plan commit (touched code) is Impl; the drive-by is AdHoc.
        let marker = |prefix: &str| {
            rows.iter().find_map(|r| match r {
                OnelineRow::Commit { sha, marker, .. } if sha.as_str().starts_with(prefix) => {
                    Some(*marker)
                }
                _ => None,
            })
        };
        assert_eq!(marker("aa"), Some(RowMarker::Impl), "code commit is Impl");
        assert_eq!(marker("bb"), Some(RowMarker::AdHoc), "drive-by is AdHoc");
    }

    #[test]
    fn oneline_marker_gutter_keeps_subjects_aligned() {
        // The leading 1-col marker icon is on EVERY commit row so subjects
        // begin at the SAME column regardless of marker. Each glyph is 1
        // char (and 1 display col), so CHAR position of the subject matches
        // across rows (byte offsets differ — the glyphs are multi-byte).
        let impl_row = OnelineRow::Commit {
            sha: sha("aa"),
            subject: "x".into(),
            marker: RowMarker::Impl,
        };
        let adhoc_row = OnelineRow::Commit {
            sha: sha("bb"),
            subject: "x".into(),
            marker: RowMarker::AdHoc,
        };
        let lines = oneline_plain_lines(&[impl_row, adhoc_row]);
        let col = |line: &str| line.chars().position(|c| c == 'x').unwrap();
        assert_eq!(
            col(&lines[0]),
            col(&lines[1]),
            "subject column aligned across markers: {lines:?}"
        );
        assert!(
            lines[0].starts_with(RowMarker::Impl.glyph()),
            "impl row leads with ⚒: {lines:?}"
        );
        assert!(
            lines[1].starts_with(RowMarker::AdHoc.glyph()),
            "adhoc row leads with ~: {lines:?}"
        );
    }

    #[test]
    fn row_marker_glyphs_are_all_single_char() {
        // Alignment invariant: every marker glyph is exactly ONE char. The
        // display-width==1 invariant (the trap ruthless flagged) is pinned in
        // the status_tui width test against `char_width`. A future emoji swap
        // (multi-scalar / width-2) fails one of the two.
        for m in [
            RowMarker::Plain,
            RowMarker::AdHoc,
            RowMarker::Planning,
            RowMarker::Impl,
            RowMarker::Finish,
        ] {
            assert_eq!(
                m.glyph().to_string().chars().count(),
                1,
                "{m:?} is one char"
            );
        }
    }

    #[test]
    fn oneline_rows_umbrella_headers_and_prefix_stripping() {
        use clank_core::repo_state::LogEvent;
        let sha = |n: u8| crate::lifecycle::CommitSha::parse(&format!("{n:0<40x}")).unwrap();
        let foo = crate::lifecycle::PlanKey::parse("foo").unwrap();
        let e1 = LogEvent::PlanCommit {
            plan: foo.clone(),
            sha: sha(1),
            ts: 1,
            touched_plan: true,
            touched_code: false,
            subject: "[foo] intro".into(),
        };
        // Multi-plan prefix carries information → stays verbatim.
        let e2 = LogEvent::PlanCommit {
            plan: foo.clone(),
            sha: sha(2),
            ts: 2,
            touched_plan: false,
            touched_code: true,
            subject: "[foo,bar] shared change".into(),
        };
        let events = [&e1, &e2];
        let rows = oneline_rows(&events, &Default::default());
        let lines = oneline_plain_lines(&rows);
        assert_eq!(lines[0], "foo", "umbrella header at col 0");
        assert!(
            lines[1].ends_with(" intro"),
            "matching prefix stripped: {lines:?}"
        );
        assert!(
            lines[2].ends_with(" [foo,bar] shared change"),
            "multi-plan prefix kept verbatim: {lines:?}"
        );
    }
}
