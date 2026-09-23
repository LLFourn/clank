//! Local plan resolution for the operator CLI. Operates against a
//! freshly-folded [`RepoState`] — no daemon required.

use std::path::Path;

use crate::lifecycle::{CommitSha, PlanKey};
use crate::repo_state::RepoState;

/// Resolve a CLI plan argument against a local `RepoState`.
///
/// - Explicit arg: parse `<basename>/<stem>.md`, `<stem>.md`, or
///   `<stem>`. If the arg includes a basename, it must match the
///   target repo. Resulting stem must exist in `state.fold.plans`.
/// - No arg: filter `state.fold.plans` to active visible plans
///   (worktree file present, not finished). Exactly one wins; zero
///   or many is an error with candidate ids.
pub fn resolve_plan(
    state: &RepoState,
    expected_basename: &str,
    plan_arg: Option<&str>,
) -> anyhow::Result<PlanKey> {
    if let Some(raw) = plan_arg.map(str::trim).filter(|s| !s.is_empty()) {
        let stem = parse_arg(raw, expected_basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
        let known =
            state.fold.plans.contains_key(&key) || state.fold.finished_record(&key).is_some();
        if !known {
            anyhow::bail!(
                "plan `{}/{}.md` not found in repo. candidates: {}",
                expected_basename,
                stem,
                candidate_summary(state, expected_basename, &stem),
            );
        }
        return Ok(key);
    }
    infer_single_visible_active(state, expected_basename)
}

fn infer_single_visible_active(
    state: &RepoState,
    expected_basename: &str,
) -> anyhow::Result<PlanKey> {
    let visible: Vec<&PlanKey> = state
        .fold
        .plans
        .keys()
        .filter(|key| {
            let path = state
                .root
                .join(crate::init_facts::plan_md_rel(key.as_str()));
            path.exists()
        })
        .collect();
    match visible.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!(
            "no active in-flight plan to infer; pass a plan id explicitly. repo: `{expected_basename}`",
        ),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|k| format!("{}/{}.md", expected_basename, k.as_str()))
                .collect();
            anyhow::bail!(
                "ambiguous: {} active plans in `{expected_basename}`; pass one explicitly. candidates: {}",
                many.len(),
                names.join(", "),
            )
        }
    }
}

/// How many finished plans a "not found" diagnostic names. A repo
/// accumulates hundreds of them; naming all of them buries the two or
/// three the reader could have meant (ruthless 863f359).
const FINISHED_NAMED: usize = 5;

