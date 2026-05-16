//! Walk-back commit attribution. Pure: takes a structured per-commit diff
//! summary and the parent's effective session; returns the commit's
//! `AttributionResult`.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` "Commit Attribution —
//! pure git walk" for the four rules this module enforces.

use crate::lifecycle::PlanKey;
use crate::repo_state::{AttributionResult, PlanTouchKind};

/// Per-commit summary of plan-file changes and code changes.
///
/// Built from `git diff-tree -r --name-status -M <sha>` in the IO layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitChanges {
    pub plan_touches: Vec<PlanTouch>,
    /// True if any non-`.trinity/` file was modified.
    pub has_non_plan_code_changes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTouch {
    pub session: PlanKey,
    pub kind: PlanTouchKind,
}

/// Classify a commit into an `AttributionResult` given its diff summary and
/// the effective session of its parent.
///
/// Rules:
/// - Exactly one plan touch → `Attributed { session: that, plan_touch: Some(kind), has_code_changes }`.
/// - Zero plan touches → inherit parent's effective session (if any), `plan_touch: None`.
/// - More than one plan touch → `Unattributed` (descendants walk through).
///
/// `parent_effective` is the session of the nearest single-plan-touch
/// ancestor in the parent chain. Multi-plan-touch parents transmit their
/// own grandparent's effective session forward — this is handled by the
/// caller, which threads `effective_session_of(commit)` through the walk.
pub fn classify(changes: &CommitChanges, parent_effective: Option<&PlanKey>) -> AttributionResult {
    match changes.plan_touches.len() {
        1 => {
            let touch = &changes.plan_touches[0];
            AttributionResult::Attributed {
                session: touch.session.clone(),
                plan_touch: Some(touch.kind),
                has_code_changes: changes.has_non_plan_code_changes,
            }
        }
        0 => match parent_effective {
            Some(session) => AttributionResult::Attributed {
                session: session.clone(),
                plan_touch: None,
                has_code_changes: changes.has_non_plan_code_changes,
            },
            None => AttributionResult::Unattributed,
        },
        _ => AttributionResult::Unattributed,
    }
}

