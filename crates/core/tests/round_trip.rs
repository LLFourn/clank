//! Wire-string tests for `clank-core`'s closed vocabulary.
//!
//! Pins:
//! - snake_case wire strings for every closed-vocab enum
//! - `as_str` agrees with serde's wire form
//! - rejection of unknown vocabulary strings

use clank_core::vocab::*;
use serde_json::json;

// ============================================================
// Wire-string pinning
// ============================================================

fn assert_wire(value: impl serde::Serialize, expected: &str) {
    let v = serde_json::to_value(&value).expect("serialize");
    assert_eq!(
        v.as_str(),
        Some(expected),
        "wire string drift: expected {expected:?}, got {v}"
    );
}

#[test]
fn plan_lifecycle_wire_strings() {
    assert_wire(PlanLifecycle::Active, "active");
    assert_wire(PlanLifecycle::Finished, "finished");
}

#[test]
fn plan_worktree_status_wire_strings() {
    assert_wire(PlanWorktreeStatus::Clean, "clean");
    assert_wire(PlanWorktreeStatus::BodyDirty, "body_dirty");
    assert_wire(PlanWorktreeStatus::PlanFileMissing, "plan_file_missing");
}

#[test]
fn verdict_wire_strings() {
    assert_wire(Verdict::Continue, "continue");
    assert_wire(Verdict::RequestChanges, "request_changes");
    assert_wire(Verdict::Unmarked, "unmarked");
}

#[test]
fn commit_gate_state_wire_strings() {
    assert_wire(CommitGateState::Unreviewed, "unreviewed");
    assert_wire(CommitGateState::Continued, "continued");
    assert_wire(CommitGateState::ChangesRequested, "changes_requested");
}

#[test]
fn waiting_reason_wire_strings() {
    assert_wire(WaitingReason::SessionFinished, "session_finished");
    assert_wire(WaitingReason::CommitPlanRevision, "commit_plan_revision");
    assert_wire(
        WaitingReason::AddressCommitChanges,
        "address_commit_changes",
    );
    assert_wire(WaitingReason::ReadyToFinalize, "ready_to_finalize");
    assert_wire(WaitingReason::GateContinue, "gate_continue");
    assert_wire(WaitingReason::CommitNeedsReview, "commit_needs_review");
}

#[test]
fn as_str_agrees_with_wire() {
    macro_rules! check {
        ($variant:expr) => {
            assert_eq!(
                serde_json::to_value(&$variant).unwrap().as_str().unwrap(),
                $variant.as_str(),
                "as_str disagrees with serde wire form for {:?}",
                $variant,
            );
        };
    }
    check!(PlanLifecycle::Active);
    check!(PlanLifecycle::Finished);
    check!(PlanWorktreeStatus::PlanFileMissing);
    check!(Verdict::Continue);
    check!(CommitGateState::ChangesRequested);
    check!(WaitingReason::CommitNeedsReview);
    check!(WaitingReason::ReadyToFinalize);
}

#[test]
fn verdict_rejects_unknown_string() {
    let v = json!("maybe");
    assert!(serde_json::from_value::<Verdict>(v).is_err());
}
