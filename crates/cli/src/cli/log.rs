//! `clank log` — chronological timeline of commits and reviews
//! for one or more plans.

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

    let git_head = git_rev_parse_head(&repo).ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;

    let plan_filter = resolve_plan_filter(&repo, &basename, args.all, args.plan.as_deref())?;

    let (from, to) = match &args.range {
        Some(range) => parse_range(&repo, range)?,
        None => {
            let from = match &plan_filter {
                Some(keys) => earliest_plan_intro_parent(&repo, keys),
                None => None,
            };
            (from, git_head)
        }
    };

    let (_state, log_events) = crate::rebuild::rebuild_from(&repo, from.as_ref(), &to)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold range: {e}"))?;

    let filtered_events: Vec<&LogEvent> = log_events
        .iter()
        .filter(|e| match e {
            LogEvent::PlanIntro { plan, .. }
            | LogEvent::PlanCommit { plan, .. }
            | LogEvent::PlanFinalized { plan, .. }
            | LogEvent::PlanDeleted { plan, .. } => match &plan_filter {
                Some(keys) => keys.iter().any(|k| k == plan),
                None => true,
            },
        })
        .collect();

    // Apply limit (over commit groups: intro/commit/finalize each = 1 group).
    let limited: Vec<&LogEvent> = if args.limit > 0 {
        let total = filtered_events.len();
        let skip = total.saturating_sub(args.limit);
        filtered_events.into_iter().skip(skip).collect()
    } else {
        filtered_events
    };
    let filtered_events = limited;

    if filtered_events.is_empty() {
        println!("no log events");
        return Ok(());
    }

    let reviewable_shas: Vec<CommitSha> = filtered_events
        .iter()
        .filter_map(|e| match e {
            LogEvent::PlanCommit { sha, .. } | LogEvent::PlanIntro { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .collect();

    let plan_keys: Vec<PlanKey> = plan_filter.clone().unwrap_or_else(|| {
        let mut keys: Vec<PlanKey> = Vec::new();
        for e in &filtered_events {
            let k = match e {
                LogEvent::PlanIntro { plan, .. }
                | LogEvent::PlanCommit { plan, .. }
                | LogEvent::PlanFinalized { plan, .. }
                | LogEvent::PlanDeleted { plan, .. } => plan,
            };
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
        keys
    });

    if args.json {
        print_json(&filtered_events, &repo, &plan_keys, &reviewable_shas)?;
    } else if args.oneline {
        print_oneline(&filtered_events, &repo, &plan_keys, &reviewable_shas)?;
    } else {
        print_human(&filtered_events, &repo, &plan_keys, &reviewable_shas)?;
    }
    Ok(())
}

fn git_rev_parse_head(repo: &Path) -> Option<CommitSha> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    CommitSha::parse(String::from_utf8_lossy(&output.stdout).trim()).ok()
}

fn parse_range(repo: &Path, range: &str) -> anyhow::Result<(Option<CommitSha>, CommitSha)> {
    if let Some((from_str, to_str)) = range.split_once("..") {
        let from = git_resolve(repo, from_str)?;
        let to = git_resolve(repo, to_str)?;
        Ok((Some(from), to))
    } else {
        let sha = git_resolve(repo, range)?;
        let parent = git_parent(repo, &sha);
        let head = git_rev_parse_head(repo).ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;
        Ok((parent, head))
    }
}

fn git_resolve(repo: &Path, rev: &str) -> anyhow::Result<CommitSha> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", rev])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("cannot resolve `{rev}`");
    }
    CommitSha::parse(String::from_utf8_lossy(&output.stdout).trim())
        .map_err(|e| anyhow::anyhow!("invalid SHA for `{rev}`: {e}"))
}

fn git_parent(repo: &Path, sha: &CommitSha) -> Option<CommitSha> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", &format!("{}^", sha.as_str())])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    CommitSha::parse(String::from_utf8_lossy(&output.stdout).trim()).ok()
}

