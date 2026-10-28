//! `clank log` — chronological timeline of commits and reviews.
//!
//! Default: last 30 commits from HEAD. `--plan` filters to one plan.
//! Explicit `<range>` overrides the default window.

use std::io::Write;
use std::path::Path;

use super::{LogArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::feedback_scan::scan_feedback;
use crate::git_io::CommitMeta;
use crate::lifecycle::{CommitSha, PlanKey};
use clank_core::repo_state::LogEvent;
use clank_core::vocab::Verdict;

pub async fn run(args: LogArgs) -> anyhow::Result<()> {
    write_log(&args, color(), &mut std::io::stdout().lock()).await
}

async fn write_log(args: &LogArgs, c: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let git = crate::git_io::open(&repo)?;
    let head = git.head_sha()?;

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

    // `None` only on an unborn branch with no range asked for: there is
    // no history to read, but the events are still a timeline.
    let span: Option<(Option<CommitSha>, CommitSha)> = match &args.range {
        Some(range) => Some(parse_range(&repo, range)?),
        None => head.map(|head| {
            let from = if args.limit > 0 {
                git_rev_parse(&repo, &format!("HEAD~{}", args.limit))
            } else {
                None
            };
            (from, head)
        }),
    };

    let state = crate::rebuild::rebuild_repo(&repo)
        .await
        .map_err(|e| anyhow::anyhow!("failed to rebuild repo: {e}"))?;
    let page = match &span {
        Some((from, to)) => read_page(&git, from.as_ref(), to, args)?,
        None => Vec::new(),
    };
    if args.diagnostics {
        let mut diagnostics = Vec::new();
        if let (Some((_, to)), Some(oldest)) = (&span, page.last()) {
            let from = git.parent_of(&oldest.sha)?;
            crate::rebuild::rebuild_from_recording_with_git(
                &repo,
                &git,
                from.as_ref(),
                to,
                &mut diagnostics,
            )?;
        }
        let records: Vec<_> = diagnostics
            .iter()
            .rev()
            .filter(|record| {
                plan_filter
                    .as_ref()
                    .is_none_or(|plan| record.plan.as_ref() == Some(plan))
            })
            .collect();
        if args.json {
            writeln!(out, "{}", serde_json::to_string_pretty(&records)?)?;
        } else if records.is_empty() {
            writeln!(out, "no history diagnostics")?;
        } else {
            for record in records {
                writeln!(out, "{}  {}", record.sha, record.warning)?;
            }
        }
        return Ok(());
    }
    let history = match &span {
        Some((_, to)) => {
            let adopted_at = state.fold.adopted_at.as_ref();
            let tip_post_adoption = match adopted_at {
                None => false,
                Some(_) if state.head.as_ref() == Some(to) => true,
                Some(adopted) => git
                    .first_parent_chain_find(to, &std::iter::once(adopted.clone()).collect())?
                    .is_some(),
            };
            history_from_page(&repo, &git, to, page, adopted_at, tip_post_adoption).await?
        }
        None => HistoryWindow::default(),
    };
    let log_events = history.folded;

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

    // Events interleave by time (log-timeline-github-events); a
    // plan-scoped view is plan work only, so the repo-wide stream
    // stays out of it entirely.
    //
    // `--no-github` takes GITHUB out, and nothing else. It used to
    // discard the whole snapshot, which was the same thing while
    // github was the only backend and silently hid every command
    // source the moment one wasn't (codex db5821e).
    let (gh_events, nudges) = if plan_filter.is_some() {
        (Vec::new(), Vec::new())
    } else {
        let snap = crate::cli::github::timeline::timeline_snapshot(&repo);
        for n in &snap.notices {
            eprintln!("log: {n}");
        }
        (
            timeline_events_for(snap.events, args.no_github),
            snap.nudges,
        )
    };

    // Most recent first (git log convention).
    let filtered: Vec<&LogEvent> = filtered.into_iter().rev().collect();
    let plain: &[PlainCommit] = if plan_filter.is_none() {
        &history.plain
    } else {
        &[]
    };
    let items = interleave_with_plain(
        &filtered,
        plain,
        &gh_events,
        &nudges,
        history.next.is_some(),
    );

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
        return print_json(out, &items, &repo, &reviewable_shas);
    }
    if items.is_empty() && span.is_some() {
        writeln!(out, "no log events")?;
    } else if args.oneline {
        print_oneline(out, &items, &repo, &reviewable_shas, c)?;
    } else {
        print_human(out, &items, &repo, &reviewable_shas, c)?;
    }
    if span.is_none() {
        let line = no_commits_line(git.current_branch()?.as_deref());
        if c {
            writeln!(out, "{DIM}{line}{Z}")?;
        } else {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

fn no_commits_line(branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("branch `{branch}` has no commits yet"),
        None => "this branch has no commits yet".to_string(),
    }
}

/// The first-parent commits in `from..to` (or `-n` back from `to`),
/// newest first.
fn read_page(
    git: &crate::git_io::Repo,
    from: Option<&CommitSha>,
    to: &CommitSha,
    args: &LogArgs,
) -> anyhow::Result<Vec<CommitMeta>> {
    Ok(match from {
        Some(base) => {
            // Range reads return oldest-first; bounded pages are already newest-first.
            let mut page = git.first_parent_commits_between(base, to)?;
            page.reverse();
            page
        }
        None if args.range.is_none() && args.limit > 0 => {
            git.first_parent_page_to(to, args.limit)?
        }
        None => {
            let mut all = git.first_parent_commits_to(to)?;
            all.reverse();
            all
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlainCommit {
    pub(crate) meta: CommitMeta,
    pub(crate) refs: Vec<String>,
}

#[derive(Default)]
pub(crate) struct HistoryWindow {
    pub(crate) folded: Vec<LogEvent>,
    pub(crate) plain: Vec<PlainCommit>,
    /// First commit of the next older page, or `None` at the root.
    pub(crate) next: Option<HistoryCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryCursor {
    pub(crate) tip: CommitSha,
    /// Whether `tip` is at or after the adoption commit. Once a page crosses
    /// the seam this stays false, avoiding an ancestry walk on every older
    /// page.
    pub(crate) post_adoption: bool,
}

/// Read one requested first-parent page and split it at the durable adoption
/// boundary. The Git page is already bounded by its caller. Only the newer
/// side enters the fold; older commits become plain rows directly.
#[cfg(test)]
pub(crate) async fn history_window(
    repo: &Path,
    head: &CommitSha,
    limit: usize,
    adopted_at: Option<&CommitSha>,
) -> anyhow::Result<HistoryWindow> {
    let cursor = HistoryCursor {
        tip: head.clone(),
        post_adoption: adopted_at.is_some(),
    };
    history_page(repo, &cursor, limit, adopted_at).await
}

pub(crate) async fn history_page(
    repo: &Path,
    cursor: &HistoryCursor,
    limit: usize,
    adopted_at: Option<&CommitSha>,
) -> anyhow::Result<HistoryWindow> {
    let git = crate::git_io::open(repo)?;
    let page = git.first_parent_page_to(&cursor.tip, limit)?;
    history_from_page(
        repo,
        &git,
        &cursor.tip,
        page,
        adopted_at,
        cursor.post_adoption,
    )
    .await
}

async fn history_from_page(
    repo: &Path,
    git: &crate::git_io::Repo,
    tip: &CommitSha,
    page: Vec<CommitMeta>,
    adopted_at: Option<&CommitSha>,
    tip_post_adoption: bool,
) -> anyhow::Result<HistoryWindow> {
    if page.is_empty() {
        return Ok(HistoryWindow::default());
    }

    let next_tip = git.parent_of(&page.last().expect("non-empty page").sha)?;

    let (folded_page, plain_page, next_post_adoption) = match adopted_at {
        None => (Vec::new(), page, false),
        Some(adopted) if tip_post_adoption => {
            if let Some(seam) = page.iter().position(|commit| &commit.sha == adopted) {
                let mut folded = page;
                let plain = folded.split_off(seam + 1);
                (folded, plain, false)
            } else {
                // `tip` is post-adoption and the boundary is older than this
                // page, so every requested commit belongs to the fold side.
                (page, Vec::new(), true)
            }
        }
        Some(_) => (Vec::new(), page, false),
    };

    let folded = match (adopted_at, folded_page.last()) {
        (Some(adopted), Some(oldest)) => {
            let from = git.parent_of(&oldest.sha)?;
            let (_, events) = crate::rebuild::rebuild_from_adoption_with_git(
                repo,
                git,
                from.as_ref(),
                tip,
                adopted,
            )?;
            events
        }
        _ => Vec::new(),
    };

    let mut refs_by_sha: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, sha) in git.all_ref_tips() {
        refs_by_sha
            .entry(sha.as_str().to_string())
            .or_default()
            .push(name);
    }
    let plain = plain_page
        .into_iter()
        .map(|meta| PlainCommit {
            refs: refs_by_sha.remove(meta.sha.as_str()).unwrap_or_default(),
            meta,
        })
        .collect();
    Ok(HistoryWindow {
        folded,
        plain,
        next: next_tip.map(|tip| HistoryCursor {
            tip,
            post_adoption: next_post_adoption,
        }),
    })
}

/// One interleaved timeline entry: a fold event or a merged github
/// event (log-timeline-github-events).
pub(crate) enum TimelineItem<'a> {
    Repo(&'a LogEvent),
    Plain(&'a PlainCommit),
    Github(usize, &'a crate::cli::github::timeline::MergedEvent),
    Nudge(&'a crate::cli::github::timeline::Nudge),
}

/// Two-pointer newest-first merge preserving each list's own order.
/// Github events OLDER than the oldest displayed commit fall outside
/// the folded range and are excluded (when any commits are shown at
/// all and older history exists); newer-than-newest ones lead the view — a just-arrived event
/// is exactly what the reader wants on top. `gh` arrives
/// oldest-first (the snapshot's order).
#[cfg(test)]
pub(crate) fn interleave<'a>(
    events: &[&'a LogEvent],
    gh: &'a [crate::cli::github::timeline::MergedEvent],
) -> Vec<TimelineItem<'a>> {
    interleave_with_plain(events, &[], gh, &[], true)
}

/// [`interleave`] with the plain commits and each agent's nudge history,
/// which sits among the events by time under the same floor.
///
/// `older_history`: commits older than those shown exist. Without them
/// the window reaches the root, so an event from before the first
/// commit has no older page to belong to and stays in this one.
pub(crate) fn interleave_with_plain<'a>(
    events: &[&'a LogEvent],
    plain: &'a [PlainCommit],
    gh: &'a [crate::cli::github::timeline::MergedEvent],
    nudges: &'a [crate::cli::github::timeline::Nudge],
    older_history: bool,
) -> Vec<TimelineItem<'a>> {
    let floor = plain
        .last()
        .map(|p| p.meta.author_ts)
        .or_else(|| events.last().map(|e| e.ts()))
        .filter(|_| older_history);
    // The carried index is the ORIGINAL snapshot position — captured
    // by enumerate BEFORE the floor filter and the reversal, so the
    // displayed row and the event it opens share identity (codex
    // d7f4d40: a derived-list position inverted the mapping).
    let after_floor = |t: i64| floor.is_none_or(|f| t >= f);
    let at = |t: u64| i64::try_from(t).unwrap_or(i64::MAX);
    let mut external: Vec<(i64, TimelineItem<'a>)> = gh
        .iter()
        .enumerate()
        .map(|(i, g)| (at(g.at), TimelineItem::Github(i, g)))
        .chain(
            nudges
                .iter()
                .map(|n| (at(n.record.at), TimelineItem::Nudge(n))),
        )
        .filter(|(t, _)| after_floor(*t))
        .collect();
    // Newest first; a stable sort keeps each list's own order on a tie.
    external.reverse();
    external.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut history: Vec<TimelineItem<'a>> = events
        .iter()
        .map(|event| TimelineItem::Repo(event))
        .collect();
    history.extend(plain.iter().map(TimelineItem::Plain));
    let history_ts = |item: &TimelineItem<'_>| match item {
        TimelineItem::Repo(event) => event.ts(),
        TimelineItem::Plain(commit) => commit.meta.author_ts,
        TimelineItem::Github(_, event) => at(event.at),
        TimelineItem::Nudge(n) => at(n.record.at),
    };
    let mut out = Vec::with_capacity(history.len() + external.len());
    let mut history = history.into_iter().peekable();
    let mut external = external.into_iter().peekable();
    loop {
        let take_external = match (history.peek(), external.peek()) {
            (Some(item), Some((t, _))) => *t >= history_ts(item),
            (None, Some(_)) => true,
            (_, None) => false,
        };
        if take_external {
            out.push(external.next().expect("peeked").1);
        } else if let Some(item) = history.next() {
            out.push(item);
        } else {
            break;
        }
    }
    out
}

/// The colored `gh` row for one merged event: the WHOLE row dims for
/// pre-watch (baseline) history — it's context, not team activity —
/// while live rows get the cyan badge; no-color output keeps the
/// textual markers (codex c917ba7).
#[cfg(test)]
fn gh_colored_row(g: &crate::cli::github::timeline::MergedEvent, mark: &str) -> String {
    let line = event_row_text(g);
    let badge = event_badge(g);
    colored_event_row(badge, &line, g.baseline, mark)
}

fn colored_event_row(badge: &str, line: &str, baseline: bool, mark: &str) -> String {
    if baseline {
        format!("{DIM}{badge} {line}{mark}{Z}")
    } else {
        format!("{C}{badge}{Z} {line}{mark}")
    }
}

/// Which of the timeline's events this invocation shows.
///
/// `--no-github` takes GITHUB out, and nothing else. It used to
/// discard the whole snapshot, which was the same thing while github
/// was the only backend and silently hid every command source the
/// moment one wasn't (codex db5821e). Named rather than inlined so
/// the rule is testable where it is written.
pub(crate) fn timeline_events_for(
    events: Vec<crate::cli::github::timeline::MergedEvent>,
    no_github: bool,
) -> Vec<crate::cli::github::timeline::MergedEvent> {
    events
        .into_iter()
        .filter(|e| !no_github || e.backend != crate::cli::github::timeline::Backend::Github)
        .collect()
}

/// The gutter badge for one event: its source's configured glyph, or
/// the backend's default.
///
/// ONE answer, used by every printer. Hardcoding `gh` labelled every
/// command source's events as github (codex 80e1fb9).
pub(crate) fn event_badge(g: &crate::cli::github::timeline::MergedEvent) -> &str {
    badge_of(g.glyph.as_deref(), g.backend)
}

/// The source's configured glyph wins; the default shows the backend.
///
/// Interpret the icon at the display boundary too, so rows assembled
/// without a timeline snapshot still cannot emit controls or empty badges.
pub(crate) fn badge_of(
    glyph: Option<&str>,
    backend: crate::cli::github::timeline::Backend,
) -> &str {
    glyph
        .and_then(crate::cli::command_source::display_glyph)
        .unwrap_or(default_badge(backend))
}

/// The icon for a source that has none configured: a symbol for what
/// kind of event it is, not a name the row would then repeat.
pub(crate) fn default_badge(backend: crate::cli::github::timeline::Backend) -> &'static str {
    match backend {
        crate::cli::github::timeline::Backend::Github => "🐙",
        crate::cli::github::timeline::Backend::Command => "⚡",
        crate::cli::github::timeline::Backend::Timer => "⏰",
        crate::cli::github::timeline::Backend::Message => "✉",
    }
}

/// The row text every timeline surface shows — `clank log`, the TUI and
/// the remote page — after the state mark and the icon.
///
/// The icon says what kind of event it is, so the row says who it is
/// for and what it says. A source or kind name appears only where it
/// tells the reader something the icon cannot: the source when it has
/// no icon of its own, a kind that differs from its source, and
/// github's repo, number and action. Who sent it and where it came
/// from stay in the details and the structured output.
///
/// Before and after, for Life's sources:
///
/// ```text
/// ? 💬 signal  signal  “can you book the dentist”   ->  ? 💬 @claude  “can you book the dentist”
/// ✓ 🎙 voice  voice  “remind me at 5”                ->  ✓ 🎙 @assistent,claude  “remind me at 5”
/// → ✉ message  codex  “done”  sent → codex (life)    ->  → ✉ → @codex (~/src/life)  “done”
/// ```
pub(crate) fn event_row_text(g: &crate::cli::github::timeline::MergedEvent) -> String {
    use crate::cli::github::timeline::Backend;
    // Bodies and source names can contain lossless operator/producer text.
    // Every field entering a terminal row uses the existing single-line
    // projection; stored content and the full detail page stay unchanged.
    let clean = |text: &str| crate::cli::status_tui::text::one_line(text, usize::MAX);
    let mut out = match (&g.backend, &g.sender) {
        (Backend::Message, Some(sender)) => g.route(sender),
        _ => g.destination().to_string(),
    };
    let iconless = g
        .glyph
        .as_deref()
        .and_then(crate::cli::command_source::display_glyph)
        .is_none();
    let context = match g.backend {
        Backend::Github => {
            let mut c = clean(&g.source);
            if let Some(n) = g.number {
                c.push_str(&format!("#{n}"));
            }
            c.push(' ');
            c.push_str(&clean(&g.kind));
            if let Some(d) = &g.detail {
                c.push_str(&format!("/{}", clean(d)));
            }
            Some(c)
        }
        Backend::Message => None,
        // A broken source is named whatever its icon: which one to fix
        // is the whole point of the row, and its kind is clank's own.
        Backend::Command | Backend::Timer if g.failure => Some(clean(&g.source)),
        Backend::Command | Backend::Timer => {
            let named = iconless.then(|| clean(&g.source));
            // A timer's kind is only ever its backend's name.
            let kind = (g.backend == Backend::Command && g.kind != g.source && !g.kind.is_empty())
                .then(|| clean(&g.kind));
            match (named, kind) {
                (Some(source), Some(kind)) => Some(format!("{source}/{kind}")),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    };
    if let Some(c) = context {
        out.push_str(&format!("  {c}"));
    }
    if let Some(r) = &g.recurrence {
        out.push_str(&format!(
            "  ({})",
            crate::cli::events::recurrence_summary(r)
        ));
    }
    if let Some(t) = &g.content {
        out.push_str(&format!("  “{}”", clean(t)));
    }
    if g.failure {
        out.push_str("  (this SOURCE is broken, not the world)");
    }
    if let Some(mark) = reply_mark(g) {
        out.push_str(&format!("  {mark}"));
    }
    out
}

/// How an event shows the replies sent to its backend: `↩` and how many
/// its logs retain, `+` when older ones were pruned, and how many of
/// them are still pending.
pub(crate) fn reply_mark(g: &crate::cli::github::timeline::MergedEvent) -> Option<String> {
    if g.replies.is_empty() && !g.reply_history_pruned {
        return None;
    }
    let pruned = if g.reply_history_pruned { "+" } else { "" };
    let pending = g.replies.iter().filter(|r| !r.accepted).count();
    let pending = match pending {
        0 => String::new(),
        n => format!(" ({n} pending)"),
    };
    Some(format!("↩{}{pruned}{pending}", g.replies.len()))
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
    /// When the review was written — its file's mtime, the only
    /// record a review has of its own time. `None` when unreadable.
    pub(crate) at: Option<i64>,
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
                        at: crate::age::written_at(&repo.join(&entry.source_path)),
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
/// Dim — pre-watch (baseline) github rows render entirely faint.
const DIM: &str = "\x1b[2m";
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

fn print_human(
    out: &mut dyn Write,
    items: &[TimelineItem],
    repo: &Path,
    shas: &[CommitSha],
    c: bool,
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let mut started = false;
    let mut chunk: Vec<&LogEvent> = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(e) => chunk.push(e),
            TimelineItem::Plain(commit) => {
                print_human_events(out, &chunk, repo, &reviews, c, &mut started)?;
                chunk.clear();
                if started {
                    writeln!(out)?;
                }
                started = true;
                let (author, date, body) = commit_info(repo, &commit.meta.sha);
                if c {
                    writeln!(out, "{Y}commit {}{Z}", commit.meta.sha.as_str())?;
                } else {
                    writeln!(out, "commit {}", commit.meta.sha.as_str())?;
                }
                writeln!(out, "Author: {author}")?;
                writeln!(out, "Date:   {date}")?;
                if !commit.refs.is_empty() {
                    writeln!(out, "Refs:   {}", commit.refs.join(", "))?;
                }
                writeln!(out)?;
                for line in body.lines() {
                    writeln!(out, "    {line}")?;
                }
            }
            TimelineItem::Nudge(n) => {
                print_human_events(out, &chunk, repo, &reviews, c, &mut started)?;
                chunk.clear();
                if started {
                    writeln!(out)?;
                }
                started = true;
                let line = format!("{NUDGE_GLYPH} @{} {}", n.agent, n.record.summary());
                if c {
                    writeln!(out, "{C}{line}{Z}")?;
                } else {
                    writeln!(out, "{line}")?;
                }
                for text in n.record.text.lines() {
                    writeln!(out, "    {text}")?;
                }
            }
            TimelineItem::Github(_, g) => {
                print_human_events(out, &chunk, repo, &reviews, c, &mut started)?;
                chunk.clear();
                if started {
                    writeln!(out)?;
                }
                started = true;
                let mark = if g.baseline { "  (pre-watch)" } else { "" };
                let badge = format!("{} {}", g.delivery().glyph(), event_badge(g));
                let line = event_row_text(g);
                if c {
                    writeln!(
                        out,
                        "{}",
                        colored_event_row(&badge, &line, g.baseline, mark)
                    )?;
                } else {
                    writeln!(out, "{badge} {line}{mark}")?;
                }

                // The row says who it is for; this is where it came from.
                let clean = |text: &str| crate::cli::status_tui::text::one_line(text, usize::MAX);
                writeln!(out, "From: {}", clean(&g.source))?;
                if let Some(actor) = &g.actor {
                    writeln!(out, "By:   {}", clean(actor))?;
                }
                if let Some(url) = &g.url {
                    writeln!(out, "{url}")?;
                }
            }
        }
    }
    print_human_events(out, &chunk, repo, &reviews, c, &mut started)
}

fn print_human_events(
    out: &mut dyn Write,
    events: &[&LogEvent],
    repo: &Path,
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    c: bool,
    started: &mut bool,
) -> anyhow::Result<()> {
    for event in events.iter() {
        if *started {
            writeln!(out)?;
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
                    writeln!(
                        out,
                        "{Y}finalize {}{Z} {C}(plan: {}){Z}",
                        short(sha),
                        plan.as_str()
                    )?;
                } else {
                    writeln!(out, "finalize {} (plan: {})", short(sha), plan.as_str())?;
                }
                continue;
            }
            LogEvent::PlanDeleted { plan, sha, .. } => {
                writeln!(out, "delete {} (plan: {})", short(sha), plan.as_str())?;
                continue;
            }
            LogEvent::AdHoc { sha, .. } => {
                let (author, date, body) = commit_info(repo, sha);
                if c {
                    writeln!(out, "{Y}commit {}{Z}", sha.as_str())?;
                } else {
                    writeln!(out, "commit {}", sha.as_str())?;
                }
                writeln!(out, "Author: {author}")?;
                writeln!(out, "Date:   {date}")?;
                writeln!(out)?;
                for line in body.lines() {
                    writeln!(out, "    {line}")?;
                }
                // Ad-hoc commits carry reviews too
                // (review.adhoc_feedback — adhoc-reviews-in-log).
                print_reviews(out, reviews, sha.as_str(), c)?;
                continue;
            }
        };
        let (author, date, body) = commit_info(repo, sha);
        if c {
            writeln!(
                out,
                "{Y}commit {}{Z} {C}(plan: {}, {kind}){Z}",
                sha.as_str(),
                plan.as_str()
            )?;
        } else {
            writeln!(
                out,
                "commit {} (plan: {}, {kind})",
                sha.as_str(),
                plan.as_str()
            )?;
        }
        writeln!(out, "Author: {author}")?;
        writeln!(out, "Date:   {date}")?;
        writeln!(out)?;
        for line in body.lines() {
            writeln!(out, "    {line}")?;
        }
        print_reviews(out, reviews, sha.as_str(), c)?;
    }
    Ok(())
}

fn print_reviews(
    out: &mut dyn Write,
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    sha: &str,
    c: bool,
) -> std::io::Result<()> {
    if let Some(rs) = reviews.get(sha) {
        for line in human_review_lines(rs, c) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
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
    /// Footer naming the plan run above it. Ad-hoc rows stay inline.
    Header {
        plan: Option<String>,
        duration_secs: Option<u64>,
        /// This page reaches the real introduction. Otherwise its footer is
        /// provisional context and can move down when an older page joins.
        starts_here: bool,
    },
    Commit {
        sha: CommitSha,
        /// The commit's author time (epoch seconds) — what the TUI's
        /// age column reads.
        at: i64,
        /// Subject with the `[plan]` prefix STRIPPED when it
        /// matches the enclosing umbrella's plan (redundant under
        /// the header); verbatim otherwise.
        subject: String,
        /// Gutter marker: `AdHoc` (no `[plan]` tag, folded into the
        /// surrounding umbrella — `~`), `Finish` (the plan's finalize
        /// commit — `⚑`), or `Plain`.
        marker: RowMarker,
        /// The plan this row also STANDS FOR, when that plan is
        /// finished and this is its only commit. The umbrella exists
        /// to group several commits; with one there is nothing to
        /// group, so the header is suppressed and the name is drawn
        /// here instead of the subject.
        ///
        /// Still a `Commit`, deliberately: reviews bind to their sha
        /// by scanning forward for the next one, and the ledger
        /// attributes pages the same way in reverse. A separate row
        /// kind would orphan every review above it (codex on d420716).
        stands_for: Option<String>,
    },
    /// A pre-adoption Git commit. It is intentionally a distinct row kind:
    /// no plan umbrella, marker, reviews, or fold attribution can attach to
    /// it accidentally.
    PlainCommit {
        sha: CommitSha,
        at: i64,
        subject: String,
        refs: Vec<String>,
    },
    Review {
        verdict: Verdict,
        author: String,
        summary: String,
        /// See [`Review::at`].
        at: Option<i64>,
    },
    /// A merged github event interleaved into the timeline
    /// (log-timeline-github-events); `line` is [`event_row_text`]'s
    /// shape, shared with the CLI renderers. `baseline` = pre-watch
    /// history (rendered dimmed / marked, never open).
    Github {
        /// Index into the SAME timeline read's merged-event list
        /// (`StatusSnapshot.github_events` in the TUI) — Enter's
        /// page target. Row and list are always set together.
        event_idx: usize,
        delivery: crate::cli::github::timeline::DeliveryState,
        line: String,
        unhandled: bool,
        baseline: bool,
        /// The event's display time (epoch seconds).
        at: u64,
        /// Which backend produced it — the gutter badge's default.
        /// Hardcoding `gh` labelled every command source's events as
        /// github (codex 80e1fb9).
        backend: crate::cli::github::timeline::Backend,
        /// The source's configured glyph, which the badge prefers.
        glyph: Option<String>,
    },
    /// A timeline-read notice (corrupt/foreign logs) surfaced as a
    /// dim row — the TUI's rendering of the snapshot's notices.
    Notice(String),
    /// A textless rule above a finished run, where something other than
    /// the next run follows it: what is above it is outside the run.
    /// Drawn, never selected.
    RunEnd,
    /// An attempt to push work into an agent's terminal: history, never
    /// work, so it carries no delivery state.
    Nudge {
        agent: String,
        at: u64,
        attempt: String,
        summary: String,
        text: String,
    },
}

/// The mark of a nudge row: an attempt to push work into a terminal.
pub(crate) const NUDGE_GLYPH: &str = "↻";

/// The CLI's rendering of [`OnelineRow::RunEnd`].
pub(crate) const RUN_END_RULE: &str = "────────────────────────";

/// Insert a [`OnelineRow::RunEnd`] above each finished run, from row
/// `from` on, decided by the rows as finally interleaved. A finish's own
/// reviews sit above it and stay inside the run; a banner directly above
/// them is the next run's and already separates the two; at the top of
/// the rows nothing follows yet. Rerunning it adds nothing, so a joined
/// page can be passed over again from its seam.
pub(crate) fn mark_run_ends(rows: &mut Vec<OnelineRow>, from: usize) {
    let mut i = from;
    while i < rows.len() {
        let finish = matches!(
            rows[i],
            OnelineRow::Commit {
                marker: RowMarker::Finish,
                ..
            }
        );
        if finish {
            let mut above = i;
            while above > 0 && matches!(rows[above - 1], OnelineRow::Review { .. }) {
                above -= 1;
            }
            let ends = above > 0
                && !matches!(
                    rows[above - 1],
                    OnelineRow::Header { .. } | OnelineRow::RunEnd
                );
            if ends {
                rows.insert(above, OnelineRow::RunEnd);
                i += 1;
            }
        }
        i += 1;
    }
}

/// Finalize SHAs that stand for their whole plan, mapped to its name.
///
/// Keyed by SHA, never by name: `FinishedPlan` is an INSTANCE record
/// and a plan name can recur, so a later plan called `foo` must not
/// inherit an earlier `foo`'s eligibility (codex on 5b1e87c).
pub(crate) type StandsFor = std::collections::BTreeMap<String, String>;

/// Group the bounded Git window once and retain external event positions.
pub(crate) fn oneline_items_rows(
    items: &[TimelineItem],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    stands_for: &StandsFor,
) -> Vec<OnelineRow> {
    // External rows preserve timeline order but do not end a Git plan run.
    // Group the bounded Git window once, then splice those rows by event.
    let events: Vec<&LogEvent> = items
        .iter()
        .filter_map(|item| match item {
            TimelineItem::Repo(event) => Some(*event),
            _ => None,
        })
        .collect();
    let mut git_rows = oneline_rows(&events, reviews, stands_for)
        .into_iter()
        .peekable();
    let mut out = Vec::new();
    for item in items {
        match item {
            TimelineItem::Repo(event) => {
                for row in git_rows.by_ref() {
                    let commit =
                        matches!(&row, OnelineRow::Commit { sha, .. } if sha == event.sha());
                    out.push(row);
                    if commit {
                        break;
                    }
                }
                // A footer follows only the oldest Git event of its run.
                while matches!(git_rows.peek(), Some(OnelineRow::Header { .. })) {
                    out.push(git_rows.next().unwrap());
                }
            }
            TimelineItem::Plain(commit) => out.push(OnelineRow::PlainCommit {
                sha: commit.meta.sha.clone(),
                at: commit.meta.author_ts,
                subject: commit.meta.subject.clone(),
                refs: commit.refs.clone(),
            }),
            TimelineItem::Nudge(n) => out.push(OnelineRow::Nudge {
                agent: n.agent.clone(),
                at: n.record.at,
                attempt: n.record.attempt.clone(),
                summary: n.record.summary(),
                text: n.record.text.clone(),
            }),
            TimelineItem::Github(idx, g) => out.push(OnelineRow::Github {
                delivery: g.delivery(),
                event_idx: *idx,
                line: event_row_text(g),
                unhandled: g.unhandled,
                baseline: g.baseline,
                at: g.at,
                backend: g.backend,
                glyph: g.glyph.clone(),
            }),
        }
    }
    mark_run_ends(&mut out, 0);
    out
}

/// Join one older bounded page. A provisional footer moves to the actual
/// introduction without duplicating the plan name or losing its duration.
/// Inspect only the page being appended, never rebuild or scan loaded history.
pub(crate) fn extend_oneline_rows(rows: &mut Vec<OnelineRow>, mut older: Vec<OnelineRow>) {
    if let Some(OnelineRow::Header {
        plan: Some(plan),
        duration_secs,
        starts_here: false,
    }) = rows.last()
    {
        let first_header = older
            .iter()
            .position(|r| matches!(r, OnelineRow::Header { .. }));
        if let Some(i) = first_header {
            if let OnelineRow::Header {
                plan: Some(next),
                duration_secs: next_duration,
                ..
            } = &mut older[i]
                && next == plan
            {
                if next_duration.is_none() {
                    *next_duration = *duration_secs;
                }
                rows.pop();
            }
        } else if !older.iter().any(|r| {
            matches!(
                r,
                OnelineRow::PlainCommit { .. }
                    | OnelineRow::Commit {
                        stands_for: Some(_),
                        ..
                    }
            )
        }) {
            let footer = rows.pop().unwrap();
            let seam = rows.len();
            rows.append(&mut older);
            rows.push(footer);
            mark_run_ends(rows, seam);
            return;
        }
    }
    let seam = rows.len();
    rows.append(&mut older);
    mark_run_ends(rows, seam);
}

pub(crate) fn plan_footer_text(plan: Option<&str>, duration_secs: Option<u64>) -> String {
    let name = plan.unwrap_or("adhoc");
    match duration_secs {
        Some(secs) => format!("{name} · {}", crate::age::duration(secs)),
        None => name.to_owned(),
    }
}

/// Pure row producer for the oneline view. Subjects come from the
/// fold's `LogEvent`s (carried since status-tui-live-log) — no
/// per-event `git log -1` shelling, which matters for the live TUI
/// pane re-rendering every refresh.
///
/// `events` are NEWEST-FIRST (the git-log convention every caller —
/// `print_oneline`, the status TUI's paged log reader — uses); that
/// order is what `umbrella_sections` folds ad-hoc commits by.
pub(crate) fn oneline_rows(
    events: &[&LogEvent],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
    stands_for: &StandsFor,
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
        // A finished plan of ONE commit needs no umbrella: there is
        // nothing to group, and the row below carries the name. Asked
        // of the run's own commits, so a chunk holding only part of a
        // plan cannot answer for the whole of it.
        let collapses = |plan: &str| {
            run.iter()
                .any(|e| stands_for.get(e.sha().as_str()).is_some_and(|p| p == plan))
        };
        // Decided from the run, emitted after it.
        let footer = match umbrella_plan.as_deref() {
            Some(plan) if !collapses(plan) => Some(plan.to_string()),
            _ => None,
        };
        let run_duration = run.iter().find_map(|e| match e {
            LogEvent::PlanFinalized { duration_secs, .. } => Some(*duration_secs),
            _ => None,
        });
        let starts_here = run.iter().any(|e| matches!(e, LogEvent::PlanIntro { .. }));
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
            // or invalid prefixes still carry information and
            // stay verbatim.
            let parsed = parse_subject(&subject);
            let strip = matches!(
                (&umbrella_plan, &parsed.prefix),
                (Some(up), Some(clank_core::repo_state::TitlePrefix::Plan(name)))
                    if name == up
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
                        at: r.at,
                    });
                }
            }
            out.push(OnelineRow::Commit {
                sha: sha.clone(),
                at: event.ts(),
                subject,
                marker: RowMarker::of(event),
                stands_for: stands_for.get(sha.as_str()).cloned(),
            });
        }
        // The name goes UNDER its run, because the list is newest-first
        // and a plan's beginning is at its bottom: printed first, the
        // name sits beside the finish and leaves the intro that opened
        // the plan orphaned at the far end. Same clock as a review,
        // which renders above the commit it followed.
        //
        // A truncated page gets a provisional footer. Appending an older
        // page moves it to the introduction via `extend_oneline_rows`,
        // retaining the recorded duration and one name for that run.
        if let Some(plan) = footer {
            let duration_secs = run_duration;
            out.push(OnelineRow::Header {
                plan: Some(plan),
                duration_secs,
                starts_here,
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
            OnelineRow::Header {
                plan,
                duration_secs,
                ..
            } => plan_footer_text(plan.as_deref(), *duration_secs),
            OnelineRow::Github {
                line,
                delivery,
                baseline,
                backend,
                glyph,
                ..
            } => {
                let mark = if *baseline { " (pre-watch)" } else { "" };
                format!(
                    "{} {} {line}{mark}",
                    delivery.glyph(),
                    badge_of(glyph.as_deref(), *backend)
                )
            }
            OnelineRow::Notice(n) => format!("({n})"),
            OnelineRow::Nudge { agent, summary, .. } => {
                format!("{NUDGE_GLYPH} @{agent} {summary}")
            }
            OnelineRow::RunEnd => RUN_END_RULE.to_string(),
            OnelineRow::PlainCommit {
                sha, subject, refs, ..
            } => {
                let refs = if refs.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", refs.join(", "))
                };
                format!("  {} {subject}{refs}", short(sha))
            }
            // The 1-col marker icon LEADS every commit row (finish/impl/
            // planning/adhoc), then the sha, then the subject. Fixed width
            // keeps subjects column-aligned.
            OnelineRow::Commit {
                sha,
                subject,
                marker,
                ..
            } => {
                format!("{} {} {subject}", marker.glyph(), short(sha))
            }
            OnelineRow::Review {
                verdict,
                author,
                summary,
                ..
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

fn print_oneline(
    out: &mut dyn Write,
    items: &[TimelineItem],
    repo: &Path,
    shas: &[CommitSha],
    c: bool,
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    print_oneline_rows(
        out,
        oneline_items_rows(items, &reviews, &Default::default()),
        c,
    )?;
    Ok(())
}

fn print_oneline_rows(out: &mut dyn Write, rows: Vec<OnelineRow>, c: bool) -> std::io::Result<()> {
    for row in rows {
        match row {
            OnelineRow::Notice(message) => writeln!(out, "{message}")?,
            OnelineRow::Nudge { agent, summary, .. } => {
                if c {
                    writeln!(out, "{C}{NUDGE_GLYPH} @{agent}{Z} {summary}")?;
                } else {
                    writeln!(out, "{NUDGE_GLYPH} @{agent} {summary}")?;
                }
            }
            OnelineRow::RunEnd => {
                if c {
                    writeln!(out, "{DIM}{RUN_END_RULE}{Z}")?;
                } else {
                    writeln!(out, "{RUN_END_RULE}")?;
                }
            }
            OnelineRow::PlainCommit {
                sha, subject, refs, ..
            } => {
                let refs = if refs.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", refs.join(", "))
                };
                if c {
                    writeln!(out, "  {Y}{}{Z} {subject}{refs}", short(&sha))?;
                } else {
                    writeln!(out, "  {} {subject}{refs}", short(&sha))?;
                }
            }
            OnelineRow::Github {
                line,
                delivery,
                unhandled: _,
                baseline,
                glyph,
                backend,
                ..
            } => {
                let badge = badge_of(glyph.as_deref(), backend);
                let mark = if baseline { " (pre-watch)" } else { "" };
                let badge = format!("{} {badge}", delivery.glyph());
                if c {
                    writeln!(out, "{}", colored_event_row(&badge, &line, baseline, mark))?;
                } else {
                    writeln!(out, "{badge} {line}{mark}")?;
                }
            }
            OnelineRow::Header {
                plan,
                duration_secs,
                ..
            } => {
                let name = plan_footer_text(plan.as_deref(), duration_secs);
                if c {
                    writeln!(out, "{C}{name}{Z}")?;
                } else {
                    writeln!(out, "{name}")?;
                }
            }
            OnelineRow::Commit {
                sha,
                subject,
                marker,
                ..
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
                    writeln!(out, "{icon} {Y}{}{Z} {subject}", short(&sha))?;
                } else {
                    writeln!(out, "{g} {} {subject}", short(&sha))?;
                }
            }
            OnelineRow::Review {
                verdict,
                author,
                summary,
                ..
            } => {
                let m = verdict_mark(verdict, c);
                let snip = if summary.is_empty() {
                    String::new()
                } else {
                    format!(": {summary}")
                };
                if c {
                    writeln!(out, "    {m} {C}{author}{Z}{snip}")?;
                } else {
                    writeln!(out, "    {m} {author}{snip}")?;
                }
            }
        }
    }
    Ok(())
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
        duration_secs: u64,
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
    #[serde(rename = "git-commit")]
    Plain {
        sha: &'a str,
        ts: i64,
        subject: &'a str,
        refs: &'a [String],
    },
    #[serde(rename = "nudge")]
    Nudge {
        agent: &'a str,
        ts: i64,
        attempt: &'a str,
        trigger: &'a crate::cli::nudge_log::Trigger,
        outcome: &'a crate::cli::nudge_log::Outcome,
        text: &'a str,
    },
    #[serde(rename = "review")]
    Review {
        sha: &'a str,
        author: &'a str,
        verdict: Verdict,
    },
    #[serde(rename = "github_event")]
    Github {
        members: &'a [crate::cli::github::timeline::MemberRef],
        delivery: crate::cli::github::timeline::DeliveryState,
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
        baseline: bool,
    },
    /// A command source's event. Its OWN shape, because the two
    /// genuinely differ: there is no repo, number, actor or url, and
    /// there is a `failure` flag that github has no analogue for.
    /// Emitting these as `github_event` with the source name in
    /// `repo` and the content in `title` was a lie the consumer had
    /// no way to detect (codex 80e1fb9).
    #[serde(rename = "command_event")]
    Command {
        members: &'a [crate::cli::github::timeline::MemberRef],
        delivery: crate::cli::github::timeline::DeliveryState,
        ts: i64,
        source: &'a str,
        /// The sub-kind, named `event` like github's because the DTO
        /// is tagged on `kind`.
        event: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        /// The SOURCE is broken, rather than something it observed.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        failure: bool,
        seen_by: &'a [String],
        unhandled: bool,
        baseline: bool,
        /// Replies sent to its backend that its logs retain.
        #[serde(skip_serializing_if = "<[_]>::is_empty")]
        replies: &'a [crate::cli::github::timeline::BackendReply],
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        reply_history_pruned: bool,
        /// For a reply whose event has aged out: its operation, and the
        /// copy it answered.
        #[serde(skip_serializing_if = "<[_]>::is_empty")]
        outgoing: &'a [crate::cli::github::timeline::OutgoingReply],
    },
    #[serde(rename = "timer_event")]
    Timer {
        members: &'a [crate::cli::github::timeline::MemberRef],
        delivery: crate::cli::github::timeline::DeliveryState,
        ts: i64,
        due_at: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        recurrence: Option<&'a clank_core::agent_config::TimerRecurrence>,
        source: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<&'a str>,
        seen_by: &'a [String],
        unhandled: bool,
    },
    /// A message one agent sent another. Its own row rather than a
    /// command's: `source` here is the SENDER, which is a different
    /// fact from the name of a producer that ran.
    Message {
        delivery: crate::cli::github::timeline::DeliveryState,
        ts: i64,
        /// Who sent it.
        from: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        outgoing: &'a Vec<crate::cli::github::timeline::OutgoingReply>,
        members: &'a [crate::cli::github::timeline::MemberRef],
        outgoing_only: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<&'a str>,
        seen_by: &'a [String],
        unhandled: bool,
        baseline: bool,
    },
}

fn print_json(
    out: &mut dyn Write,
    items: &[TimelineItem],
    repo: &Path,
    shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let rows = json_items(items, &reviews);
    writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
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
            TimelineItem::Nudge(n) => {
                out.extend(json_rows(&chunk, reviews));
                chunk.clear();
                out.push(LogJsonRow::Nudge {
                    agent: &n.agent,
                    ts: i64::try_from(n.record.at).unwrap_or(i64::MAX),
                    attempt: &n.record.attempt,
                    trigger: &n.record.trigger,
                    outcome: &n.record.outcome,
                    text: &n.record.text,
                });
            }
            TimelineItem::Plain(commit) => {
                out.extend(json_rows(&chunk, reviews));
                chunk.clear();
                out.push(LogJsonRow::Plain {
                    sha: commit.meta.sha.as_str(),
                    ts: commit.meta.author_ts,
                    subject: &commit.meta.subject,
                    refs: &commit.refs,
                });
            }
            TimelineItem::Github(_, g) => {
                out.extend(json_rows(&chunk, reviews));
                chunk.clear();
                let ts = i64::try_from(g.at).unwrap_or(i64::MAX);
                out.push(match g.backend {
                    crate::cli::github::timeline::Backend::Github => LogJsonRow::Github {
                        delivery: g.delivery(),
                        members: &g.members,
                        ts,
                        repo: &g.source,
                        event: &g.kind,
                        detail: g.detail.as_deref(),
                        number: g.number,
                        title: g.content.as_deref(),
                        actor: g.actor.as_deref(),
                        url: g.url.as_deref(),
                        seen_by: &g.seen_by,
                        unhandled: g.unhandled,
                        baseline: g.baseline,
                    },
                    crate::cli::github::timeline::Backend::Command => LogJsonRow::Command {
                        delivery: g.delivery(),
                        members: &g.members,
                        ts,
                        source: &g.source,
                        event: &g.kind,
                        content: g.content.as_deref(),
                        failure: g.failure,
                        seen_by: &g.seen_by,
                        unhandled: g.unhandled,
                        baseline: g.baseline,
                        replies: &g.replies,
                        reply_history_pruned: g.reply_history_pruned,
                        outgoing: &g.outgoing,
                    },
                    crate::cli::github::timeline::Backend::Timer => LogJsonRow::Timer {
                        delivery: g.delivery(),
                        members: &g.members,
                        ts,
                        recurrence: g.recurrence.as_ref(),
                        due_at: g.at,
                        source: &g.source,
                        content: g.content.as_deref(),
                        seen_by: &g.seen_by,
                        unhandled: g.unhandled,
                    },
                    crate::cli::github::timeline::Backend::Message => LogJsonRow::Message {
                        delivery: g.delivery(),
                        ts,
                        body: g.body.as_deref(),
                        outgoing: &g.outgoing,
                        members: &g.members,
                        outgoing_only: g.outgoing_only(),
                        from: &g.source,
                        subject: g.content.as_deref(),
                        seen_by: &g.seen_by,
                        unhandled: g.unhandled,
                        baseline: g.baseline,
                    },
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
                duration_secs,
                plan,
                sha,
                ts,
                subject,
            } => LogJsonRow::Finalized {
                duration_secs: *duration_secs,
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

    #[test]
    fn actual_wal_delivery_has_shared_cli_json_and_web_states() {
        use crate::cli::github::{
            log::Transport,
            timeline::{self, DeliveryState},
        };
        let (repo, _, mut log, _, _) = timeline::tests::reply_fixture(false, true);
        let row = log.read_rows().unwrap().unwrap().rows.remove(0);
        let key = clank_core::wait::EventPresentation {
            source: "messages".into(),
            seq: row.seq,
            ident: row.ident.clone(),
        };
        for state in [
            DeliveryState::Unwoken,
            DeliveryState::Woken,
            DeliveryState::Acknowledged,
        ] {
            if state == DeliveryState::Woken {
                log.append_presentations(std::slice::from_ref(&key), 123, || true)
                    .unwrap();
            }
            if state == DeliveryState::Acknowledged {
                log.append_checked_ack(key.seq, &key.ident).unwrap();
            }
            let snapshot = timeline::timeline_snapshot(repo.path());
            let event = snapshot
                .events
                .iter()
                .find(|e| !e.members.is_empty())
                .unwrap();
            assert_eq!(event.delivery(), state);
            assert_eq!(event.members[0].transport, Transport::Message);
            let web = serde_json::to_value(event).unwrap();
            assert_eq!(web["delivery"], serde_json::to_value(state).unwrap());
            let items = interleave(&[], &snapshot.events);
            let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
            let lines = oneline_plain_lines(&rows);
            let line = lines
                .iter()
                .find(|s| s.contains("Original question"))
                .unwrap();
            assert!(line.starts_with(&format!("{} ✉ ", state.glyph())), "{line}");
            let reviews = Default::default();
            let json = serde_json::to_value(json_items(&items, &reviews)).unwrap();
            let entry = json
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["subject"] == "Original question")
                .unwrap();
            assert_eq!(entry["delivery"], web["delivery"]);
            assert_eq!(entry["members"], web["members"]);
            assert_eq!(entry["unhandled"], state != DeliveryState::Acknowledged);
        }
    }

    #[test]
    fn recurring_timer_provenance_reaches_log_json_and_web_event_state() {
        use crate::cli::{
            github::{log::EventLog, timeline},
            timer_source::recurrence::Recurrence,
        };
        let repo = tempfile::tempdir().unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("rev/events");
        let r = Recurrence::from_generation(clank_core::agent_config::TimerGeneration {
            name: "deploy\nextra".into(),
            scope: None,
            anchor: 100,
            interval: 10,
        })
        .unwrap();
        let content = "Check deployment\nReport result\u{1b}[31m";
        EventLog::open(&dir, &r.source_key())
            .unwrap()
            .accept_recurring(&r, content, 140)
            .unwrap();
        let snapshot = timeline::timeline_snapshot(repo.path());
        assert!(snapshot.notices.is_empty());
        assert_eq!(snapshot.events.len(), 1);
        let event = &snapshot.events[0];
        let summary = event_row_text(event);
        assert!(summary.contains("5 scheduled occurrences"), "{summary}");
        assert!(summary.contains("every 10s"));
        assert!(!summary.chars().any(char::is_control));
        assert_eq!(event.body.as_deref(), Some(content));
        let web = serde_json::to_value(event).unwrap();
        assert_eq!(web["recurrence"]["generation_id"], r.id);
        assert_eq!(web["recurrence"]["first_slot"], 0);
        assert_eq!(web["recurrence"]["last_slot"], 4);
        assert_eq!(web["body"], content);
        let items = interleave(&[], &snapshot.events);
        let reviews = Default::default();
        let json = serde_json::to_value(json_items(&items, &reviews)).unwrap();
        assert_eq!(json[0]["recurrence"], web["recurrence"]);
        assert_eq!(json[0]["due_at"], 140);
        assert_eq!(json[0]["content"], content);
    }

    #[test]
    fn a_timer_wal_projects_as_a_timer_in_log_and_web_state_and_ack_refreshes_it() {
        use crate::cli::github::{
            log::{EventLog, Transport},
            timeline,
        };
        let repo = tempfile::tempdir().unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("rev/events");
        let source = clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check-deploy".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: Some(
                "Check \u{1b}[31mdeployment\u{1b}[0m\t\nReport result\r\nFull body".into(),
            ),
        };
        let mut log = EventLog::open(&dir, &source.source_key(None)).unwrap();
        let seq = log
            .append_inbox(
                Transport::Timer,
                Some("timer/100"),
                None,
                Some(100),
                &clank_core::wait::WaitItem::TimerEvent {
                    presentation: None,
                    name: source.name.clone(),
                    recurrence: None,
                    id: "timer/100".into(),
                    due_at: 100,
                    content: source.content.clone().unwrap(),
                    selector: None,
                },
            )
            .unwrap();
        for acked in [false, true] {
            if acked {
                log.append_ack(seq).unwrap();
            }
            let snapshot = timeline::timeline_snapshot(repo.path());
            assert!(snapshot.notices.is_empty());
            assert_eq!(snapshot.events.len(), 1);
            let event = &snapshot.events[0];
            assert_eq!(event.backend, timeline::Backend::Timer);
            assert_eq!(
                event.at, 100,
                "display time is the due instant, not ingest time"
            );
            assert_eq!(event.unhandled, !acked);
            assert_eq!(event.content.as_deref(), source.content.as_deref());
            assert_eq!(event_badge(event), default_badge(timeline::Backend::Timer));
            let web = serde_json::to_value(event).unwrap();
            assert_eq!(web["backend"], "timer");
            assert_eq!(web["members"][0]["transport"], "timer");
            let items = interleave(&[], &snapshot.events);
            let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
            let lines = oneline_plain_lines(&rows);
            assert_eq!(lines.len(), 1);
            let mark = if acked { "✓✓" } else { "?" };
            assert!(
                lines[0].starts_with(&format!(
                    "{mark} {} ",
                    default_badge(timeline::Backend::Timer)
                )),
                "{lines:?}"
            );
            assert!(lines[0].contains("“Check deployment”"));
            assert!(!lines[0].chars().any(char::is_control), "{lines:?}");
            assert!(!lines[0].contains("Report result"));
            assert!(!lines[0].contains("[31m"));
            // The human log and the TUI row use this same description.
            assert!(!event_row_text(event).chars().any(char::is_control));
            let mut hostile_fields = event.clone();
            hostile_fields.source = "deploy\nsecond source".into();
            hostile_fields.actor = Some("\u{1b}]0;title\u{7}agent\r\nother".into());
            hostile_fields.detail = Some("\u{1b}[31mready\t".into());
            hostile_fields.kind = "timer\nother".into();
            let described = event_row_text(&hostile_fields);
            assert!(!described.chars().any(char::is_control), "{described:?}");
            assert_eq!(described, "@rev  deploy  “Check deployment”");
            hostile_fields.backend = timeline::Backend::Github;
            let described = event_row_text(&hostile_fields);
            assert!(!described.chars().any(char::is_control), "{described:?}");
            assert_eq!(described, "@rev  deploy timer/ready  “Check deployment”");
            let reviews = Default::default();
            let json = serde_json::to_value(json_items(&items, &reviews)).unwrap();
            assert_eq!(json[0]["kind"], "timer_event");
            assert_eq!(json[0]["due_at"], 100);
            assert_eq!(json[0]["unhandled"], !acked);
            assert_eq!(json[0]["content"], source.content.as_deref().unwrap());
            assert!(json[0].get("failure").is_none());
        }
    }

    #[test]
    fn outgoing_message_body_route_and_link_survive_log_json_and_oneline_projection() {
        use crate::cli::github::timeline;
        let (repo, _external, _source, _target, op) = timeline::tests::reply_fixture(false, true);
        let snapshot = timeline::timeline_snapshot(repo.path());
        let reply = snapshot
            .events
            .iter()
            .find(|e| !e.outgoing.is_empty())
            .unwrap();
        let items = vec![TimelineItem::Github(0, reply)];
        let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
        assert!(matches!(
            &rows[0],
            OnelineRow::Github {
                delivery: crate::cli::github::timeline::DeliveryState::Sent,
                unhandled: false,
                ..
            }
        ));
        let described = event_row_text(reply);
        let crate::cli::github::log::ReplyRoute::Message(to) = &op.route else {
            panic!("a message reply")
        };
        assert!(
            described.starts_with("@")
                && described.contains(&format!(" → @coordinator ({})", to.repo.display())),
            "an outgoing reply names its sender, its target and the repo it is in: {described}"
        );
        assert_eq!(event_badge(reply), "✉");
        let reviews = Default::default();
        let json = serde_json::to_value(json_items(&items, &reviews)).unwrap();
        assert_eq!(json[0]["kind"], "Message");
        assert_eq!(json[0]["body"], op.text);
        assert_eq!(json[0]["outgoing_only"], true);
        assert_eq!(
            json[0]["outgoing"][0]["destination"]["agent"],
            "coordinator"
        );
        assert_eq!(json[0]["outgoing"][0]["operation_id"], op.id);
        assert_eq!(
            json[0]["outgoing"][0]["original_handle"],
            op.original_handle
        );
        assert_eq!(json[0]["members"], serde_json::json!([]));
        assert_eq!(json[0]["seen_by"], serde_json::json!([]));
        assert_eq!(json[0]["unhandled"], false);
    }

    fn test_member(unhandled: bool) -> crate::cli::github::timeline::MemberRef {
        use crate::cli::github::{
            log::Transport,
            timeline::{MemberKey, MemberRef},
        };
        MemberRef {
            key: MemberKey {
                agent: "claude".into(),
                source: "fixture".into(),
                seq: 1,
                ident: "fixture".into(),
            },
            acked: !unhandled,
            woken_at: None,
            transport: Transport::Poll,
            content: None,
        }
    }

    fn gh(at: u64, key_hint: &str, unhandled: bool) -> crate::cli::github::timeline::MergedEvent {
        crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at,
            backend: crate::cli::github::timeline::Backend::Github,
            glyph: None,
            source: "o/r".into(),
            kind: "pr_comment".into(),
            content: Some(key_hint.into()),
            body: None,
            failure: false,
            detail: Some("review".into()),
            number: Some(12),
            actor: Some("alice".into()),
            url: None,
            seen_by: vec!["claude".into()],
            sender: None,
            members: vec![test_member(unhandled)],
            unhandled,
            baseline: false,
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

    fn nudge(agent: &str, at: u64, attempt: &str) -> crate::cli::github::timeline::Nudge {
        crate::cli::github::timeline::Nudge {
            agent: agent.into(),
            record: crate::cli::nudge_log::Record {
                at,
                attempt: attempt.into(),
                trigger: crate::cli::nudge_log::Trigger::Manual,
                text: "carry on".into(),
                outcome: crate::cli::nudge_log::Outcome::Submitted,
            },
        }
    }

    /// Nudges sit among the commits and events by time, under the same
    /// floor, and render as history: their glyph, no delivery mark, and
    /// a JSON row of their own kind.
    #[test]
    fn nudges_interleave_by_time_as_history() {
        let e_new = adhoc(200, "new");
        let e_old = adhoc(100, "old");
        let events: Vec<&LogEvent> = vec![&e_new, &e_old];
        let gh_events = vec![gh(160, "between", false)];
        let nudges = vec![
            nudge("claude", 50, "too-old"),
            nudge("claude", 150, "n1"),
            nudge("codex", 300, "n2"),
        ];
        let items = interleave_with_plain(&events, &[], &gh_events, &nudges, true);
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                TimelineItem::Github(_, g) => g.content.clone().unwrap(),
                TimelineItem::Repo(LogEvent::AdHoc { subject, .. }) => subject.clone(),
                TimelineItem::Nudge(n) => n.record.attempt.clone(),
                _ => "?".into(),
            })
            .collect();
        assert_eq!(shape, ["n2", "new", "between", "n1", "old"]);
        let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
        let lines = oneline_plain_lines(&rows);
        assert!(
            lines.contains(&"↻ @codex nudged · by hand".to_string()),
            "{lines:?}"
        );
        let nudge_lines: Vec<&String> = lines.iter().filter(|l| l.contains("nudged")).collect();
        assert_eq!(nudge_lines.len(), 2);
        assert!(
            nudge_lines.iter().all(|l| l.starts_with("↻ @")),
            "no delivery mark: {lines:?}"
        );
        let json = serde_json::to_value(json_items(&items, &Default::default())).unwrap();
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == "nudge"
                    && row["agent"] == "codex"
                    && row["outcome"]["kind"] == "submitted")
        );
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
                TimelineItem::Github(_, g) => g.content.as_deref().unwrap(),
                TimelineItem::Repo(LogEvent::AdHoc { subject, .. }) => subject.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(shape, vec!["lead", "new", "between", "old"]);
        // The carried index is the ORIGINAL snapshot position (codex
        // d7f4d40): with the floor filtering "too-old" out and the
        // display reversed, each rendered row must still open ITS
        // OWN event — "lead" is snapshot[2], "between" snapshot[1] —
        // never a derived-list position.
        let idx_of = |title: &str| {
            items
                .iter()
                .find_map(|i| match i {
                    TimelineItem::Github(idx, g) if g.content.as_deref() == Some(title) => {
                        Some(*idx)
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(gh_events[idx_of("lead")].content.as_deref(), Some("lead"));
        assert_eq!(
            gh_events[idx_of("between")].content.as_deref(),
            Some("between")
        );
        assert_eq!(idx_of("lead"), 2, "original position, not display order");
        assert_eq!(idx_of("between"), 1, "floor filtering shifts nothing");
        // No commits at all → every gh event shows.
        let items = interleave(&[], &gh_events);
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn baseline_rows_dim_fully_in_color_and_mark_in_plain() {
        // codex c917ba7: pre-watch history dims the WHOLE row in the
        // color paths — not just a cyan badge — and keeps the textual
        // marker without ANSI in plain output.
        let base = gh(100, "old news", false);
        let base = crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            baseline: true,
            ..base
        };
        let colored = gh_colored_row(&base, "  (pre-watch)");
        assert!(colored.starts_with(DIM), "{colored:?}");
        assert!(colored.ends_with(Z), "{colored:?}");
        assert!(
            !colored.contains(C),
            "no normal-intensity badge: {colored:?}"
        );
        assert!(colored.contains("(pre-watch)"));
        // A live row keeps the cyan badge and normal text.
        let live = gh(100, "fresh", true);
        let colored = gh_colored_row(&live, "  [UNHANDLED]");
        assert!(colored.starts_with(C), "{colored:?}");
        assert!(!colored.contains(DIM), "{colored:?}");
    }

    /// External rows preserve their position inside one Git plan run.
    #[test]
    fn an_external_row_keeps_one_plan_run() {
        use crate::lifecycle::{CommitSha, PlanKey};
        let commit = |ts: i64| LogEvent::PlanCommit {
            plan: PlanKey::parse("foo").unwrap(),
            sha: CommitSha::parse(&format!("{ts:0<40x}")).unwrap(),
            ts,
            touched_plan: false,
            touched_code: true,
            subject: "[foo] work".into(),
        };
        let (newer, older) = (commit(200), commit(100));
        let events: Vec<&LogEvent> = vec![&newer, &older];
        // A github event dated between them stays inside the run.
        let gh_events = vec![gh(150, "between", true)];
        let items = interleave(&events, &gh_events);
        let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
        let shape: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                OnelineRow::Header { plan, .. } => plan.as_deref().unwrap_or("adhoc"),
                OnelineRow::Github { backend, .. } => match backend {
                    crate::cli::github::timeline::Backend::Github => "gh",
                    crate::cli::github::timeline::Backend::Command => "cmd",
                    crate::cli::github::timeline::Backend::Timer => "timer",
                    crate::cli::github::timeline::Backend::Message => "✉",
                },
                OnelineRow::Commit { .. } => "commit",
                _ => "other",
            })
            .collect();
        assert_eq!(
            shape,
            vec!["commit", "gh", "commit", "foo"],
            "one footer for the Git run: {rows:?}"
        );
    }

    fn run(name: &str, base: u64, intro_at: i64, finish_at: Option<i64>) -> Vec<LogEvent> {
        use crate::lifecycle::{CommitSha, PlanKey};
        let key = PlanKey::parse(name).unwrap();
        let hash = |n: u64| CommitSha::parse(&format!("{n:040x}")).unwrap();
        let mut events = Vec::new();
        if let Some(at) = finish_at {
            events.push(LogEvent::PlanFinalized {
                plan: key.clone(),
                sha: hash(base + 2),
                ts: at,
                duration_secs: 60,
                subject: format!("[{name}] finish"),
            });
        }
        events.push(LogEvent::PlanCommit {
            plan: key.clone(),
            sha: hash(base + 1),
            ts: intro_at + 1,
            touched_plan: false,
            touched_code: true,
            subject: format!("[{name}] work"),
        });
        events.push(LogEvent::PlanIntro {
            plan: key,
            sha: hash(base),
            ts: intro_at,
            subject: format!("[{name}] intro"),
        });
        events
    }

    /// The rows' shape, newest first: `-` a run's end rule, `=` a banner,
    /// `⚑` a finish, `c` another commit, `r` a review, `e` an event.
    fn shape(rows: &[OnelineRow]) -> String {
        rows.iter()
            .map(|r| match r {
                OnelineRow::RunEnd => '-',
                OnelineRow::Header { .. } => '=',
                OnelineRow::Commit {
                    marker: RowMarker::Finish,
                    ..
                } => '⚑',
                OnelineRow::Commit { .. } | OnelineRow::PlainCommit { .. } => 'c',
                OnelineRow::Review { .. } => 'r',
                OnelineRow::Github { .. } | OnelineRow::Notice(_) => 'e',
                OnelineRow::Nudge { .. } => 'n',
            })
            .collect()
    }

    fn rows_of(
        events: &[LogEvent],
        external: &[crate::cli::github::timeline::MergedEvent],
        reviews: &std::collections::BTreeMap<String, Vec<Review>>,
        stands_for: &StandsFor,
    ) -> Vec<OnelineRow> {
        let events: Vec<&LogEvent> = events.iter().collect();
        oneline_items_rows(&interleave(&events, external), reviews, stands_for)
    }

    /// A finished run is closed by a textless rule when anything but the
    /// next run follows it, decided from the rows as finally interleaved:
    /// an ad-hoc commit or an event after it, or an event between it and
    /// the next run's intro. The finish's own reviews stay inside it.
    #[test]
    fn a_finished_run_ends_with_a_rule_unless_the_next_run_follows() {
        let p = run("p", 10, 100, Some(300));
        let mut after = vec![adhoc(400, "drive by")];
        after.extend(p.clone());
        assert_eq!(
            shape(&rows_of(
                &after,
                &[],
                &Default::default(),
                &Default::default()
            )),
            "c-⚑cc="
        );

        let mut next = run("q", 20, 400, None);
        next.extend(p.clone());
        assert_eq!(
            shape(&rows_of(
                &next,
                &[],
                &Default::default(),
                &Default::default()
            )),
            "cc=⚑cc=",
            "the next banner"
        );
        assert_eq!(
            shape(&rows_of(
                &next,
                &[gh(350, "between", false)],
                &Default::default(),
                &Default::default()
            )),
            "cc=e-⚑cc=",
            "an event between the finish and the next run"
        );
        assert_eq!(
            shape(&rows_of(&p, &[], &Default::default(), &Default::default())),
            "⚑cc=",
            "nothing after it yet"
        );
        let active = run("a", 30, 100, None);
        let mut later = vec![adhoc(400, "drive by")];
        later.extend(active);
        assert_eq!(
            shape(&rows_of(
                &later,
                &[],
                &Default::default(),
                &Default::default()
            )),
            "ccc=",
            "an active run"
        );

        let finish = p[0].sha().as_str().to_string();
        let reviews = [(
            finish,
            vec![Review {
                verdict: Verdict::Finished,
                author: "codex".into(),
                summary: "done".into(),
                body: String::new(),
                at: Some(310),
            }],
        )]
        .into_iter()
        .collect();
        let rows = rows_of(&after, &[], &reviews, &Default::default());
        assert_eq!(shape(&rows), "c-r⚑cc=", "its review stays inside");
    }

    /// A finished plan collapsed to its one finalize commit is a run too.
    #[test]
    fn a_collapsed_finished_plan_ends_with_a_rule() {
        let p = run("solo", 10, 100, Some(300));
        let finish = p[0].clone();
        let stands_for: StandsFor = [(finish.sha().as_str().to_string(), "solo".to_string())]
            .into_iter()
            .collect();
        let events = vec![adhoc(400, "drive by"), finish];
        assert_eq!(
            shape(&rows_of(&events, &[], &Default::default(), &stands_for)),
            "c-⚑"
        );
    }

    /// Joined page by page, the rows have the one rule they have whole:
    /// a finish at the top of an older page is closed against the page
    /// above it, and passing over a seam twice adds nothing.
    #[test]
    fn a_run_end_survives_the_paging_seam_once() {
        let mut events = vec![adhoc(400, "drive by")];
        events.extend(run("p", 10, 100, Some(300)));
        let whole = rows_of(&events, &[], &Default::default(), &Default::default());
        let mut paged = rows_of(&events[..1], &[], &Default::default(), &Default::default());
        extend_oneline_rows(
            &mut paged,
            rows_of(&events[1..2], &[], &Default::default(), &Default::default()),
        );
        extend_oneline_rows(
            &mut paged,
            rows_of(&events[2..], &[], &Default::default(), &Default::default()),
        );
        assert_eq!(shape(&paged), shape(&whole));
        let len = paged.len();
        mark_run_ends(&mut paged, 0);
        assert_eq!(paged.len(), len);
        assert_eq!(
            paged
                .iter()
                .filter(|r| matches!(r, OnelineRow::RunEnd))
                .count(),
            1
        );
        assert!(oneline_plain_lines(&whole).contains(&RUN_END_RULE.to_string()));
    }

    #[test]
    fn older_pages_move_one_footer_to_the_intro_and_keep_its_duration() {
        use crate::lifecycle::{CommitSha, PlanKey};
        let key = PlanKey::parse("repeat").unwrap();
        let hash = |n| CommitSha::parse(&format!("{n:040x}")).unwrap();
        let intro = LogEvent::PlanIntro {
            plan: key.clone(),
            sha: hash(1),
            ts: 100,
            subject: "[repeat] intro".into(),
        };
        let work = LogEvent::PlanCommit {
            plan: key.clone(),
            sha: hash(3),
            ts: 200,
            touched_plan: false,
            touched_code: true,
            subject: "[repeat] work".into(),
        };
        let finish = LogEvent::PlanFinalized {
            plan: key,
            sha: hash(4),
            ts: 220,
            duration_secs: 120,
            subject: "[repeat] finish".into(),
        };
        let drive_by = LogEvent::AdHoc {
            sha: hash(2),
            ts: 150,
            subject: "drive by".into(),
        };
        let page =
            |events: &[&LogEvent]| oneline_rows(events, &Default::default(), &Default::default());
        let mut rows = page(&[&finish]);
        extend_oneline_rows(&mut rows, page(&[&work]));
        extend_oneline_rows(&mut rows, page(&[&drive_by]));
        extend_oneline_rows(&mut rows, page(&[&intro]));
        assert_eq!(oneline_plain_lines(&rows).last().unwrap(), "repeat · 2m");
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, OnelineRow::Header { .. }))
                .count(),
            1
        );
        assert!(matches!(
            rows.last(),
            Some(OnelineRow::Header {
                starts_here: true,
                ..
            })
        ));
        assert_eq!(
            rows.iter()
                .filter_map(|r| match r {
                    OnelineRow::Commit { sha, .. } => Some(sha.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [hash(4), hash(3), hash(2), hash(1)]
        );
        // A fully loaded newer run cannot merge into the same historical name.
        let old_intro = LogEvent::PlanIntro {
            plan: PlanKey::parse("repeat").unwrap(),
            sha: hash(5),
            ts: 40,
            subject: "[repeat] earlier intro".into(),
        };
        let old_finish = LogEvent::PlanFinalized {
            plan: PlanKey::parse("repeat").unwrap(),
            sha: hash(6),
            ts: 50,
            duration_secs: 10,
            subject: "[repeat] earlier finish".into(),
        };
        extend_oneline_rows(&mut rows, page(&[&old_finish, &old_intro]));
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, OnelineRow::Header { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn oneline_items_rows_splice_github_rows_for_the_tui() {
        let e = adhoc(100, "c");
        let events: Vec<&LogEvent> = vec![&e];
        let gh_events = vec![gh(150, "arrived", true)];
        let items = interleave(&events, &gh_events);
        let reviews = std::collections::BTreeMap::new();
        let rows = oneline_items_rows(&items, &reviews, &Default::default());
        assert!(
            matches!(&rows[0], OnelineRow::Github { unhandled: true, line, .. } if line.contains("o/r#12")),
            "{rows:?}"
        );
        // The repo chunk follows (umbrella header + commit).
        assert!(rows.iter().any(|r| matches!(r, OnelineRow::Commit { .. })));
        // Plain lines render the badge + open-work mark.
        let lines = oneline_plain_lines(&rows);
        assert!(
            lines[0].starts_with(&format!(
                "? {} ",
                default_badge(crate::cli::github::timeline::Backend::Github)
            )) && !lines[0].contains("⚠"),
            "{lines:?}"
        );
    }

    /// `--no-github` takes GITHUB out, and nothing else.
    ///
    /// It discarded the whole snapshot, which was the same thing
    /// while github was the only backend — and silently hid every
    /// command source the moment one wasn't (codex db5821e).
    #[test]
    fn no_github_hides_github_events_and_keeps_command_ones() {
        let ev = |backend, source: &str| crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 150,
            backend,
            glyph: None,
            source: source.into(),
            kind: "k".into(),
            content: None,
            body: None,
            failure: false,
            detail: None,
            number: None,
            actor: None,
            url: None,
            seen_by: Vec::new(),
            sender: None,
            members: vec![test_member(true)],
            unhandled: true,
            baseline: false,
        };
        let all = vec![
            ev(crate::cli::github::timeline::Backend::Github, "o/r"),
            ev(crate::cli::github::timeline::Backend::Command, "deploys"),
        ];

        let sources = |v: Vec<crate::cli::github::timeline::MergedEvent>| {
            v.into_iter().map(|e| e.source).collect::<Vec<_>>()
        };
        assert_eq!(
            sources(timeline_events_for(all.clone(), true)),
            vec!["deploys"],
            "the command source survives; only github is excluded",
        );
        assert_eq!(
            sources(timeline_events_for(all, false)),
            vec!["o/r", "deploys"],
            "and without the flag, both — or this would pass by dropping everything",
        );
    }

    /// A command event's LOG ROW and JSON say what it is.
    ///
    /// Every merged command event was emitted as `github_event` with
    /// the source name in `repo` and the content in `title`, and the
    /// gutter badge was hardcoded `gh` — a lie the consumer had no
    /// way to detect, and the `failure` flag was dropped entirely
    /// (codex 80e1fb9).
    #[test]
    fn a_command_events_row_and_json_are_not_dressed_up_as_github() {
        let cmd = crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 150,
            backend: crate::cli::github::timeline::Backend::Command,
            glyph: None,
            source: "deploys".into(),
            kind: "clank.source-failure".into(),
            content: Some("`sh -c ...` has stopped 3 times in a row".into()),
            body: None,
            failure: true,
            detail: None,
            number: None,
            actor: None,
            url: None,
            seen_by: vec!["claude".into()],
            sender: None,
            members: vec![test_member(true)],
            unhandled: true,
            baseline: false,
        };
        let e = adhoc(100, "c");
        let events: Vec<&LogEvent> = vec![&e];
        let gh_events = vec![cmd];
        let items = interleave(&events, &gh_events);

        // THE ROW: its own badge.
        let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
        let badge = rows
            .iter()
            .find_map(|r| match r {
                OnelineRow::Github { backend, .. } => Some(*backend),
                _ => None,
            })
            .expect("the command row is spliced in");
        assert_eq!(badge, crate::cli::github::timeline::Backend::Command);
        let lines = oneline_plain_lines(&rows);
        assert!(
            lines.iter().any(|l| l.starts_with(&format!(
                "? {} ",
                default_badge(crate::cli::github::timeline::Backend::Command)
            ))),
            "the gutter says what produced it: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("? gh ")),
            "and never claims github: {lines:?}"
        );
        // A broken SOURCE reads as one, even here.
        assert!(
            lines.iter().any(|l| l.contains("this SOURCE is broken")),
            "{lines:?}"
        );

        // THE JSON: its own shape, with no borrowed github fields.
        let reviews = std::collections::BTreeMap::new();
        let v = serde_json::to_value(json_items(&items, &reviews)).unwrap();
        let row = v
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "command_event")
            .expect("a command event is not a github_event");
        assert_eq!(row["source"], "deploys");
        assert_eq!(row["event"], "clank.source-failure");
        assert_eq!(row["content"], "`sh -c ...` has stopped 3 times in a row");
        assert_eq!(row["failure"], true, "the flag survives the JSON");
        assert!(row.get("repo").is_none(), "no borrowed repo: {row}");
        assert!(row.get("title").is_none(), "no borrowed title: {row}");
    }

    /// `log --json` carries what the row's mark says: an event's backend
    /// replies with their operation, owner and state, and for a reply
    /// whose event aged out, its operation and the copy it answered.
    #[test]
    fn log_json_carries_backend_reply_history() {
        use crate::cli::github::timeline::{Backend, BackendReply, MemberKey, OutgoingReply};
        let original = MemberKey {
            agent: "alpha".into(),
            source: "command-voice".into(),
            seq: 3,
            ident: "i".into(),
        };
        let mut with = gh(150, "note", true);
        with.backend = Backend::Command;
        with.replies = vec![BackendReply {
            original: original.clone(),
            operation_id: "op-1".into(),
            text: "on it".into(),
            at: 160,
            accepted: false,
        }];
        with.reply_history_pruned = true;
        let mut orphan = gh(170, "earlier", false);
        orphan.backend = Backend::Command;
        orphan.members = Vec::new();
        orphan.outgoing = vec![OutgoingReply {
            operation_id: "op-0".into(),
            delivery_id: "op-0".into(),
            from: "alpha".into(),
            from_repo: "/r".into(),
            destination: None,
            original: original.clone(),
            original_handle: "note-1".into(),
            acknowledge: false,
        }];
        let events: Vec<&LogEvent> = Vec::new();
        let gh_events = vec![with, orphan];
        let items = interleave(&events, &gh_events);
        let json = serde_json::to_value(json_items(&items, &Default::default())).unwrap();
        let rows = json.as_array().unwrap();
        let attached = rows
            .iter()
            .find(|r| r["replies"].is_array())
            .expect("{json}");
        assert_eq!(attached["kind"], "command_event");
        assert_eq!(attached["replies"][0]["operation_id"], "op-1");
        assert_eq!(attached["replies"][0]["accepted"], false);
        assert_eq!(attached["replies"][0]["original"]["agent"], "alpha");
        assert_eq!(attached["reply_history_pruned"], true);
        let alone = rows
            .iter()
            .find(|r| r["outgoing"].is_array())
            .expect("{json}");
        assert_eq!(alone["outgoing"][0]["operation_id"], "op-0");
        assert_eq!(alone["outgoing"][0]["original"]["seq"], 3);
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
                at: None,
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
                    duration_secs: 120,
                    plan: "p",
                    sha: "abc",
                    ts: 5,
                    subject: "[p] wrap it up",
                })
                .unwrap(),
                serde_json::json!({"kind":"finalized","plan":"p","sha":"abc","ts":5,"subject":"[p] wrap it up","duration_secs":120}),
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
            (
                serde_json::to_value(LogJsonRow::Plain {
                    sha: "abc",
                    ts: 5,
                    subject: "before clank",
                    refs: &["main".to_string()],
                })
                .unwrap(),
                serde_json::json!({"kind":"git-commit","sha":"abc","ts":5,"subject":"before clank","refs":["main"]}),
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }
}

#[cfg(test)]
mod tests {

    /// A review carries no time of its own: its file's mtime is the
    /// only record of when it was written, and it is what the log's
    /// time column reads.
    #[test]
    fn a_review_is_dated_by_the_file_it_was_written_to() {
        let dir = tempfile::tempdir().unwrap();
        let sha = CommitSha::parse(&"a".repeat(40)).unwrap();
        let path = dir
            .path()
            .join(".clank/agents/codex/feedback")
            .join(format!("{}.md", sha.as_str()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "CONTINUE lgtm\n").unwrap();
        let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();

        let reviews = collect_reviews(dir.path(), std::slice::from_ref(&sha));
        let [review] = reviews
            .get(sha.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default()
        else {
            panic!("exactly one review was written");
        };
        assert_eq!(review.author, "codex");
        assert_eq!(
            review.at,
            crate::age::written_at(&path),
            "dated by its own file"
        );
        assert!(
            crate::age::elapsed(review.at.unwrap()) >= 3500,
            "an hour ago, not now"
        );
    }
    use super::*;
    use clank_core::ids::PlanKey;

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }

    /// A finished plan of ONE commit loses its umbrella and says its
    /// name on the commit row. A plan with two keeps both.
    #[test]
    fn a_one_commit_finished_plan_collapses_and_a_two_commit_one_does_not() {
        let one = [LogEvent::PlanFinalized {
            duration_secs: 0,
            plan: PlanKey::parse("solo").unwrap(),
            sha: sha("aa"),
            ts: 2,
            subject: "[solo] did the thing".into(),
        }];
        let stands: super::StandsFor = [(sha("aa").as_str().to_string(), "solo".to_string())]
            .into_iter()
            .collect();
        let refs: Vec<&LogEvent> = one.iter().collect();
        let rows = oneline_rows(&refs, &Default::default(), &stands);
        assert!(
            !rows.iter().any(|r| matches!(r, OnelineRow::Header { .. })),
            "no umbrella over a single commit: {rows:?}"
        );
        match &rows[0] {
            OnelineRow::Commit {
                sha: s,
                marker,
                stands_for,
                ..
            } => {
                assert_eq!(s, &sha("aa"));
                assert_eq!(*marker, RowMarker::Finish, "the flag still marks it");
                assert_eq!(stands_for.as_deref(), Some("solo"));
            }
            other => panic!("expected one commit row, got {other:?}"),
        }

        // Two commits: the umbrella earns its place.
        let two = [
            LogEvent::PlanIntro {
                plan: PlanKey::parse("duo").unwrap(),
                sha: sha("cc"),
                ts: 1,
                subject: "[duo] intro".into(),
            },
            LogEvent::PlanFinalized {
                duration_secs: 0,
                plan: PlanKey::parse("duo").unwrap(),
                sha: sha("dd"),
                ts: 2,
                subject: "[duo] wrap up".into(),
            },
        ];
        let refs: Vec<&LogEvent> = two.iter().collect();
        let rows = oneline_rows(&refs, &Default::default(), &Default::default());
        assert!(
            rows.iter()
                .any(|r| matches!(r, OnelineRow::Header { plan: Some(p), .. } if p == "duo")),
            "a two-commit plan keeps its umbrella: {rows:?}"
        );
    }

    /// The case the first draft of this plan would have got wrong: a
    /// GitHub event splits a two-commit plan into chunks, so one chunk
    /// holds ONLY its finalize. Eligibility comes from the fold, not
    /// from what a chunk happens to contain, so it must not collapse.
    #[test]
    fn a_plan_split_across_chunks_is_not_a_one_commit_plan() {
        let intro = LogEvent::PlanIntro {
            plan: PlanKey::parse("split").unwrap(),
            sha: sha("11"),
            ts: 1,
            subject: "[split] intro".into(),
        };
        let fin = LogEvent::PlanFinalized {
            duration_secs: 0,
            plan: PlanKey::parse("split").unwrap(),
            sha: sha("22"),
            ts: 3,
            subject: "[split] wrap up".into(),
        };
        let event = crate::cli::github::timeline::MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 2,
            backend: crate::cli::github::timeline::Backend::Github,
            glyph: None,
            source: "o/r".into(),
            kind: "pr_comment".into(),
            content: None,
            body: None,
            failure: false,
            detail: None,
            number: Some(1),
            actor: None,
            url: None,
            seen_by: Vec::new(),
            sender: None,
            members: Vec::new(),
            unhandled: false,
            baseline: false,
        };
        let items = vec![
            TimelineItem::Repo(&fin),
            TimelineItem::Github(0, &event),
            TimelineItem::Repo(&intro),
        ];
        // The fold says this plan is TWO commits, so nothing is
        // eligible — even though the first chunk shows one.
        let rows = oneline_items_rows(&items, &Default::default(), &Default::default());
        let headers = rows
            .iter()
            .filter(|r| matches!(r, OnelineRow::Header { plan: Some(p), .. } if p == "split"))
            .count();
        assert_eq!(headers, 1, "one run keeps one umbrella: {rows:?}");
        assert!(
            rows.iter().all(|r| !matches!(
                r,
                OnelineRow::Commit {
                    stands_for: Some(_),
                    ..
                }
            )),
            "and no row claims to stand for the whole plan: {rows:?}"
        );
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
                duration_secs: 1,
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
        let refs: Vec<&LogEvent> = events.iter().rev().collect();
        let mut reviews = std::collections::BTreeMap::new();
        reviews.insert(
            sha("aa").as_str().to_string(),
            vec![Review {
                at: None,
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
                at: None,
                author: "codex".into(),
                verdict: Verdict::Continue,
                summary: "x".into(),
                body: String::new(),
            }],
        );
        let lines = oneline_plain_lines(&oneline_rows(&refs, &reviews, &Default::default()));
        // Umbrella shape (log-plan-umbrellas + adhoc-commit-marker):
        // each commit LEADS with its 1-col marker icon (planning `✎`,
        // finish `⚑`, ad-hoc `~`), then the sha, then the subject
        // (finish shows its real whole-plan message). The plan's name
        // sits at col 0 UNDER its run — newest-first, so a plan's
        // beginning is at its bottom. The ad-hoc commit still has no
        // header of its own; it simply follows, marked per row.
        assert_eq!(
            lines,
            vec![
                "    ✓ codex: x".to_string(),
                "~ cc00000 drive-by".to_string(),
                "⚑ bb00000 wrap up".to_string(),
                "    ✓ codex: lgtm".to_string(),
                "✎ aa00000 intro".to_string(),
                "foo · 1s".to_string(),
            ]
        );
    }

    /// A finalize stays legible while ANOTHER plan is active — which
    /// `clank status` does not cover.
    ///
    /// `a-repo-transition-is-derived-not-delivered` retired the
    /// `Finished` wake on the grounds that the state is derived and
    /// readable. `status` only prints `last finished` when no plan is
    /// active (`select_plans_and_finished` returns None otherwise), so
    /// for the case the retired wake also covered — one plan finalizes
    /// while another runs — the log is the surface, and this pins it
    /// (codex on 777fb2c).
    #[test]
    fn a_finalize_is_marked_even_while_another_plan_is_active() {
        let events = [
            LogEvent::PlanFinalized {
                duration_secs: 0,
                plan: PlanKey::parse("done").unwrap(),
                sha: sha("bb"),
                ts: 2,
                subject: "[done] wrap up".into(),
            },
            LogEvent::PlanIntro {
                plan: PlanKey::parse("running").unwrap(),
                sha: sha("cc"),
                ts: 3,
                subject: "[running] intro".into(),
            },
        ];
        let refs: Vec<&LogEvent> = events.iter().collect();
        let lines = oneline_plain_lines(&oneline_rows(
            &refs,
            &Default::default(),
            &Default::default(),
        ));

        assert!(
            lines.iter().any(|l| l.starts_with("⚑ bb00000")),
            "the finalize keeps its own marker: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "done · 0s"),
            "and names the plan that finished: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("✎ cc00000")),
            "beside the plan still running: {lines:?}"
        );
    }

    #[test]
    fn human_review_lines_shape_for_adhoc_reviews() {
        // adhoc-reviews-in-log: the pure line builder `clank log`'s
        // AdHoc arm now shares with the plan path — verdict mark,
        // author, summary snip, indented body.
        let rs = vec![Review {
            at: None,
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
        let rows = oneline_rows(&refs, &Default::default(), &Default::default());
        // Exactly one header (foo) — no "adhoc" header.
        let headers: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r, OnelineRow::Header { .. }))
            .collect();
        assert_eq!(headers.len(), 1, "no separate adhoc header: {rows:?}");
        assert!(matches!(headers[0], OnelineRow::Header { plan: Some(p), .. } if p == "foo"));
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
            at: 0,
            sha: sha("aa"),
            subject: "x".into(),
            marker: RowMarker::Impl,

            stands_for: None,
        };
        let adhoc_row = OnelineRow::Commit {
            at: 0,
            sha: sha("bb"),
            subject: "x".into(),
            marker: RowMarker::AdHoc,

            stands_for: None,
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
        // An invalid comma candidate still carries historical commit text.
        let e2 = LogEvent::PlanCommit {
            plan: foo.clone(),
            sha: sha(2),
            ts: 2,
            touched_plan: false,
            touched_code: true,
            subject: "[foo,bar] shared change".into(),
        };
        let events = [&e1, &e2];
        let rows = oneline_rows(&events, &Default::default(), &Default::default());
        let lines = oneline_plain_lines(&rows);
        assert!(
            lines[0].ends_with(" intro"),
            "matching prefix stripped: {lines:?}"
        );
        assert!(
            lines[1].ends_with(" [foo,bar] shared change"),
            "invalid candidate kept verbatim: {lines:?}"
        );
        // The name closes its run: last, at col 0.
        assert_eq!(lines[2], "foo", "umbrella name under its commits");
    }
}

#[cfg(test)]
mod event_row_tests {
    use super::*;
    use crate::cli::github::timeline::{Backend, MemberKey, MergedEvent, OutgoingReply};

    fn event(backend: Backend, source: &str, kind: &str, seen_by: &[&str]) -> MergedEvent {
        MergedEvent {
            replies: Vec::new(),
            reply_history_pruned: false,
            outgoing: Vec::new(),
            recurrence: None,
            at: 150,
            backend,
            glyph: None,
            source: source.into(),
            kind: kind.into(),
            content: None,
            body: None,
            failure: false,
            detail: None,
            number: None,
            actor: None,
            url: None,
            seen_by: seen_by.iter().map(|s| s.to_string()).collect(),
            sender: None,
            members: Vec::new(),
            unhandled: true,
            baseline: false,
        }
    }

    fn saying(mut e: MergedEvent, glyph: Option<&str>, content: &str) -> MergedEvent {
        e.glyph = glyph.map(str::to_string);
        e.content = Some(content.into());
        e
    }

    fn reply(from_repo: &str, to_repo: &str, agent: &str) -> OutgoingReply {
        OutgoingReply {
            operation_id: "op".into(),
            delivery_id: "d".into(),
            from: "claude".into(),
            from_repo: from_repo.into(),
            destination: Some(clank_core::wait::MessageReplyRoute {
                repo: to_repo.into(),
                agent: clank_core::ids::AgentLabel::parse(agent).unwrap(),
            }),
            original: MemberKey {
                agent: "claude".into(),
                source: "messages".into(),
                seq: 1,
                ident: "i".into(),
            },
            original_handle: "h".into(),
            acknowledge: false,
        }
    }

    /// The rows Life reported, which repeated the source's name where
    /// its icon already says it.
    #[test]
    fn signal_voice_and_message_rows_say_who_they_are_for() {
        let signal = saying(
            event(Backend::Command, "signal", "signal", &["claude"]),
            Some("💬"),
            "can you book the dentist\nfor Thursday",
        );
        assert_eq!(
            event_row_text(&signal),
            "@claude  “can you book the dentist”"
        );
        let voice = saying(
            event(Backend::Command, "voice", "voice", &["assistent", "claude"]),
            Some("🎙"),
            "remind me at 5",
        );
        assert_eq!(
            event_row_text(&voice),
            "@assistent,claude  “remind me at 5”"
        );
        let message = saying(
            event(Backend::Message, "codex", "message", &["claude"]),
            None,
            "done",
        );
        assert_eq!(
            event_row_text(&message),
            "@claude  “done”",
            "the sender is in the details, not in place of the recipient"
        );
        let item = TimelineItem::Github(0, &signal);
        let rows = oneline_items_rows(&[item], &Default::default(), &Default::default());
        assert_eq!(
            oneline_plain_lines(&rows),
            [format!(
                "{} 💬 @claude  “can you book the dentist”",
                signal.delivery().glyph()
            )]
        );
    }

    #[test]
    fn a_source_without_its_own_icon_is_named_and_a_distinct_kind_is_kept() {
        let plain = saying(
            event(Backend::Command, "deploys", "deploys", &["claude"]),
            None,
            "x",
        );
        assert_eq!(event_row_text(&plain), "@claude  deploys  “x”");
        let kinded = saying(
            event(Backend::Command, "deploys", "finished", &["claude"]),
            None,
            "x",
        );
        assert_eq!(event_row_text(&kinded), "@claude  deploys/finished  “x”");
        let iconed = saying(
            event(Backend::Command, "deploys", "finished", &["claude"]),
            Some("🚀"),
            "x",
        );
        assert_eq!(event_row_text(&iconed), "@claude  finished  “x”");
        let mut broken = iconed.clone();
        broken.kind = "clank.source-failure".into();
        broken.failure = true;
        assert_eq!(
            event_row_text(&broken),
            "@claude  deploys  “x”  (this SOURCE is broken, not the world)",
            "a broken source is named whatever its icon"
        );
    }

    #[test]
    fn github_rows_keep_repo_number_and_action_but_not_the_actor() {
        let mut pr = saying(
            event(Backend::Github, "o/r", "pr_comment", &["claude"]),
            None,
            "Ship it",
        );
        pr.number = Some(12);
        pr.detail = Some("review".into());
        pr.actor = Some("alice".into());
        assert_eq!(
            event_row_text(&pr),
            "@claude  o/r#12 pr_comment/review  “Ship it”"
        );
    }

    #[test]
    fn a_destination_nobody_recorded_is_shown_unknown_not_guessed() {
        let orphan = saying(
            event(Backend::Command, "deploys", "deploys", &[]),
            Some("🚀"),
            "x",
        );
        assert_eq!(event_row_text(&orphan), "@?  “x”");
    }

    #[test]
    fn a_reply_names_its_target_and_the_repo_only_when_it_is_another() {
        let mut here = saying(
            event(Backend::Message, "claude", "message", &[]),
            None,
            "ok",
        );
        here.outgoing = vec![reply("/src/life", "/src/life", "codex")];
        assert_eq!(event_row_text(&here), "→ @codex  “ok”");

        let mut there = here.clone();
        there.outgoing = vec![reply("/src/life", "/src/clank", "codex")];
        assert_eq!(event_row_text(&there), "→ @codex (/src/clank)  “ok”");

        let mut same_name = here.clone();
        same_name.outgoing = vec![reply("/src/life", "/work/life", "codex")];
        assert_eq!(
            event_row_text(&same_name),
            "→ @codex (/work/life)  “ok”",
            "a repo is shown by its path, so one sharing the sender's name is not mistaken for it"
        );

        let mut both = there.clone();
        both.seen_by = vec!["claude".into()];
        assert_eq!(
            event_row_text(&both),
            "@claude → @codex (/src/clank)  “ok”",
            "a local holder and a reply's target are both shown, each as what it is"
        );
    }
}

#[cfg(test)]
pub(crate) mod unborn_tests {
    use super::*;
    use crate::cli::github::log::{EventLog, Transport};
    use crate::cli::github::timeline::DeliveryState;

    /// Oldest first, each in the delivery state its fixture leaves it in.
    pub(crate) const MESSAGES: [(&str, DeliveryState); 3] = [
        ("never woken for", DeliveryState::Unwoken),
        ("woken for", DeliveryState::Woken),
        ("acknowledged", DeliveryState::Acknowledged),
    ];

    pub(crate) fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_DATE", "@9000 +0000")
            .env("GIT_COMMITTER_DATE", "@9000 +0000")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A branch with no commits whose `claude` has been sent
    /// [`MESSAGES`], all before the time [`git`] dates commits at: the
    /// first commit comes after the messages, as it did when they hid.
    pub(crate) fn unborn_repo_with_messages() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "--quiet", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "t@t"]);
        git(repo.path(), &["config", "user.name", "t"]);
        let mut log = EventLog::open(
            &crate::agent_store::agents_root(repo.path()).join("claude/events"),
            crate::inbox::MESSAGES_SOURCE,
        )
        .unwrap();
        for (i, (subject, _)) in MESSAGES.iter().enumerate() {
            log.append_inbox(
                Transport::Message,
                None,
                Some(&format!("m{i}")),
                Some(2000 + 1000 * i as u64),
                &clank_core::wait::WaitItem::Message {
                    presentation: None,
                    handle: format!("h{i}"),
                    from: Some("codex".into()),
                    repo: None,
                    subject: (*subject).into(),
                    body: (*subject).into(),
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();
        }
        let rows = log.read_rows().unwrap().unwrap().rows;
        for (row, (_, state)) in rows.iter().zip(MESSAGES) {
            match state {
                DeliveryState::Woken => {
                    let key = clank_core::wait::EventPresentation {
                        source: crate::inbox::MESSAGES_SOURCE.into(),
                        seq: row.seq,
                        ident: row.ident.clone(),
                    };
                    log.append_presentations(&[key], 2500, || true).unwrap();
                }
                DeliveryState::Acknowledged => log.append_checked_ack(row.seq, &row.ident).unwrap(),
                _ => {}
            }
        }
        repo
    }

    async fn log(repo: &Path, flags: &[&str]) -> anyhow::Result<String> {
        let argv = ["clank", "log", "--repo", repo.to_str().unwrap()]
            .into_iter()
            .chain(flags.iter().copied());
        let crate::cli::command::Cli {
            command: crate::cli::command::Command::Log(args),
        } = clap::Parser::try_parse_from(argv)?
        else {
            panic!("`clank log` parses as a log");
        };
        let mut out = Vec::new();
        write_log(&args, false, &mut out).await?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn json(text: &str) -> Vec<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(text)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    const COMMIT_KINDS: [&str; 7] = [
        "intro",
        "commit",
        "finalized",
        "deleted",
        "ad-hoc",
        "git-commit",
        "review",
    ];

    #[tokio::test]
    async fn an_unborn_branch_logs_its_messages_and_says_it_has_no_commits() {
        let repo = unborn_repo_with_messages();
        for flags in [&[][..], &["--oneline"][..]] {
            let out = log(repo.path(), flags).await.unwrap();
            let lines: Vec<&str> = out.lines().collect();
            let rows: Vec<&str> = MESSAGES
                .iter()
                .rev()
                .map(|(subject, state)| {
                    let mark = format!("{} ✉ ", state.glyph());
                    *lines
                        .iter()
                        .find(|l| l.starts_with(&mark) && l.contains(subject))
                        .unwrap_or_else(|| panic!("`{mark}… {subject}` in {out}"))
                })
                .collect();
            let at = |row: &str| lines.iter().position(|l| *l == row).unwrap();
            assert!(
                rows.windows(2).all(|w| at(w[0]) < at(w[1])),
                "newest first: {out}"
            );
            assert_eq!(
                lines.last(),
                Some(&"branch `main` has no commits yet"),
                "{out}"
            );
        }
    }

    #[tokio::test]
    async fn an_unborn_branch_json_is_its_events_and_no_commit_rows() {
        let repo = unborn_repo_with_messages();
        for flags in [&["--json"][..], &["--json", "--no-github"][..]] {
            let rows = json(&log(repo.path(), flags).await.unwrap());
            let subjects: Vec<&str> = rows.iter().filter_map(|r| r["subject"].as_str()).collect();
            assert_eq!(subjects, ["acknowledged", "woken for", "never woken for"]);
            assert!(
                rows.iter()
                    .all(|r| !COMMIT_KINDS.contains(&r["kind"].as_str().unwrap())),
                "{rows:?}"
            );
        }

        let rows = json(
            &log(repo.path(), &["--json", "--plan", "foo"])
                .await
                .unwrap(),
        );
        assert!(rows.is_empty(), "a plan's view leaves repo-wide events out");

        let empty = tempfile::tempdir().unwrap();
        git(empty.path(), &["init", "--quiet", "-b", "main"]);
        assert_eq!(log(empty.path(), &["--json"]).await.unwrap().trim(), "[]");
    }

    #[tokio::test]
    async fn the_first_commit_joins_the_same_events() {
        let repo = unborn_repo_with_messages();
        let before = json(&log(repo.path(), &["--json"]).await.unwrap());
        git(
            repo.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "first"],
        );

        let after = json(&log(repo.path(), &["--json"]).await.unwrap());
        let (commits, events): (Vec<_>, Vec<_>) = after
            .into_iter()
            .partition(|r| COMMIT_KINDS.contains(&r["kind"].as_str().unwrap()));
        assert_eq!(events, before, "the same events, once each");
        assert_eq!(commits.len(), 1, "{commits:?}");
        assert_eq!(commits[0]["subject"], "first");

        let human = log(repo.path(), &[]).await.unwrap();
        assert!(human.contains("    first"), "{human}");
        assert!(!human.contains("no commits yet"), "{human}");
    }

    #[tokio::test]
    async fn an_unborn_branch_still_refuses_history_it_cannot_resolve() {
        let repo = unborn_repo_with_messages();
        for range in ["nosuch", "nosuch..main", "main"] {
            let err = log(repo.path(), &[range]).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("cannot resolve"),
                "{range}: {err:#}"
            );
        }
        let not_a_repo = tempfile::tempdir().unwrap();
        assert!(log(not_a_repo.path(), &[]).await.is_err());
    }

    #[tokio::test]
    async fn an_unborn_branch_has_no_history_diagnostics() {
        let repo = unborn_repo_with_messages();
        assert_eq!(
            log(repo.path(), &["--diagnostics"]).await.unwrap(),
            "no history diagnostics\n"
        );
        assert_eq!(
            json(
                &log(repo.path(), &["--diagnostics", "--json"])
                    .await
                    .unwrap()
            ),
            Vec::<serde_json::Value>::new()
        );
    }
}