/// Candidates for a "plan not found": every active plan, and the
/// finished plans that resemble what was asked for — or, when none
/// does, the most recently finished, with the rest counted.
fn candidate_summary(state: &RepoState, basename: &str, asked: &str) -> String {
    let mut names: Vec<String> = state
        .fold
        .plans
        .keys()
        .map(|k| format!("{basename}/{}.md", k.as_str()))
        .collect();

    // `finished_plans` is the fold's ordered log and a plan can be
    // re-finalized, so walk it newest-first and keep each name once.
    let mut finished: Vec<&str> = Vec::new();
    for fp in state.fold.finished_plans.iter().rev() {
        let stem = fp.plan.as_str();
        if !finished.contains(&stem) {
            finished.push(stem);
        }
    }
    let resembling: Vec<&str> = finished
        .iter()
        .copied()
        .filter(|stem| stem.contains(asked) || asked.contains(*stem))
        .collect();
    let named: Vec<&str> = if resembling.is_empty() {
        finished.iter().copied().take(FINISHED_NAMED).collect()
    } else {
        resembling.into_iter().take(FINISHED_NAMED).collect()
    };
    for stem in &named {
        names.push(format!("{basename}/{stem}.md (finished)"));
    }
    let unnamed = finished.len() - named.len();
    if unnamed > 0 {
        names.push(format!("and {unnamed} more finished"));
    }

    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

/// Parse a plan-arg in any of the accepted shapes: bare `<stem>`,
/// `<stem>.md`, the repo-relative `.clank/plans/<stem>.md`, or the
/// full `<basename>/<stem>.md`. Returns the bare stem; rejects a
/// mismatched basename or a path outside `.clank/plans/`.
pub fn parse_arg(raw: &str, expected_basename: &str) -> anyhow::Result<String> {
    if let Some(rest) = raw.strip_prefix(".clank/plans/") {
        if rest.contains('/') {
            anyhow::bail!("plan path `{raw}` must not contain nested directories");
        }
        return Ok(rest.trim_end_matches(".md").to_string());
    }
    if let Some((basename, rest)) = raw.split_once('/') {
        if basename != expected_basename {
            anyhow::bail!(
                "plan id `{raw}` names repo `{basename}` but we're operating on `{expected_basename}` \
                 — pass `--repo` to override, or invoke from inside the right repo",
            );
        }
        return Ok(rest.trim_end_matches(".md").to_string());
    }
    Ok(raw.trim_end_matches(".md").to_string())
}

/// Plan-attributed commits for a plan key, in chronological order.
///
/// Source of truth differs by plan status:
/// - **Active**: `state.fold.plans[plan_key].commits` — the fold's
///   per-plan timeline. Attribution is intrinsic (the fold only
///   adds plan-attributed events to a plan's timeline).
/// - **Finished**: re-derive via [`crate::preview::build_rewrite_preview`]
///   and **FILTER `!c.foreign`**. The preview is range-based
///   (`[intro_sha, head_sha]`); it includes interleaved non-plan
///   commits marked `foreign: true` per `clank_core::api::RewriteCommit`.
///   The foreign filter is load-bearing — codex caught (twice)
///   that omitting it reintroduces the interleaved-plans bug
///   (cba9249 active-path; 0859200 finished-path).
///
/// Returns an error if the plan key is in neither `state.fold.plans`
/// nor `state.fold.finished_plans` — matches `resolve_plan`'s
/// "plan not found" diagnostic shape.
pub async fn commits_for_plan(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
) -> anyhow::Result<Vec<CommitSha>> {
    Ok(plan_history(repo_root, state, plan_key).await?.commits)
}

/// A plan's commits together with the lifecycle they came from, so a
/// reader cannot pair one with the other's facts.
pub struct PlanHistory {
    /// `None` while the plan is active. A plan re-introduced after a
    /// finish is in BOTH sets, and its commits are the new cycle's —
    /// so the old finalize is not this history's (codex a7f47e4).
    pub finished: Option<clank_core::repo_state::FinishedPlan>,
    pub commits: Vec<CommitSha>,
}

/// See [`commits_for_plan`] for where each path's commits come from.
pub async fn plan_history(
    repo_root: &Path,
    state: &RepoState,
    plan_key: &PlanKey,
) -> anyhow::Result<PlanHistory> {
    // Active path: fold's per-plan timeline is already
    // plan-attributed. Cheap — no preview rebuild needed.
    if let Some(ps) = state.fold.plans.get(plan_key) {
        return Ok(PlanHistory {
            finished: None,
            commits: ps.commits.iter().map(|e| e.sha.clone()).collect(),
        });
    }
    // Finished path: preview rebuild + foreign filter. The record is
    // the one the preview itself walks — `finished_record` is the
    // single chooser.
    let Some(record) = state.fold.finished_record(plan_key) else {
        anyhow::bail!(
            "plan `{}` not found in active or finished sets",
            plan_key.as_str()
        );
    };
    let preview = crate::preview::build_rewrite_preview(repo_root, state, plan_key, true)
        .await
        .map_err(|e| anyhow::anyhow!("rewrite preview failed for `{}`: {e}", plan_key.as_str()))?;
    Ok(PlanHistory {
        finished: Some(record.clone()),
        commits: preview
            .commits
            .iter()
            .filter(|c| !c.foreign)
            .map(|c| c.sha.clone())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_stem() {
        assert_eq!(parse_arg("foo", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn parses_stem_dot_md() {
        assert_eq!(parse_arg("foo.md", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn parses_full_plan_id() {
        assert_eq!(parse_arg("myrepo/foo.md", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn rejects_basename_mismatch() {
        let err = parse_arg("other/foo.md", "myrepo").unwrap_err();
        assert!(err.to_string().contains("names repo `other`"));
    }

    #[test]
    fn parses_repo_relative_plan_path() {
        assert_eq!(parse_arg(".clank/plans/foo.md", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn rejects_nested_under_clank_plans() {
        assert!(parse_arg(".clank/plans/team/foo.md", "myrepo").is_err());
    }

    fn state_with(active: &[&str], finished: &[&str]) -> RepoState {
        let mut state = RepoState::empty(std::path::PathBuf::from("/nowhere"));
        for stem in active {
            state
                .fold
                .plans
                .insert(PlanKey::parse(stem).unwrap(), Default::default());
        }
        let sha = CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
        for stem in finished {
            state
                .fold
                .finished_plans
                .push(clank_core::repo_state::FinishedPlan {
                    plan: PlanKey::parse(stem).unwrap(),
                    intro: sha.clone(),
                    finalized_at: sha.clone(),
                });
        }
        state
    }

    #[test]
    fn an_unknown_plan_names_the_active_ones_and_counts_the_finished() {
        let finished: Vec<String> = (0..20).map(|i| format!("old-plan-{i:02}")).collect();
        let refs: Vec<&str> = finished.iter().map(String::as_str).collect();
        let state = state_with(&["live-one"], &refs);
        let err = resolve_plan(&state, "repo", Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("repo/live-one.md"), "{err}");
        assert!(err.contains("repo/old-plan-19.md (finished)"), "{err}");
        assert!(
            !err.contains("old-plan-00"),
            "the oldest finished are not named: {err}"
        );
        assert!(err.contains("and 15 more finished"), "{err}");
    }

    #[test]
    fn a_replanned_name_is_counted_once() {
        // A plan can be re-introduced and re-finalized, so the log
        // carries its name twice.
        let state = state_with(&[], &["twice", "twice", "other"]);
        let err = resolve_plan(&state, "repo", Some("nope"))
            .unwrap_err()
            .to_string();
        assert_eq!(err.matches("repo/twice.md").count(), 1, "{err}");
        assert!(
            !err.contains("more finished"),
            "two names, both named: {err}"
        );
    }

    #[test]
    fn a_near_miss_names_the_finished_plan_it_resembles() {
        let state = state_with(
            &[],
            &[
                "the-cloud-is-always-the-cloud",
                "something-else",
                "and-another",
                "a-fourth",
                "a-fifth",
                "a-sixth",
            ],
        );
        let err = resolve_plan(&state, "repo", Some("cloud-is-always"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("repo/the-cloud-is-always-the-cloud.md (finished)"),
            "{err}"
        );
        assert!(
            !err.contains("something-else"),
            "only what it resembles: {err}"
        );
        assert!(err.contains("and 5 more finished"), "{err}");
    }
}
