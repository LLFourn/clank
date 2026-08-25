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

    let collapse = args.squash || args.purge;

    // Collapse modes process ONE PLAN AT A TIME, so mutually-interleaved
    // plans (which the plain pick handles by replaying the union in
    // source order) would be silently REORDERED — each plan's later
    // commits would 3-way merge against the other plan's full state
    // instead of their source context. Refuse instead of reordering.
    if collapse && keys.len() > 1 {
        let spans: Vec<(usize, usize, &PlanKey)> = natives_by_plan
            .iter()
            .map(|(k, n)| {
                let ps: Vec<usize> = n.iter().filter_map(&pos_of).collect();
                (
                    ps.iter().copied().min().unwrap_or(0),
                    ps.iter().copied().max().unwrap_or(0),
                    k,
                )
            })
            .collect();
        for (i, a) in spans.iter().enumerate() {
            for b in &spans[i + 1..] {
                if a.0 <= b.1 && b.0 <= a.1 {
                    anyhow::bail!(
                        "plans `{}` and `{}` interleave on `{}` — --squash/--purge \
                         collapse one plan at a time and would reorder them. Pick \
                         them plain, or untangle them on the source first.",
                        a.2.as_str(),
                        b.2.as_str(),
                        args.from,
                    );
                }
            }
        }
    }

    // The landing shape, computed ONCE for --dry and execute alike
    // (one-computation rule): each plan in SOURCE order with its
    // ordered commits and, for --squash, the composed message.
    let mut landings: Vec<(PlanKey, Vec<&crate::git_io::CommitMeta>, Option<String>)> = Vec::new();
    let mut plans_sorted: Vec<&(PlanKey, BTreeSet<CommitSha>)> = natives_by_plan.iter().collect();
    plans_sorted.sort_by_key(|(_, n)| n.iter().filter_map(&pos_of).min().unwrap_or(0));
    for (key, natives) in plans_sorted {
        let commits: Vec<&crate::git_io::CommitMeta> =
            metas.iter().filter(|m| natives.contains(&m.sha)).collect();
        let message = args.squash.then(|| {
            squash_message(
                &repo,
                &source.fold,
                key,
                args.purge,
                &args.from,
                commits.len(),
            )
        });
        landings.push((key.clone(), commits, message));
    }

    if args.dry {
        println!(
            "# clank pick --dry ({} plan(s) from `{}`)",
            keys.len(),
            args.from
        );
        for (key, commits, message) in &landings {
            println!("# plan: {} ({} commit(s))", key.as_str(), commits.len());
            if let Some(msg) = message {
                let subject = msg.lines().next().unwrap_or_default();
                println!(
                    "squash {} commit(s) → 1: {subject}{}",
                    commits.len(),
                    if args.purge {
                        "  (.clank stripped)"
                    } else {
                        ""
                    },
                );
                for m in commits {
                    println!("  ← {} {}", &m.sha.as_str()[..7], m.subject);
                }
            } else {
                for m in commits {
                    println!(
                        "pick  {} {}{}",
                        &m.sha.as_str()[..7],
                        m.subject,
                        if args.purge {
                            "  (.clank stripped; dropped if empty)"
                        } else {
                            ""
                        },
                    );
                }
            }
        }
        println!("# (--dry: no commits, no refs updated)");
        return Ok(());
    }

    if !collapse {
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
        return Ok(());
    }

    // ── Collapse modes (pick-purge-and-squash) ──
    //
    // STUDY FINDINGS (verified against live git; see the plan):
    // - `cherry-pick -n` accepts a multi-commit sequence AND stacks onto
    //   an already-staged index, accumulating in the index/worktree.
    // - A mid-sequence conflict leaves git's cherry-pick state;
    //   `--abort` restores the pre-sequence state.
    // - The clean-case accumulated tree EQUALS target-base + the plan's
    //   cumulative source diff — no drift class beyond what plain
    //   per-commit cherry-pick already accepts (each step is the same
    //   3-way merge either way).
    // - A `.clank`-only commit CONFLICTS (modify/delete) if the earlier
    //   `.clank` state wasn't landed — so --purge must keep the
    //   index/worktree UNSTRIPPED during accumulation (full 3-way
    //   context) and strip only the COMMITTED trees.
    //
    // Mechanism: accumulate with `-n`, snapshot the index via
    // write-tree, strip via a dangling probe commit + the rewrite
    // engine's strip_tree/tree_clank_paths, chain landed commits with
    // replay_commit/squash_commit, and move HEAD ONCE with a final
    // reset --hard. Nothing lands until that reset: to back out at any
    // point — `git cherry-pick --abort` (if mid-conflict), then
    // `git reset --hard`.
    let mut cur_head = crate::git_io::resolve_commit(&repo, "HEAD")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve HEAD"))?
        .as_str()
        .to_string();
    let start_head = cur_head.clone();
    let mut landed = 0usize;
    let mut dropped = 0usize;
    for (key, commits, message) in &landings {
        if let Some(message) = message {
            // --squash: accumulate the whole plan, one commit.
            let shas: Vec<&str> = commits.iter().map(|m| m.sha.as_str()).collect();
            if !crate::git_plumbing::cherry_pick_no_commit(&repo, &shas)? {
                bail_collapse(key.as_str(), &args.from)?;
            }
            let tree = crate::git_plumbing::write_index_tree(&repo)?;
            let tip = shas.last().expect("non-empty plan");
            let candidate = crate::git_plumbing::squash_commit(
                &repo,
                tip,
                &tree,
                Some(&cur_head),
                message.as_bytes(),
            )?;
            let final_commit = if args.purge {
                strip_or_drop(&repo, &candidate, &cur_head)?.map(|stripped| {
                    crate::git_plumbing::squash_commit(
                        &repo,
                        tip,
                        &stripped,
                        Some(&cur_head),
                        message.as_bytes(),
                    )
                })
            } else {
                Some(Ok(candidate))
            };
            match final_commit {
                Some(c) => {
                    cur_head = c?;
                    landed += 1;
                }
                None => {
                    dropped += 1;
                    eprintln!(
                        "note: `{}` collapsed to nothing after the .clank strip — dropped",
                        key.as_str()
                    );
                }
            }
        } else {
            // --purge without --squash: per-commit, stripped, empties drop.
            for m in commits {
                if !crate::git_plumbing::cherry_pick_no_commit(&repo, &[m.sha.as_str()])? {
                    bail_collapse(key.as_str(), &args.from)?;
                }
                let tree = crate::git_plumbing::write_index_tree(&repo)?;
                let candidate = crate::git_plumbing::replay_commit(
                    &repo,
                    m.sha.as_str(),
                    &tree,
                    Some(&cur_head),
                )?;
                match strip_or_drop(&repo, &candidate, &cur_head)? {
                    Some(stripped) => {
                        cur_head = crate::git_plumbing::replay_commit(
                            &repo,
                            m.sha.as_str(),
                            &stripped,
                            Some(&cur_head),
                        )?;
                        landed += 1;
                    }
                    None => dropped += 1,
                }
            }
        }
    }
    if cur_head == start_head {
        // Everything dropped: restore the clean worktree, land nothing.
        crate::git_plumbing::reset_hard(&repo, "HEAD")?;
        println!(
            "picked {} plan(s) from `{}`: nothing to land (all commits empty after the .clank strip)",
            keys.len(),
            args.from,
        );
        return Ok(());
    }
    crate::git_plumbing::reset_hard(&repo, &cur_head)?;
    println!(
        "picked {} plan(s) from `{}`: {} commit(s) landed{} (reviews reset)",
        keys.len(),
        args.from,
        landed,
        if dropped > 0 {
            format!(", {dropped} dropped empty after the .clank strip")
        } else {
            String::new()
        },
    );
    Ok(())
}

