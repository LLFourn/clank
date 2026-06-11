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

/// Gap allowed between checkpoints at `distance_from_tip` commits
/// behind the fold's tip: 1 at the tip, doubling with distance.
fn allowed_gap(distance_from_tip: u64) -> u64 {
    (distance_from_tip / 2).max(1)
}

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

/// Should the fold persist a checkpoint after applying the commit
/// at `distance_from_tip` commits behind the fold's tip, when the
/// last persisted (or starting) checkpoint is `gap_since_last`
/// commits back? Always true at the tip itself.
pub fn should_checkpoint(gap_since_last: u64, distance_from_tip: u64) -> bool {
    gap_since_last >= allowed_gap(distance_from_tip)
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
        // (gap_since_last, distance_from_tip) → decision
        let cases = [
            // at and near the tip: every commit checkpoints
            (1, 0, true),
            (1, 1, true),
            (1, 2, true),
            (1, 3, true),
            // far back: gap must reach distance/2
            (4, 10, false),
            (5, 10, true),
            (499, 1000, false),
            (500, 1000, true),
            // zero gap never checkpoints (same commit)
            (0, 0, false),
            (0, 1000, false),
        ];
        for (gap, dist, want) in cases {
            assert_eq!(should_checkpoint(gap, dist), want, "gap={gap} dist={dist}");
        }
    }

    /// Simulate a cold fold of N commits: the write pattern the
    /// policy produces must be O(log N) and must include the tip.
    #[test]
    fn cold_fold_write_pattern_is_logarithmic() {
        for n in [1u64, 2, 10, 100, 1_000, 10_000] {
            let mut written = Vec::new();
            let mut last = 0u64;
            for depth in 1..=n {
                if should_checkpoint(depth - last, n - depth) {
                    written.push(depth);
                    last = depth;
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
