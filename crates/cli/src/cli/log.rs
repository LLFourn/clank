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

    if filtered.is_empty() {
        println!("no log events");
        return Ok(());
    }

    // Most recent first (git log convention).
    let filtered: Vec<&LogEvent> = filtered.into_iter().rev().collect();

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
        print_json(&filtered, &repo, &reviewable_shas)?;
    } else if args.oneline {
        print_oneline(&filtered, &repo, &reviewable_shas)?;
    } else {
        print_human(&filtered, &repo, &reviewable_shas)?;
    }
    Ok(())
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
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%an <%ae>%n%ai%n%B", sha.as_str()])
        .output();
    match o {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            let mut lines = text.lines();
            let author = lines.next().unwrap_or("").to_string();
            let date = lines.next().unwrap_or("").to_string();
            let body: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
            (author, date, body)
        }
        _ => (String::new(), String::new(), String::new()),
    }
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
        Verdict::Approve => ("✓", G),
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

fn print_human(events: &[&LogEvent], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let c = color();
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            println!();
        }
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
        if let Some(rs) = reviews.get(sha.as_str()) {
            for r in rs {
                let m = verdict_mark(r.verdict, c);
                let snip = if r.summary.is_empty() {
                    String::new()
                } else {
                    format!(": {}", r.summary)
                };
                if c {
                    println!("    {m} {C}{}{Z} {}{snip}", r.author, r.verdict);
                } else {
                    println!("    {m} {} {}{snip}", r.author, r.verdict);
                }
                if !r.body.is_empty() {
                    println!();
                    for line in r.body.lines() {
                        println!("        {line}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// One `--oneline` display row: data only, no formatting — shared
/// by the CLI renderer (which colors it) and the status TUI's log
/// pane (which dims it). Plan: status-tui-live-log.
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
    },
    Review {
        verdict: Verdict,
        author: String,
        summary: String,
    },
}

/// Pure row producer for the oneline view. Subjects come from the
/// fold's `LogEvent`s (carried since status-tui-live-log) — no
/// per-event `git log -1` shelling, which matters for the live TUI
/// pane re-rendering every refresh.
pub(crate) fn oneline_rows(
    events: &[&LogEvent],
    reviews: &std::collections::BTreeMap<String, Vec<Review>>,
) -> Vec<OnelineRow> {
    use clank_core::repo_state::{UmbrellaKey, parse_subject, umbrella_sections};
    let mut out = Vec::new();
    for (key, run) in umbrella_sections(events) {
        let umbrella_plan = match &key {
            UmbrellaKey::Plan(p) => Some(p.as_str().to_string()),
            UmbrellaKey::AdHoc => None,
        };
        out.push(OnelineRow::Header {
            plan: umbrella_plan.clone(),
        });
        for event in run {
            let (sha, subject) = match event {
                LogEvent::PlanIntro { sha, subject, .. }
                | LogEvent::PlanCommit { sha, subject, .. }
                | LogEvent::AdHoc { sha, subject, .. } => (sha, subject.clone()),
                LogEvent::PlanFinalized { plan, sha, .. } => {
                    (sha, format!("[{}] finish", plan.as_str()))
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
            out.push(OnelineRow::Commit {
                sha: sha.clone(),
                subject,
            });
            // AdHoc rows carried no review sub-lines before the
            // factoring; keep that shape.
            if matches!(event, LogEvent::AdHoc { .. }) {
                continue;
            }
            if let Some(rs) = reviews.get(sha.as_str()) {
                for r in rs {
                    out.push(OnelineRow::Review {
                        verdict: r.verdict,
                        author: r.author.clone(),
                        summary: r.summary.clone(),
                    });
                }
            }
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
            OnelineRow::Commit { sha, subject } => format!("  {} {subject}", short(sha)),
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

fn print_oneline(events: &[&LogEvent], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let c = color();
    for row in oneline_rows(events, &reviews) {
        match row {
            OnelineRow::Header { plan } => {
                let name = plan.unwrap_or_else(|| "adhoc".to_string());
                if c {
                    println!("{C}{name}{Z}");
                } else {
                    println!("{name}");
                }
            }
            OnelineRow::Commit { sha, subject } => {
                if c {
                    println!("  {Y}{}{Z} {subject}", short(&sha));
                } else {
                    println!("  {} {subject}", short(&sha));
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
        plan: &'a str,
        sha: &'a str,
        ts: i64,
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
}

fn print_json(events: &[&LogEvent], repo: &Path, shas: &[CommitSha]) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
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
            LogEvent::PlanFinalized { plan, sha, ts } => LogJsonRow::Finalized {
                plan: plan.as_str(),
                sha: sha.as_str(),
                ts: *ts,
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
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

#[cfg(test)]
mod json_output_tests {
    use super::*;

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
                })
                .unwrap(),
                serde_json::json!({"kind":"finalized","plan":"p","sha":"abc","ts":5}),
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
                    verdict: Verdict::Approve,
                })
                .unwrap(),
                serde_json::json!({"kind":"review","sha":"abc","author":"codex","verdict":Verdict::Approve}),
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
                verdict: Verdict::Approve,
                summary: "lgtm".into(),
                body: String::new(),
            }],
        );
        // AdHoc carries reviews in the map but must NOT emit
        // sub-lines (pre-factoring shape).
        reviews.insert(
            sha("cc").as_str().to_string(),
            vec![Review {
                author: "codex".into(),
                verdict: Verdict::Approve,
                summary: "x".into(),
                body: String::new(),
            }],
        );
        let lines = oneline_plain_lines(&oneline_rows(&refs, &reviews));
        // Umbrella shape (log-plan-umbrellas): plan header at col
        // 0, commits indented with matching prefixes stripped,
        // ad-hoc commits under their own bucket.
        assert_eq!(
            lines,
            vec![
                "foo".to_string(),
                "  aa00000 intro".to_string(),
                "    ✓ codex: lgtm".to_string(),
                "  bb00000 finish".to_string(),
                "adhoc".to_string(),
                "  cc00000 drive-by".to_string(),
            ]
        );
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