/// Bail out of a collapse-mode conflict. Nothing has landed (HEAD only
/// moves at the final reset), so the back-out is total.
fn bail_collapse(stem: &str, from: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "cherry-pick conflicted while accumulating `{stem}`. NOTHING has landed \
         (HEAD moves once, at the end): run `git cherry-pick --abort`, then \
         `git reset --hard` to discard any accumulated changes. The source \
         `{from}` is untouched.",
    )
}

/// Strip the `.clank/` paths the pick ITSELF introduces (the rewrite
/// engine's `tree_clank_paths` + `strip_tree`): candidate paths that
/// are NOT in the parent's tree. The target's own tracked `.clank`
/// content (its .gitignore, finished/ history, other plans) is present
/// in the parent too and therefore preserved — stripping ALL `.clank`
/// paths would make the landed commit DELETE the target's files.
/// (Edge accepted: a pick that MODIFIES a `.clank` path the target
/// already tracks keeps that modification — only reachable when the
/// same artifact path exists on both sides.) `None` when the stripped
/// tree equals the parent's — empty after the strip, DROPPED (the same
/// disposition finish's engine uses).
fn strip_or_drop(
    repo: &std::path::Path,
    candidate: &str,
    parent: &str,
) -> anyhow::Result<Option<String>> {
    let cand = CommitSha::parse(candidate)
        .map_err(|e| anyhow::anyhow!("parse candidate sha `{candidate}`: {e}"))?;
    let par = CommitSha::parse(parent)
        .map_err(|e| anyhow::anyhow!("parse parent sha `{parent}`: {e}"))?;
    let cand_paths = crate::git_io::tree_clank_paths_at(repo, &cand)
        .map_err(|e| anyhow::anyhow!("listing .clank paths of `{candidate}`: {e}"))?;
    let parent_paths: std::collections::BTreeSet<String> =
        crate::git_io::tree_clank_paths_at(repo, &par)
            .map_err(|e| anyhow::anyhow!("listing .clank paths of `{parent}`: {e}"))?
            .into_iter()
            .collect();
    let strip: Vec<String> = cand_paths
        .into_iter()
        .filter(|p| !parent_paths.contains(p))
        .collect();
    let stripped = crate::git_plumbing::strip_tree(repo, candidate, &strip)?;
    let parent_tree = crate::git_plumbing::commit_tree_oid(repo, parent)?;
    Ok((stripped != parent_tree).then_some(stripped))
}

