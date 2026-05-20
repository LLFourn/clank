//! Local plan resolution for the operator CLI. Operates against a
//! freshly-folded [`RepoState`] — no daemon required.
//!
//! Rules:
//!
//! - Explicit arg: parse `<basename>/<stem>.md`, `<stem>.md`, or
//!   `<stem>`. If the arg includes a basename, it must match the
//!   target repo. Resulting stem must exist in `state.plans`.
//! - No arg: filter `state.plans` to active visible plans (matches
//!   the visibility/lifecycle rules used by `list_plans`). Exactly
//!   one wins; zero or many is an error with candidate ids.

use crate::lifecycle::PlanKey;
use crate::repo_state::RepoState;

/// Resolve a CLI plan argument against a local `RepoState`.
///
/// Returns the matched `PlanKey`. The caller already knows the repo
/// basename (from `super::repo_basename(repo_root)`) and is
/// responsible for printing user-facing errors.
pub fn resolve_plan(
    state: &RepoState,
    expected_basename: &str,
    plan_arg: Option<&str>,
) -> anyhow::Result<PlanKey> {
    if let Some(raw) = plan_arg.map(str::trim).filter(|s| !s.is_empty()) {
        let stem = parse_arg(raw, expected_basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;
        if !state.plans.contains_key(&key) {
            anyhow::bail!(
                "plan `{}/{}.md` not found in repo. candidates: {}",
                expected_basename,
                stem,
                candidate_summary(state, expected_basename),
            );
        }
        return Ok(key);
    }
    infer_single_visible_active(state, expected_basename)
}

/// Visible active plan iterator. A plan is eligible for inference
/// when it isn't frozen and its worktree file is present (matches
/// `Plan::is_visible` semantics — see crates/trinity-core/src/model.rs).
fn visible_active_plans<'a>(
    state: &'a RepoState,
) -> std::io::Result<Vec<&'a trinity_core::model::Plan>> {
    let mut out = Vec::new();
    for plan in state.plans.values() {
        if plan.is_frozen() {
            continue;
        }
        let status = crate::responses::compute_plan_worktree_status_parts(
            &state.root,
            &plan.plan_path,
            &plan.body_hash,
        )?;
        if plan.is_visible(status) {
            out.push(plan);
        }
    }
    Ok(out)
}

fn infer_single_visible_active(
    state: &RepoState,
    expected_basename: &str,
) -> anyhow::Result<PlanKey> {
    let visible = visible_active_plans(state)?;
    match visible.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => anyhow::bail!(
            "no active in-flight plan to infer; pass a plan id explicitly. repo: `{expected_basename}`",
        ),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|p| format!("{}/{}.md", expected_basename, p.id.as_str()))
                .collect();
            anyhow::bail!(
                "ambiguous: {} active plans in `{expected_basename}`; pass one explicitly. candidates: {}",
                many.len(),
                names.join(", "),
            )
        }
    }
}

fn candidate_summary(state: &RepoState, basename: &str) -> String {
    let names: Vec<String> = state
        .plans
        .keys()
        .map(|k| format!("{basename}/{}.md", k.as_str()))
        .collect();
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

/// Strip a wire-form plan argument down to its stem. Accepts
/// `<basename>/<stem>.md`, `<stem>.md`, or `<stem>`. Rejects a
/// basename that doesn't match the target repo.
fn parse_arg(raw: &str, expected_basename: &str) -> anyhow::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_only(raw: &str, basename: &str) -> anyhow::Result<String> {
        parse_arg(raw, basename)
    }

    #[test]
    fn parses_bare_stem() {
        assert_eq!(parse_only("foo", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn parses_stem_dot_md() {
        assert_eq!(parse_only("foo.md", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn parses_full_plan_id() {
        assert_eq!(parse_only("myrepo/foo.md", "myrepo").unwrap(), "foo");
    }

    #[test]
    fn rejects_basename_mismatch() {
        let err = parse_only("other/foo.md", "myrepo").unwrap_err();
        assert!(err.to_string().contains("names repo `other`"));
    }
}
