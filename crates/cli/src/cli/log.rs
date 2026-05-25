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

    let state = crate::rebuild::rebuild_repo_with_log(&repo)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo: {e}"))?;

    let plan_filter = resolve_plan_filter(&state.fold, &basename, args.all, args.plan.as_deref())?;

    let filtered_events: Vec<&LogEvent> = state
        .log_events
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
    } else {
        print_human(&filtered_events, &repo, &plan_keys, &reviewable_shas)?;
    }
    Ok(())
}

fn resolve_plan_filter(
    fold: &clank_core::repo_state::RepoState,
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
    let actives: Vec<PlanKey> = fold.plans.keys().cloned().collect();
    match actives.as_slice() {
        [one] => Ok(Some(vec![one.clone()])),
        [] => {
            if let Some(fp) = fold.finished_plans.last() {
                Ok(Some(vec![fp.plan.clone()]))
            } else {
                Ok(None)
            }
        }
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

fn print_human(
    events: &[&LogEvent],
    repo: &Path,
    plan_keys: &[PlanKey],
    reviewable_shas: &[CommitSha],
) -> anyhow::Result<()> {
    let reviews = collect_reviews(repo, plan_keys, reviewable_shas);

    for event in events {
        match event {
            LogEvent::PlanIntro { plan, sha, .. } => {
                let subj = commit_subject(repo, sha);
                let kind = "plan";
                println!("{:<40} {}  {kind}", subj, short(sha));
                print_reviews_for(&reviews, plan, sha);
            }
            LogEvent::PlanCommit {
                plan,
                sha,
                touched_plan,
                touched_code,
                ..
            } => {
                let subj = commit_subject(repo, sha);
                let kind = kind_label(*touched_plan, *touched_code);
                println!("{:<40} {}  {kind}", subj, short(sha));
                print_reviews_for(&reviews, plan, sha);
            }
            LogEvent::PlanFinalized { plan, sha, .. } => {
                println!("Finalize {:<31} {}", plan.as_str(), short(sha));
            }
            LogEvent::PlanDeleted { plan, sha, .. } => {
                println!("Delete {:<33} {}", plan.as_str(), short(sha));
            }
        }
    }
    Ok(())
}

fn print_reviews_for(
    reviews: &std::collections::BTreeMap<(String, String), Vec<Review>>,
    plan: &PlanKey,
    sha: &CommitSha,
) {
    let key = (plan.as_str().to_string(), sha.as_str().to_string());
    if let Some(rs) = reviews.get(&key) {
        for r in rs {
            let mark = match r.verdict {
                Verdict::Approve => "✓",
                Verdict::RequestChanges => "✗",
                Verdict::Unmarked => "?",
            };
            println!("  {mark} {} {}", r.author, r.verdict);
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
        let mut obj = match event {
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

        let plan_str = match event {
            LogEvent::PlanIntro { plan, .. }
            | LogEvent::PlanCommit { plan, .. }
            | LogEvent::PlanFinalized { plan, .. }
            | LogEvent::PlanDeleted { plan, .. } => plan.as_str().to_string(),
        };
        let sha_str = match event {
            LogEvent::PlanIntro { sha, .. }
            | LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanFinalized { sha, .. }
            | LogEvent::PlanDeleted { sha, .. } => sha.as_str().to_string(),
        };

        let key = (plan_str, sha_str);
        if let Some(rs) = reviews.get(&key) {
            let review_json: Vec<serde_json::Value> = rs
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "author": r.author,
                        "verdict": r.verdict,
                    })
                })
                .collect();
            obj.as_object_mut()
                .unwrap()
                .insert("reviews".into(), serde_json::json!(review_json));
        }

        json_events.push(obj);
    }

    println!("{}", serde_json::to_string_pretty(&json_events)?);
    Ok(())
}
