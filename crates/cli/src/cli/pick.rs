//! `clank pick <plan>... --from <committish>` — COPY plans (their
//! commits + plan files) from another branch onto the current one.
//! Plan: clank-pick.
//!
//! Pull model: run it where you want the plans to land. The source is
//! NEVER modified — no protective refs, no records, nothing to roll
//! back. Mechanics are cherry-pick (diff-based 3-way), not the rewrite
//! engine's tree-preserving replay: replaying trees is only sound onto
//! the same base; across bases it would clobber the target's content.
//!
//! A picked active plan enters this branch's review cycle immediately
//! (its intro carries `plans/<stem>.md`); reviews RESET by design — the
//! copies are new shas on a new base, same rule as unshelve.

use std::collections::BTreeSet;

use super::PickArgs;
use crate::lifecycle::{CommitSha, PlanKey};
use crate::rebuild::CachePolicy;

pub async fn run(args: PickArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;

    // Validate + dedupe the stems, preserving argument order for messages.
    let mut keys: Vec<PlanKey> = Vec::new();
    for raw in &args.plans {
        let key =
            PlanKey::parse(raw).map_err(|e| anyhow::anyhow!("invalid plan name `{raw}`: {e}"))?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    // Target state: collision checks + merge-base for the dependency warn.
    let target = crate::rebuild::rebuild_repo_with_policy(&repo, CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
    for key in &keys {
        let stem = key.as_str();
        if target.fold.plans.contains_key(key) {
            anyhow::bail!(
                "plan `{stem}` is already ACTIVE on this branch — finish or purge it \
                 before picking a copy from elsewhere."
            );
        }
        if target.fold.finished_plans.iter().any(|f| &f.plan == key) {
            anyhow::bail!(
                "plan `{stem}` is already FINISHED on this branch — a second copy \
                 would collide. Purge it first if you really want the other one."
            );
        }
    }

    // Source: any committish; folded at ITS tip (never checked out).
    let tip = crate::git_io::resolve_commit(&repo, &args.from)
        .ok_or_else(|| anyhow::anyhow!("`{}` is not a known branch/committish", args.from))?;
    let source = crate::preview::fold_at_tip(&repo, &tip)
        .await
        .map_err(|e| anyhow::anyhow!("folding `{}`: {e}", args.from))?;

    // Each plan's OWN commits on the source (active timeline, or the
    // finished-plan natives re-fold the rewrite preview uses).
    let mut natives_by_plan: Vec<(PlanKey, BTreeSet<CommitSha>)> = Vec::new();
    for key in &keys {
        let stem = key.as_str();
        let natives: BTreeSet<CommitSha> = if let Some(ps) = source.fold.plans.get(key) {
            ps.commits.iter().map(|e| e.sha.clone()).collect()
        } else if let Some(fp) = source.fold.finished_plans.iter().find(|f| &f.plan == key) {
            let end = fp.finalized_at.clone();
            crate::preview::re_fold_finished_plan_natives(&repo, key, &end)
                .await
                .map_err(|e| anyhow::anyhow!("re-deriving `{stem}`'s commits: {e}"))?
        } else {
            anyhow::bail!(
                "plan `{stem}` not found on `{}` (active or finished)",
                args.from
            );
        };
        if natives.is_empty() {
            anyhow::bail!("plan `{stem}` has no commits on `{}`", args.from);
        }
        natives_by_plan.push((key.clone(), natives));
    }

    // Source-order walk: positions decide replay order AND the spans for
    // the interleaved-foreign refusal.
    let git = crate::git_io::open(&repo)?;
    let metas = git.first_parent_commits_to(&tip)?;
    let pos_of = |sha: &CommitSha| metas.iter().position(|m| &m.sha == sha);
    let union: BTreeSet<&CommitSha> = natives_by_plan.iter().flat_map(|(_, n)| n).collect();

    // Interleaved-foreign check, per plan: inside the plan's own span,
    // every commit must belong to SOME picked plan — cherry-pick replays
    // diffs, so a foreign commit inside the span is likely load-bearing
    // context we'd silently skip. Name the offenders (same rule + spirit
    // as the squash guard). Picking two mutually-interleaved plans
    // TOGETHER is fine: each covers the other's "foreign" commits.
    for (key, natives) in &natives_by_plan {
        let positions: Vec<usize> = natives.iter().filter_map(&pos_of).collect();
        let (Some(&lo), Some(&hi)) = (positions.iter().min(), positions.iter().max()) else {
            anyhow::bail!(
                "plan `{}`'s commits are not on `{}`'s first-parent history",
                key.as_str(),
                args.from
            );
        };
        let offenders: Vec<String> = metas[lo..=hi]
            .iter()
            .filter(|m| !union.contains(&m.sha))
            .map(|m| format!("{} {}", &m.sha.as_str()[..7], m.subject))
            .collect();
        if !offenders.is_empty() {
            anyhow::bail!(
                "cannot pick `{}`: {} foreign commit(s) interleaved within its range \
                 on `{}`:\n    {}\n  Pick the interleaved plan(s) together with it, or \
                 untangle them on the source first.",
                key.as_str(),
                offenders.len(),
                args.from,
                offenders.join("\n    "),
            );
        }
    }

    // The pick list: the union, in source (walk) order.
    let ordered: Vec<&crate::git_io::CommitMeta> =
        metas.iter().filter(|m| union.contains(&m.sha)).collect();

    // Dependency warning: unpicked source plans introduced between the
    // merge-base and the picked commits — textual dependence on what's
    // left behind is the likely conflict source. Warn, never refuse.
    if let Some(head) = target.head.as_ref()
        && let Ok(Some(mb)) = crate::git_io::merge_base_at(&repo, head, &tip)
        && let Some(mb_pos) = pos_of(&mb)
    {
        let top = ordered.last().and_then(|m| pos_of(&m.sha)).unwrap_or(0);
        let picked: BTreeSet<&str> = keys.iter().map(|k| k.as_str()).collect();
        let mut below: Vec<&str> = Vec::new();
        for (k, ps) in &source.fold.plans {
            if picked.contains(k.as_str()) {
                continue;
            }
            if let Some(first) = ps.commits.first()
                && let Some(p) = pos_of(&first.sha)
                && p > mb_pos
                && p < top
            {
                below.push(k.as_str());
            }
        }
        for fp in &source.fold.finished_plans {
            if picked.contains(fp.plan.as_str()) {
                continue;
            }
            if let Some(p) = pos_of(&fp.intro)
                && p > mb_pos
                && p < top
            {
                below.push(fp.plan.as_str());
            }
        }
        if !below.is_empty() {
            below.sort_unstable();
            below.dedup();
            eprintln!(
                "warning: unpicked plan(s) sit under the picked commits on `{}`: {} — \
                 the picks may textually depend on them and conflict.",
                args.from,
                below.join(", "),
            );
        }
    }

    // Dirty-tree refusal BEFORE the --dry branch: the preview must report
    // the same refusal the live run would hit (dry equals execute — the
    // same rule finish enforces; ruthless aeb97ff).
    if crate::cli::rewrite::working_tree_dirty(&repo)? {
        anyhow::bail!("working tree dirty; commit or stash first");
    }

    if args.dry {
        println!(
            "# clank pick --dry ({} plan(s) from `{}`)",
            keys.len(),
            args.from
        );
        for (key, natives) in &natives_by_plan {
            println!("# plan: {} ({} commit(s))", key.as_str(), natives.len());
        }
        println!("# would cherry-pick, in source order:");
        for m in &ordered {
            println!("pick  {} {}", &m.sha.as_str()[..7], m.subject);
        }
        println!("# (--dry: no commits, no refs updated)");
        return Ok(());
    }

    let total = ordered.len();
    for (i, m) in ordered.iter().enumerate() {
        if !crate::git_plumbing::cherry_pick(&repo, m.sha.as_str())? {
            anyhow::bail!(
                "cherry-pick of {} ({}) conflicted after {i} of {total} commit(s) landed. \
                 Resolve and `git cherry-pick --continue`, or `git cherry-pick --abort` \
                 to back out this pick. The source `{}` is untouched either way.",
                &m.sha.as_str()[..7],
                m.subject,
                args.from,
            );
        }
    }
    println!(
        "picked {} plan(s) from `{}`: {} commit(s) replayed onto HEAD (reviews reset)",
        keys.len(),
        args.from,
        total,
    );
    Ok(())
}
