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

    // Phase 1: fast cached rebuild to find the plan's intro SHA.
    let fast_state =
        crate::rebuild::rebuild_repo_with_policy(&repo, crate::rebuild::CachePolicy::Use)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo: {e}"))?;

    let head = fast_state
        .head
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("repo has no HEAD"))?;

    let plan_filter =
        resolve_plan_filter(&fast_state.fold, &basename, args.all, args.plan.as_deref())?;

    let intro = find_earliest_intro(&fast_state, &plan_filter);
    let from: Option<CommitSha> = intro.as_ref().and_then(|sha| git_parent_of(&repo, sha));

    // Phase 2: rebuild_from to get log events for the range.
    let (_state, log_events) = crate::rebuild::rebuild_from(&repo, from.as_ref(), head)
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

fn find_earliest_intro(
    state: &crate::repo_state::RepoState,
    plan_filter: &Option<Vec<PlanKey>>,
) -> Option<CommitSha> {
    let keys: Vec<&PlanKey> = match plan_filter {
        Some(keys) => keys.iter().collect(),
        None => state
            .fold
            .plans
            .keys()
            .chain(state.fold.finished_plans.iter().map(|fp| &fp.plan))
            .collect(),
    };

    let mut earliest: Option<CommitSha> = None;
    for key in &keys {
        if let Some(ps) = state.fold.plans.get(*key) {
            if let Some(first) = ps.commits.first() {
                earliest = Some(earliest.map_or(first.sha.clone(), |prev| {
                    if first.ts < state.fold.plans.get(*key).unwrap().commits[0].ts {
                        first.sha.clone()
                    } else {
                        prev
                    }
                }));
            }
        }
        for fp in &state.fold.finished_plans {
            if &fp.plan == *key {
                earliest = earliest.or(Some(fp.intro.clone()));
            }
        }
    }
    earliest
}

fn git_parent_of(repo: &Path, sha: &CommitSha) -> Option<CommitSha> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", &format!("{}^", sha.as_str())])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    CommitSha::parse(s.trim()).ok()
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