/// The "effective session" for a commit is the nearest ancestor's owning
/// session — the value descendants inherit during walk-back. Both
/// zero-plan-touch and multi-plan-touch commits transmit their parent's
/// effective session forward.
pub fn effective_session(
    changes: &CommitChanges,
    parent_effective: Option<&PlanKey>,
) -> Option<PlanKey> {
    match changes.plan_touches.len() {
        1 => Some(changes.plan_touches[0].session.clone()),
        _ => parent_effective.cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(name: &str) -> PlanKey {
        PlanKey::parse(name).unwrap()
    }

    fn touch(name: &str, kind: PlanTouchKind) -> PlanTouch {
        PlanTouch {
            session: sess(name),
            kind,
        }
    }

    fn changes(touches: Vec<PlanTouch>, code: bool) -> CommitChanges {
        CommitChanges {
            plan_touches: touches,
            has_non_plan_code_changes: code,
        }
    }

    #[test]
    fn single_plan_touch_no_code_attributes_to_that_session() {
        let c = changes(vec![touch("foo", PlanTouchKind::Intro)], false);
        let result = classify(&c, None);
        assert_eq!(
            result,
            AttributionResult::Attributed {
                session: sess("foo"),
                plan_touch: Some(PlanTouchKind::Intro),
                has_code_changes: false,
            }
        );
    }

    #[test]
    fn single_plan_touch_with_code_attributes_mixed() {
        let c = changes(vec![touch("foo", PlanTouchKind::Revision)], true);
        let result = classify(&c, None);
        assert_eq!(
            result,
            AttributionResult::Attributed {
                session: sess("foo"),
                plan_touch: Some(PlanTouchKind::Revision),
                has_code_changes: true,
            }
        );
    }

    #[test]
    fn zero_plan_touch_with_code_inherits_parent_session() {
        let c = changes(vec![], true);
        let parent = sess("foo");
        let result = classify(&c, Some(&parent));
        assert_eq!(
            result,
            AttributionResult::Attributed {
                session: sess("foo"),
                plan_touch: None,
                has_code_changes: true,
            }
        );
    }

    #[test]
    fn zero_plan_touch_no_parent_is_unattributed() {
        let c = changes(vec![], true);
        let result = classify(&c, None);
        assert_eq!(result, AttributionResult::Unattributed);
    }

    #[test]
    fn zero_plan_touch_no_code_no_parent_is_unattributed() {
        let c = changes(vec![], false);
        let result = classify(&c, None);
        assert_eq!(result, AttributionResult::Unattributed);
    }

    #[test]
    fn multi_plan_touch_is_unattributed_even_with_parent() {
        let c = changes(
            vec![
                touch("foo", PlanTouchKind::Revision),
                touch("bar", PlanTouchKind::Revision),
            ],
            true,
        );
        let parent = sess("baz");
        let result = classify(&c, Some(&parent));
        assert_eq!(result, AttributionResult::Unattributed);
    }

    #[test]
    fn effective_session_single_plan_touch_returns_that_session() {
        let c = changes(vec![touch("foo", PlanTouchKind::Intro)], false);
        assert_eq!(effective_session(&c, None), Some(sess("foo")));
    }

    #[test]
    fn effective_session_zero_plan_touch_inherits_parent() {
        let c = changes(vec![], true);
        let parent = sess("foo");
        assert_eq!(effective_session(&c, Some(&parent)), Some(sess("foo")));
    }

    #[test]
    fn effective_session_multi_plan_touch_is_transparent_to_parent() {
        let c = changes(
            vec![
                touch("foo", PlanTouchKind::Revision),
                touch("bar", PlanTouchKind::Revision),
            ],
            false,
        );
        let parent = sess("baz");
        // Multi-plan-touch is transparent: descendants walking through this
        // commit see the parent's effective session, not None.
        assert_eq!(effective_session(&c, Some(&parent)), Some(sess("baz")));
    }

    #[test]
    fn effective_session_multi_plan_touch_no_parent_is_none() {
        let c = changes(
            vec![
                touch("foo", PlanTouchKind::Revision),
                touch("bar", PlanTouchKind::Revision),
            ],
            false,
        );
        assert_eq!(effective_session(&c, None), None);
    }

    #[test]
    fn linear_chain_walk_attributes_correctly() {
        // Simulate a chain: plan_intro(A), impl_1(no plan), impl_2(no plan),
        // plan_revision(B), impl_3(no plan), impl_4(no plan).
        let chain = vec![
            (
                "intro_a",
                changes(vec![touch("a", PlanTouchKind::Intro)], false),
            ),
            ("impl_1", changes(vec![], true)),
            ("impl_2", changes(vec![], true)),
            (
                "intro_b",
                changes(vec![touch("b", PlanTouchKind::Intro)], false),
            ),
            ("impl_3", changes(vec![], true)),
            ("impl_4", changes(vec![], true)),
        ];

        let mut effective: Option<PlanKey> = None;
        let mut results = Vec::new();
        for (label, c) in &chain {
            let result = classify(c, effective.as_ref());
            effective = effective_session(c, effective.as_ref());
            results.push((label.to_string(), result));
        }

        // intro_a → A
        assert!(matches!(
            &results[0].1,
            AttributionResult::Attributed { session, plan_touch: Some(PlanTouchKind::Intro), .. } if session.as_str() == "a"
        ));
        // impl_1 → A (walked back)
        assert!(matches!(
            &results[1].1,
            AttributionResult::Attributed { session, plan_touch: None, has_code_changes: true } if session.as_str() == "a"
        ));
        // impl_2 → A (still in A's chain)
        assert!(matches!(
            &results[2].1,
            AttributionResult::Attributed { session, plan_touch: None, .. } if session.as_str() == "a"
        ));
        // intro_b → B (switches via plan-touch commit)
        assert!(matches!(
            &results[3].1,
            AttributionResult::Attributed { session, plan_touch: Some(PlanTouchKind::Intro), .. } if session.as_str() == "b"
        ));
        // impl_3 → B
        assert!(matches!(
            &results[4].1,
            AttributionResult::Attributed { session, plan_touch: None, .. } if session.as_str() == "b"
        ));
        // impl_4 → B
        assert!(matches!(
            &results[5].1,
            AttributionResult::Attributed { session, plan_touch: None, .. } if session.as_str() == "b"
        ));
    }

    #[test]
    fn multi_plan_commit_is_transparent_to_subsequent_walk() {
        // Chain: intro(A), multi_plan(A+B), impl(no plan).
        // The multi-plan commit is unattributed, but the impl after it
        // should walk through to A.
        let chain = vec![
            (
                "intro_a",
                changes(vec![touch("a", PlanTouchKind::Intro)], false),
            ),
            (
                "multi",
                changes(
                    vec![
                        touch("a", PlanTouchKind::Revision),
                        touch("b", PlanTouchKind::Intro),
                    ],
                    false,
                ),
            ),
            ("impl_after_multi", changes(vec![], true)),
        ];

        let mut effective: Option<PlanKey> = None;
        let mut results = Vec::new();
        for (label, c) in &chain {
            let result = classify(c, effective.as_ref());
            effective = effective_session(c, effective.as_ref());
            results.push((label.to_string(), result));
        }

        assert!(matches!(
            &results[0].1,
            AttributionResult::Attributed { .. }
        ));
        assert!(matches!(&results[1].1, AttributionResult::Unattributed));
        // impl_after_multi walks through `multi` (which transmitted A's
        // effective_session forward) and lands on A.
        assert!(matches!(
            &results[2].1,
            AttributionResult::Attributed { session, plan_touch: None, has_code_changes: true } if session.as_str() == "a"
        ));
    }

    #[test]
    fn root_with_no_plan_touch_is_unattributed() {
        // First commit in history has no plan touch — unattributed (pre-Trinity).
        let c = changes(vec![], true);
        assert_eq!(classify(&c, None), AttributionResult::Unattributed);
    }
}