/// Find the parent of the earliest plan file introduction across
/// the selected plans. Uses `git log --diff-filter=A` to find when
/// `.clank/plans/<stem>.md` was first committed.
fn earliest_plan_intro_parent(repo: &Path, keys: &[PlanKey]) -> Option<CommitSha> {
    let mut earliest: Option<(String, i64)> = None;
    for key in keys {
        let plan_path = format!(".clank/plans/{}.md", key.as_str());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "log",
                "--diff-filter=A",
                "--format=%H %at",
                "--follow",
                "--",
                &plan_path,
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // git log outputs newest-first. First line = most recent
        // introduction (handles reintroduced plans correctly).
        if let Some(line) = stdout.lines().next() {
            let mut parts = line.splitn(2, ' ');
            let sha_str = parts.next().unwrap_or("").trim().to_string();
            let ts: i64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
            earliest = Some(match earliest {
                Some((_prev_sha, prev_ts)) if ts < prev_ts => (sha_str, ts),
                Some(prev) => prev,
                None => (sha_str, ts),
            });
        }
    }
    let (intro_sha_str, _) = earliest?;
    let intro_sha = CommitSha::parse(&intro_sha_str).ok()?;
    // Return the parent (exclusive lower bound). None if root commit.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", &format!("{}^", intro_sha.as_str())])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    CommitSha::parse(String::from_utf8_lossy(&output.stdout).trim()).ok()
}

/// Resolve plan filter using git only (no fold state needed).
fn resolve_plan_filter(
    repo: &Path,
    basename: &str,
    all: bool,
    plan_arg: Option<&str>,
) -> anyhow::Result<Option<Vec<PlanKey>>> {
    if all {
        return Ok(None);
    }
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
        return Ok(Some(vec![key]));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-tree", "--name-only", "HEAD", ".clank/plans/"])
        .output();
    let plans: Vec<PlanKey> = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let stem = l.strip_prefix(".clank/plans/")?.strip_suffix(".md")?;
                PlanKey::parse(stem).ok()
            })
            .collect(),
        _ => Vec::new(),
    };
    match plans.as_slice() {
        [one] => Ok(Some(vec![one.clone()])),
        _ => Ok(None),
    }
}

struct Review {
    author: String,
    verdict: Verdict,
}

fn collect_reviews(
    repo: &Path,
    plan_keys: &[PlanKey],
    reviewable_shas: &[CommitSha],
) -> std::collections::BTreeMap<(String, String), Vec<Review>> {
    let mut reviews = std::collections::BTreeMap::new();
    for key in plan_keys {
        if let Ok(fv) = scan_feedback(repo, key, reviewable_shas) {
            for cf in &fv.per_commit {
                for (author, entry) in &cf.entries {
                    reviews
                        .entry((key.as_str().to_string(), cf.sha.as_str().to_string()))
                        .or_insert_with(Vec::new)
                        .push(Review {
                            author: author.as_str().to_string(),
                            verdict: entry.verdict,
                        });
                }
            }
        }
    }
    reviews
}

fn short(sha: &CommitSha) -> &str {
    &sha.as_str()[..sha.as_str().len().min(7)]
}

fn commit_subject(repo: &Path, sha: &CommitSha) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%s", sha.as_str()])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

fn kind_label(touched_plan: bool, touched_code: bool) -> &'static str {
    match (touched_plan, touched_code) {
        (true, true) => "plan+code",
        (true, false) => "plan",
        (false, true) => "code",
        (false, false) => "",
    }
}

const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn commit_info(repo: &Path, sha: &CommitSha) -> (String, String, String) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%an <%ae>%n%ai%n%B", sha.as_str()])
        .output();
    match output {
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

fn print_human(
    events: &[&LogEvent],
    repo: &Path,
    plan_keys: &[PlanKey],
    reviewable_shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, plan_keys, reviewable_shas);
    let c = use_color();

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
                        "{YELLOW}finalize {}{RESET} {CYAN}(plan: {}){RESET}",
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
        };
        let (author, date, body) = commit_info(repo, sha);
        if c {
            println!(
                "{YELLOW}commit {}{RESET} {CYAN}(plan: {}, {kind}){RESET}",
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
        print_reviews_for(&reviews, plan, sha, c);
    }
    Ok(())
}

