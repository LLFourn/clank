//! Read-time amend-chain classification for `implementation_revisions`.
//!
//! A run of consecutive rows in `list_for_plan(plan_id)` (ordered by `id ASC`)
//! that share a single `parent_sha` is an amend chain. The first row in the run
//! is the chain's base; every subsequent row is an amend of its predecessor.
//!
//! This is a structural rule, not a recorded fact: `git commit --amend` keeps
//! the parent SHA fixed, so the run-of-equal-parents pattern matches the user's
//! mental model ("the commit it was amending"). A rebase produces new parents
//! and so classifies as `Plain`.

use crate::storage::implementation_revisions::ImplementationRevision;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendInfo {
    /// Standard commit. One diff action: `parent_sha..commit_sha`.
    Plain,
    /// Amend of an earlier row. Two diff actions:
    /// - small: `prev_sha..commit_sha` (what this amend changed)
    /// - full:  `amend_base_parent_sha..commit_sha` (what the whole chain
    ///   accumulated since the original parent it amends onto).
    Amend {
        /// The previous row in the chain (the commit this one amends).
        prev_sha: String,
        /// The parent SHA shared by every row in the run — what the chain
        /// sits on top of.
        amend_base_parent_sha: Option<String>,
    },
}

/// Classify each row in `rows` (must be sorted ASC by `id`).
pub fn classify(rows: &[ImplementationRevision]) -> Vec<AmendInfo> {
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let amend = i > 0 && {
            let prev = &rows[i - 1];
            match (prev.parent_sha.as_deref(), row.parent_sha.as_deref()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        };
        if amend {
            out.push(AmendInfo::Amend {
                prev_sha: rows[i - 1].commit_sha.clone(),
                amend_base_parent_sha: row.parent_sha.clone(),
            });
        } else {
            out.push(AmendInfo::Plain);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(id: i64, sha: &str, parent: Option<&str>) -> ImplementationRevision {
        ImplementationRevision {
            id,
            plan_id: 1,
            commit_sha: sha.to_string(),
            parent_sha: parent.map(str::to_string),
            branch: None,
            commit_message: String::new(),
            diff_stat: String::new(),
            worktree_status: None,
            is_head: 0,
            registered_by: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn empty_list_returns_empty() {
        assert!(classify(&[]).is_empty());
    }

    #[test]
    fn single_plain_commit_classifies_plain() {
        let rows = vec![rev(1, "a", Some("p"))];
        assert_eq!(classify(&rows), vec![AmendInfo::Plain]);
    }

    #[test]
    fn two_commits_different_parents_both_plain() {
        let rows = vec![rev(1, "a", Some("p1")), rev(2, "b", Some("p2"))];
        assert_eq!(classify(&rows), vec![AmendInfo::Plain, AmendInfo::Plain]);
    }

    #[test]
    fn two_commits_same_parent_second_is_amend() {
        let rows = vec![rev(1, "a", Some("p")), rev(2, "b", Some("p"))];
        let got = classify(&rows);
        assert_eq!(got[0], AmendInfo::Plain);
        assert_eq!(
            got[1],
            AmendInfo::Amend {
                prev_sha: "a".to_string(),
                amend_base_parent_sha: Some("p".to_string()),
            }
        );
    }

    #[test]
    fn three_commits_same_parent_second_and_third_are_amend() {
        let rows = vec![
            rev(1, "a", Some("p")),
            rev(2, "b", Some("p")),
            rev(3, "c", Some("p")),
        ];
        let got = classify(&rows);
        assert_eq!(got[0], AmendInfo::Plain);
        assert_eq!(
            got[1],
            AmendInfo::Amend {
                prev_sha: "a".to_string(),
                amend_base_parent_sha: Some("p".to_string()),
            }
        );
        assert_eq!(
            got[2],
            AmendInfo::Amend {
                prev_sha: "b".to_string(),
                amend_base_parent_sha: Some("p".to_string()),
            }
        );
    }

    #[test]
    fn amend_then_plain_then_amend_correctly_segments() {
        // a (P1) — plain
        // b (P1) — amend of a
        // c (P2) — plain (parent changed; new run)
        // d (P2) — amend of c
        let rows = vec![
            rev(1, "a", Some("p1")),
            rev(2, "b", Some("p1")),
            rev(3, "c", Some("p2")),
            rev(4, "d", Some("p2")),
        ];
        let got = classify(&rows);
        assert_eq!(got[0], AmendInfo::Plain);
        assert!(matches!(got[1], AmendInfo::Amend { .. }));
        assert_eq!(got[2], AmendInfo::Plain);
        assert!(matches!(got[3], AmendInfo::Amend { .. }));
    }
}
