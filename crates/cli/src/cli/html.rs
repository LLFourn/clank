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

use super::{HtmlArgs, repo_basename, resolve_repo};
use crate::cli::status::StatusSnapshot;

pub async fn run(args: HtmlArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let out_dir = repo.join(".clank/html");
    build_site(&repo, &basename, &out_dir).await?;
    println!("wrote {}", out_dir.display());
    if args.open {
        launch_opener(&out_dir.join("index.html"))?;
    }
    Ok(())
}

async fn build_site(repo: &Path, basename: &str, out_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(out_dir.join("commit"))?;

    let status = StatusSnapshot::build_async(
        repo,
        basename,
        crate::rebuild::CachePolicy::Use,
        None,
        false,
    )
    .await?;

    let head_sha = head_sha(repo)?;
    let events: Vec<LogEvent> = match head_sha.as_ref() {
        Some(head) => {
            let (_state, events) = crate::rebuild::rebuild_from(repo, None, head).await?;
            events
        }
        None => Vec::new(),
    };

    let event_shas: Vec<CommitSha> = events.iter().map(event_sha).cloned().collect();
    let reviews = collect_reviews(repo, &event_shas);
    let subjects = collect_subjects(repo, head_sha.as_ref());

    // Common shared CSS file.
    std::fs::write(out_dir.join("style.css"), CSS)?;

    // Index page.
    let index_html = render_index(&status, &events, &reviews, &subjects);
    std::fs::write(out_dir.join("index.html"), index_html)?;

    // Per-commit pages.
    for event in &events {
        let sha = event_sha(event);
        let path = out_dir.join(format!("commit/{}.html", sha.as_str()));
        let page = render_commit_page(repo, event, &reviews);
        std::fs::write(&path, page)?;
    }

    Ok(())
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
) -> String {
    let mut out = String::new();
    write_doc_open(&mut out, &format!("clank · {}", status.basename), ".");
    out.push_str("<header class=\"status\">\n");
    out.push_str(&format!(
        "  <h1>{}</h1>\n",
        esc(&status.basename)
    ));
    let branch = status.branch.as_deref().unwrap_or("(detached)");
    let head_short = status
        .head_sha
        .as_deref()
        .map(short)
        .unwrap_or("(no head)");
    let dirty = if status.worktree_dirty { " · dirty" } else { "" };
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
    if status.queue_count > 0 {
        out.push_str(&format!(
            "  <div class=\"queue\">queue: {} item{}</div>\n",
            status.queue_count,
            if status.queue_count == 1 { "" } else { "s" }
        ));
    }
    if !status.blocks.is_empty() {
        out.push_str("  <div class=\"blocks\">blocks:\n    <ul>\n");
        for b in &status.blocks {
            out.push_str(&format!(
                "      <li><code>{}</code>/{}{} — {}</li>\n",
                esc(&b.agent),
                esc(&b.name),
                b.plan.as_deref().map(|p| format!(" · plan {p}")).unwrap_or_default(),
                esc(&b.question)
            ));
        }
        out.push_str("    </ul>\n  </div>\n");
    }
    out.push_str("</header>\n");

    out.push_str("<main>\n");
    out.push_str("<h2>Timeline</h2>\n");
    if events.is_empty() {
        out.push_str("<p class=\"empty\">No events yet.</p>\n");
    } else {
        out.push_str("<ol class=\"timeline\" reversed>\n");
        for event in events.iter().rev() {
            let sha = event_sha(event);
            let plan = event_plan(event);
            let kind = event_kind_label(event);
            let marks = reviews
                .get(sha.as_str())
                .map(|v| verdict_marks_html(v.as_slice()))
                .unwrap_or_default();
            let subject = subjects
                .get(sha.as_str())
                .map(String::as_str)
                .unwrap_or("");
            out.push_str("<li class=\"row\">\n");
            out.push_str(&format!(
                "  <a class=\"row-link\" href=\"commit/{}.html\">\n",
                esc(sha.as_str())
            ));
            out.push_str(&format!(
                "    <code class=\"sha\">{}</code>\n",
                esc(short(sha.as_str()))
            ));
            out.push_str(&format!(
                "    <span class=\"kind kind-{}\">{}</span>\n",
                kind, kind
            ));
            if let Some(p) = plan {
                out.push_str(&format!(
                    "    <span class=\"plan-pill\">{}</span>\n",
                    esc(p)
                ));
            }
            out.push_str(&format!(
                "    <span class=\"subject\">{}</span>\n",
                esc(subject.trim())
            ));
            if !marks.is_empty() {
                out.push_str(&format!("    <span class=\"marks\">{marks}</span>\n"));
            }
            out.push_str(&format!(
                "    <span class=\"ts\">{}</span>\n",
                esc(&fmt_ts(event_ts(event)))
            ));
            out.push_str("  </a>\n");
            out.push_str("</li>\n");
        }
        out.push_str("</ol>\n");
    }
    out.push_str("</main>\n");
    write_doc_close(&mut out);
    out
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
        "  <h1><code>{}</code> <span class=\"kind kind-{}\">{}</span></h1>\n",
        esc(short(sha.as_str())),
        kind,
        kind
    ));
    if let Some(p) = plan {
        out.push_str(&format!(
            "  <div class=\"plan-line\">plan: <span class=\"plan-pill\">{}</span></div>\n",
            esc(p)
        ));
    }
    out.push_str(&format!(
        "  <h2 class=\"subject\">{}</h2>\n",
        esc(&subject)
    ));
    out.push_str(&format!(
        "  <p class=\"sha-full\"><code>{}</code></p>\n",
        esc(sha.as_str())
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

// ─────────────────────────── diff ───────────────────────────

struct FilePatch {
    header: String,
    hunks: Vec<String>,
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
    let mut cur_hunks: Vec<String> = Vec::new();
    let mut cur_hunk: Option<String> = None;
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
            cur_hunk = Some(format!("{line}\n"));
        } else if cur_hunk.is_some() {
            // Body line of the current hunk.
            cur_hunk.as_mut().unwrap().push_str(line);
            cur_hunk.as_mut().unwrap().push('\n');
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
    let mut s = String::new();
    s.push_str(&format!(
        "  <details open class=\"file\"><summary>{}</summary>\n",
        esc(&path)
    ));
    for hunk in &fp.hunks {
        s.push_str("    <pre class=\"hunk\"><code>\n");
        for line in hunk.lines() {
            let cls = match line.chars().next() {
                Some('+') if !line.starts_with("+++") => "add",
                Some('-') if !line.starts_with("---") => "del",
                Some('@') => "hunk-hdr",
                _ => "ctx",
            };
            s.push_str(&format!(
                "<span class=\"line {cls}\">{}</span>\n",
                esc(line)
            ));
        }
        s.push_str("    </code></pre>\n");
    }
    s.push_str("  </details>\n");
    s
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
    use pulldown_cmark::{Event, Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // Strip raw HTML events — feedback bodies and plan markdown
    // can contain user-controlled HTML that we don't want to
    // pass through verbatim. pulldown-cmark's `Html` and
    // `InlineHtml` events represent literal HTML tags from the
    // source; replace them with empty text so the output is
    // strictly the markdown-derived structure.
    let parser = Parser::new_ext(md, opts).filter(|ev| {
        !matches!(ev, Event::Html(_) | Event::InlineHtml(_))
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
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
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(CommitSha::parse(&s).map_err(|e| anyhow::anyhow!("parse HEAD sha `{s}`: {e}"))?))
}

fn commit_subject(repo: &Path, sha: &CommitSha) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%s", sha.as_str()])
        .output();
    let Ok(out) = out else { return String::new() };
    if !out.status.success() {
        return String::new();
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", &format!("{}:{}", sha.as_str(), path)])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
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
        anyhow::bail!("no known opener for this platform; open `{}` manually", path_str);
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
    out.push_str("</body></html>\n");
}

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
.gate { font-size: .8rem; padding: 0 .35rem; border-radius: 4px; }
.gate-approved { color: var(--approve); }
.gate-finished { color: var(--finished); }
.gate-changes_requested { color: var(--changes); }
h2 { font-size: 1rem; text-transform: uppercase; letter-spacing: .05em; color: var(--fg-dim); margin: 1rem 0 .5rem; }
.timeline { list-style: none; margin: 0; padding: 0; }
.row { border-top: 1px solid var(--rule); }
.row:first-child { border-top: 0; }
.row-link {
  display: grid;
  grid-template-columns: 5rem 4.5rem auto 1fr auto auto;
  gap: .6rem;
  align-items: center;
  padding: .35rem .15rem;
  text-decoration: none; color: inherit;
}
.row-link:hover { background: var(--pill-bg); }
.sha { font: 500 .85rem/1 var(--mono); color: var(--fg-dim); }
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
.hunk { font: .8rem/1.4 var(--mono); margin: .35rem 0; padding: .5rem .35rem; background: var(--bg); border: 1px solid var(--rule); border-radius: 3px; overflow-x: auto; }
.hunk code { background: transparent; padding: 0; }
.line { display: block; padding: 0 .25rem; white-space: pre; }
.line.add { background: var(--add-bg); }
.line.del { background: var(--del-bg); }
.line.hunk-hdr { color: var(--hunk-hdr); }
@media print {
  .row-link:hover { background: transparent; }
  details > summary { list-style: none; }
}
"#;
