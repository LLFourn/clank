//! Checkpoint spacing policy for the fold's on-disk state cache.
//!
//! Pure arithmetic over commit *depths* (first-parent count from
//! the repo root). The CLI's fold driver consults
//! [`should_checkpoint`] per applied commit to decide when to
//! persist the in-progress [`repo_state::RepoState`], and
//! [`prune_plan`] after a fold to thin checkpoints that drifted
//! out of spec as the tip advanced. The policy is exponential
//! spacing: every commit near the tip, gaps doubling with distance
//! behind it — O(log N) checkpoints total, so any range fold finds
//! a base within half its distance from the tip.
//!
//! [`repo_state`]: crate::repo_state

/// Distance bucket a surviving checkpoint occupies: power-of-two
/// bands behind the tip (`0` for the tip itself and distance 1,
/// then `[2,4)`, `[4,8)`, …). [`prune_plan`] keeps one checkpoint
/// per bucket.
fn bucket(distance_from_tip: u64) -> u32 {
    match distance_from_tip {
        0 | 1 => 0,
        d => d.ilog2(),
    }
}

/// The canonical checkpoint distances behind the tip: the tip (`0`),
/// its parent (`1`), then powers of two (`2, 4, 8, …`). This is
/// EXACTLY the set [`prune_plan`] keeps — each canonical distance is
/// the sole member of its [`bucket`] — so the write policy and the
/// evict policy share one survivor set. A fold therefore never writes
/// a checkpoint its own trailing prune would immediately delete
/// (`fold_then_prune_is_a_fixed_point`); the previous gap-based policy
/// wrote tip-3 (and tip-5..7, …) only for prune to evict them every
/// frame — a write→fsync→delete churn that pegged the status TUI.
pub fn should_checkpoint(distance_from_tip: u64) -> bool {
    distance_from_tip == 0 || distance_from_tip.is_power_of_two()
}

/// Depths to DELETE so the surviving checkpoints keep one entry
/// per power-of-two distance bucket behind `tip_depth`.
///
/// Within a bucket the deepest (closest-to-tip) checkpoint
/// survives. The tip checkpoint itself is never deleted. Depths
/// beyond `tip_depth` (another branch's tip, or a reset moved us
/// backwards) are left alone — ancestry is unknown here, and the
/// cache's mtime-based aging reclaims them eventually.
pub fn prune_plan(checkpoint_depths: &[u64], tip_depth: u64) -> Vec<u64> {
    let mut sorted: Vec<u64> = checkpoint_depths
        .iter()
        .copied()
        .filter(|&d| d <= tip_depth)
        .collect();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.dedup();

    let mut deletions = Vec::new();
    let mut bucket_taken: Option<u32> = None;
    for depth in sorted {
        if depth == tip_depth {
            continue;
        }
        let b = bucket(tip_depth - depth);
        if bucket_taken == Some(b) {
            deletions.push(depth);
        } else {
            bucket_taken = Some(b);
        }
    }
    deletions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_checkpoint_matrix() {
        // distance_from_tip → decision. Canonical = {0, 1, 2, 4, 8, …};
        // tip-3 is NOT written (prune would evict it — the storm).
        let cases = [
            (0, true), // tip
            (1, true), // tip's parent
            (2, true),
            (3, false), // the regression: was written, prune deleted it
            (4, true),
            (5, false),
            (6, false),
            (7, false),
            (8, true),
            (16, true),
            (1000, false),
            (1024, true),
        ];
        for (dist, want) in cases {
            assert_eq!(should_checkpoint(dist), want, "dist={dist}");
        }
    }

    /// Simulate a cold fold of N commits: the write pattern the
    /// policy produces must be O(log N) and must include the tip.
    #[test]
    fn cold_fold_write_pattern_is_logarithmic() {
        for n in [1u64, 2, 10, 100, 1_000, 10_000] {
            let mut written = Vec::new();
            for depth in 1..=n {
                if should_checkpoint(n - depth) {
                    written.push(depth);
                }
            }
            assert_eq!(*written.last().unwrap(), n, "tip checkpointed (n={n})");
            let bound = 2 * (64 - n.leading_zeros() as u64) + 4;
            assert!(
                (written.len() as u64) <= bound,
                "O(log n) writes: n={n} wrote {} (bound {bound})",
                written.len()
            );
        }
    }

    /// The write policy and the evict policy share ONE survivor set:
    /// every checkpoint a fresh fold writes must SURVIVE the trailing
    /// prune. Regression guard for the status-TUI fsync storm — the
    /// gap-based `should_checkpoint` wrote tip-3 while `prune_plan`
    /// evicted it, so every render re-wrote and re-deleted it with a
    /// full F_FULLFSYNC. Nothing previously pinned the two policies'
    /// AGREEMENT.
    #[test]
    fn fold_then_prune_is_a_fixed_point() {
        for n in [1u64, 2, 3, 4, 5, 8, 13, 100, 1_000, 9_999] {
            let written: Vec<u64> = (1..=n)
                .filter(|&depth| should_checkpoint(n - depth))
                .collect();
            assert_eq!(
                prune_plan(&written, n),
                Vec::<u64>::new(),
                "fold to tip {n} wrote {written:?}; prune deleted some — policies disagree"
            );
        }
    }

    #[test]
    fn prune_never_deletes_tip() {
        let depths = [100, 99, 98, 97, 50, 25];
        let deletions = prune_plan(&depths, 100);
        assert!(!deletions.contains(&100));
    }

    #[test]
    fn prune_keeps_one_per_bucket_deepest_wins() {
        // tip 100; distances: 1,2,3,4,6,50 → buckets 0,1,1,2,2,5.
        let depths = [99, 98, 97, 96, 94, 50, 100];
        let mut deletions = prune_plan(&depths, 100);
        deletions.sort_unstable();
        // bucket 1 keeps 98 (deeper), deletes 97; bucket 2 keeps
        // 96, deletes 94; 99 (bucket 0), 50 (alone) survive.
        assert_eq!(deletions, vec![94, 97]);
    }

    #[test]
    fn prune_is_idempotent() {
        let depths = [100u64, 99, 98, 97, 96, 90, 80, 60, 20];
        let deletions = prune_plan(&depths, 100);
        let survivors: Vec<u64> = depths
            .iter()
            .copied()
            .filter(|d| !deletions.contains(d))
            .collect();
        assert_eq!(
            prune_plan(&survivors, 100),
            Vec::<u64>::new(),
            "pruning survivors deletes nothing"
        );
    }

    #[test]
    fn prune_leaves_unknown_future_depths_alone() {
        // Depths beyond the tip (other branch / reset): untouched.
        let deletions = prune_plan(&[150, 140, 100, 99], 100);
        assert!(!deletions.contains(&150) && !deletions.contains(&140));
    }

    /// As the tip advances, what was a dense cluster behind the
    /// old tip thins out — total survivors stay O(log N).
    #[test]
    fn prune_rebalances_as_tip_advances() {
        // Dense cluster the write policy would have left at tip 100…
        let mut depths: Vec<u64> = vec![100, 99, 98, 97, 95, 90, 80, 60, 20];
        // …tip advances far; re-prune relative to the new tip.
        let new_tip = 10_000;
        depths.push(new_tip);
        let deletions = prune_plan(&depths, new_tip);
        let survivors = depths.len() - deletions.len();
        assert!(
            survivors <= 4,
            "old cluster collapses to one-per-bucket: {survivors} left"
        );
        assert!(!deletions.contains(&new_tip));
    }
}