/// The composed one-commit message for a squashed plan: the plan's own
/// finalize subject + WHY when the source plan is FINISHED (de-tagged;
/// re-tagged `[stem]` unless --purge removes the plan file the tag
/// would refer to), a pick provenance line otherwise — through the same
/// `compose_squash_message` the TUI/finish squash uses.
fn squash_message(
    repo: &std::path::Path,
    source: &clank_core::repo_state::RepoState,
    key: &PlanKey,
    purge: bool,
    from: &str,
    n_commits: usize,
) -> String {
    let stem = key.as_str();
    let (subject_core, body) = match source.finished_plans.iter().find(|f| &f.plan == key) {
        Some(fp) => {
            let subject = crate::git_io::commit_subject_at(repo, &fp.finalized_at)
                .ok()
                .map(|s| {
                    s.strip_prefix(&format!("[{stem}] "))
                        .unwrap_or(&s)
                        .to_string()
                })
                .unwrap_or_else(|| stem.to_string());
            let body = crate::git_io::commit_body_at(repo, &fp.finalized_at).unwrap_or_default();
            (subject, body)
        }
        None => (stem.to_string(), String::new()),
    };
    // Tag equality: the squashed commit carries `plans/<stem>.md` unless
    // purged, so it must carry the `[stem]` tag exactly then.
    let subject = if purge {
        subject_core
    } else {
        format!("[{stem}] {subject_core}")
    };
    let provenance = format!(
        "collapsed from {n_commits} commit(s) picked from `{from}` by clank pick --squash."
    );
    crate::cli::status_tui::input::compose_squash_message(&subject, &body, &provenance)
}
