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

fn git_rev_parse(repo: &Path, rev: &str) -> Option<CommitSha> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    CommitSha::parse(String::from_utf8_lossy(&o.stdout).trim()).ok()
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

fn commit_subject(repo: &Path, sha: &CommitSha) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%s", sha.as_str()])
        .output();
    match o {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn short(sha: &CommitSha) -> &str {
    &sha.as_str()[..sha.as_str().len().min(7)]
}

// ── reviews ─────────────────────────────────────────────────

struct Review {
    author: String,
    verdict: Verdict,
    summary: String,
    body: String,
}

fn collect_reviews(
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

fn verdict_mark(v: Verdict, c: bool) -> String {
    let (mark, col) = match v {
        Verdict::Approve => ("✓", G),
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
    events: &[&LogEvent],
    repo: &Path,
    shas: &[CommitSha],
) -> anyhow::Result<()> {
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

fn print_oneline(
    events: &[&LogEvent],
    repo: &Path,
    shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let c = color();
    for event in events {
        let (_plan, sha, subj_override) = match event {
            LogEvent::PlanIntro { plan, sha, .. } | LogEvent::PlanCommit { plan, sha, .. } => {
                (plan, sha, None)
            }
            LogEvent::PlanFinalized { plan, sha, .. } => {
                (plan, sha, Some(format!("Finalize {}", plan.as_str())))
            }
            LogEvent::PlanDeleted { plan, sha, .. } => {
                (plan, sha, Some(format!("Delete {}", plan.as_str())))
            }
            LogEvent::AdHoc { sha, .. } => {
                let subj = commit_subject(repo, sha);
                if c {
                    println!("{Y}{}{Z} {subj}", short(sha));
                } else {
                    println!("{} {subj}", short(sha));
                }
                continue;
            }
        };
        let subj = subj_override.unwrap_or_else(|| commit_subject(repo, sha));
        if c {
            println!("{Y}{}{Z} {subj}", short(sha));
        } else {
            println!("{} {subj}", short(sha));
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
                    println!("  {m} {C}{}{Z}{snip}", r.author);
                } else {
                    println!("  {m} {}{snip}", r.author);
                }
            }
        }
    }
    Ok(())
}

fn print_json(
    events: &[&LogEvent],
    repo: &Path,
    shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, shas);
    let mut out: Vec<serde_json::Value> = Vec::new();
    for event in events {
        let obj = match event {
            LogEvent::PlanIntro { plan, sha, ts } => serde_json::json!({
                "kind": "intro", "plan": plan.as_str(), "sha": sha.as_str(),
                "ts": ts, "subject": commit_subject(repo, sha),
            }),
            LogEvent::PlanCommit {
                plan,
                sha,
                ts,
                touched_plan,
                touched_code,
            } => serde_json::json!({
                "kind": "commit", "plan": plan.as_str(), "sha": sha.as_str(),
                "ts": ts, "touched_plan": touched_plan, "touched_code": touched_code,
                "subject": commit_subject(repo, sha),
            }),
            LogEvent::PlanFinalized { plan, sha, ts } => serde_json::json!({
                "kind": "finalized", "plan": plan.as_str(), "sha": sha.as_str(), "ts": ts,
            }),
            LogEvent::PlanDeleted { plan, sha, ts } => serde_json::json!({
                "kind": "deleted", "plan": plan.as_str(), "sha": sha.as_str(), "ts": ts,
            }),
            LogEvent::AdHoc { sha, ts } => serde_json::json!({
                "kind": "ad-hoc", "sha": sha.as_str(), "ts": ts,
                "subject": commit_subject(repo, sha),
            }),
        };
        out.push(obj);
        let sha_str = match event {
            LogEvent::PlanIntro { sha, .. }
            | LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanFinalized { sha, .. }
            | LogEvent::PlanDeleted { sha, .. }
            | LogEvent::AdHoc { sha, .. } => sha.as_str(),
        };
        if let Some(rs) = reviews.get(sha_str) {
            for r in rs {
                out.push(serde_json::json!({
                    "kind": "review", "sha": sha_str,
                    "author": r.author, "verdict": r.verdict,
                }));
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