fn print_oneline(
    events: &[&LogEvent],
    repo: &Path,
    plan_keys: &[PlanKey],
    reviewable_shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, plan_keys, reviewable_shas);
    let c = use_color();

    for event in events {
        let (plan, sha, subj_override) = match event {
            LogEvent::PlanIntro { plan, sha, .. } | LogEvent::PlanCommit { plan, sha, .. } => {
                (plan, sha, None)
            }
            LogEvent::PlanFinalized { plan, sha, .. } => {
                (plan, sha, Some(format!("Finalize {}", plan.as_str())))
            }
            LogEvent::PlanDeleted { plan, sha, .. } => {
                (plan, sha, Some(format!("Delete {}", plan.as_str())))
            }
        };
        let subj = subj_override.unwrap_or_else(|| commit_subject(repo, sha));
        let review_summary = review_oneline_summary(&reviews, plan, sha, c);
        if c {
            println!("{YELLOW}{}{RESET} {subj}{review_summary}", short(sha));
        } else {
            println!("{} {subj}{review_summary}", short(sha));
        }
    }
    Ok(())
}

fn review_oneline_summary(
    reviews: &std::collections::BTreeMap<(String, String), Vec<Review>>,
    plan: &PlanKey,
    sha: &CommitSha,
    color: bool,
) -> String {
    let key = (plan.as_str().to_string(), sha.as_str().to_string());
    let Some(rs) = reviews.get(&key) else {
        return String::new();
    };
    let parts: Vec<String> = rs
        .iter()
        .map(|r| {
            let (mark, col) = match r.verdict {
                Verdict::Approve => ("✓", GREEN),
                Verdict::RequestChanges => ("✗", RED),
                Verdict::Unmarked => ("?", RESET),
            };
            if color {
                format!("{col}{mark} {}{RESET}", r.author)
            } else {
                format!("{mark} {}", r.author)
            }
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("  {}", parts.join(" "))
    }
}

fn print_reviews_for(
    reviews: &std::collections::BTreeMap<(String, String), Vec<Review>>,
    plan: &PlanKey,
    sha: &CommitSha,
    color: bool,
) {
    let key = (plan.as_str().to_string(), sha.as_str().to_string());
    if let Some(rs) = reviews.get(&key) {
        for r in rs {
            let (mark, col) = match r.verdict {
                Verdict::Approve => ("✓", GREEN),
                Verdict::RequestChanges => ("✗", RED),
                Verdict::Unmarked => ("?", RESET),
            };
            if color {
                println!("  {col}{mark} {} {}{RESET}", r.author, r.verdict);
            } else {
                println!("  {mark} {} {}", r.author, r.verdict);
            }
        }
    }
}

fn print_json(
    events: &[&LogEvent],
    repo: &Path,
    plan_keys: &[PlanKey],
    reviewable_shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, plan_keys, reviewable_shas);
    let mut json_events: Vec<serde_json::Value> = Vec::new();

    for event in events {
        let obj = match event {
            LogEvent::PlanIntro { plan, sha, ts } => serde_json::json!({
                "kind": "intro",
                "plan": plan.as_str(),
                "sha": sha.as_str(),
                "ts": ts,
                "subject": commit_subject(repo, sha),
            }),
            LogEvent::PlanCommit {
                plan,
                sha,
                ts,
                touched_plan,
                touched_code,
            } => serde_json::json!({
                "kind": "commit",
                "plan": plan.as_str(),
                "sha": sha.as_str(),
                "ts": ts,
                "touched_plan": touched_plan,
                "touched_code": touched_code,
                "subject": commit_subject(repo, sha),
            }),
            LogEvent::PlanFinalized { plan, sha, ts } => serde_json::json!({
                "kind": "finalized",
                "plan": plan.as_str(),
                "sha": sha.as_str(),
                "ts": ts,
            }),
            LogEvent::PlanDeleted { plan, sha, ts } => serde_json::json!({
                "kind": "deleted",
                "plan": plan.as_str(),
                "sha": sha.as_str(),
                "ts": ts,
            }),
        };
        json_events.push(obj);

        let (plan_str, sha_str) = match event {
            LogEvent::PlanIntro { plan, sha, .. }
            | LogEvent::PlanCommit { plan, sha, .. }
            | LogEvent::PlanFinalized { plan, sha, .. }
            | LogEvent::PlanDeleted { plan, sha, .. } => {
                (plan.as_str().to_string(), sha.as_str().to_string())
            }
        };
        if let Some(rs) = reviews.get(&(plan_str.clone(), sha_str.clone())) {
            for r in rs {
                json_events.push(serde_json::json!({
                    "kind": "review",
                    "plan": plan_str,
                    "sha": sha_str,
                    "author": r.author,
                    "verdict": r.verdict,
                }));
            }
        }
    }

    println!("{}", serde_json::to_string_pretty(&json_events)?);
    Ok(())
}
