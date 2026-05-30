//! Wire-shape tests for `clank-core`'s surviving public DTOs.
//!
//! Pins:
//! - snake_case wire strings for every closed-vocab enum
//! - `as_str` agrees with serde's wire form
//! - tagged-enum round-trips (`LiveEvent`, `DiffLine`)
//! - representative decode-failure cases (null on a required
//!   field, missing kind, unknown variant)

use clank_core::api::*;
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
    assert_wire(Verdict::Approve, "approve");
    assert_wire(Verdict::RequestChanges, "request_changes");
    assert_wire(Verdict::Unmarked, "unmarked");
}

#[test]
fn commit_gate_state_wire_strings() {
    assert_wire(CommitGateState::Unreviewed, "unreviewed");
    assert_wire(CommitGateState::Approved, "approved");
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
    assert_wire(WaitingReason::GateApproved, "gate_approved");
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
    check!(Verdict::Approve);
    check!(CommitGateState::ChangesRequested);
    check!(WaitingReason::CommitNeedsReview);
    check!(WaitingReason::ReadyToFinalize);
}

// ============================================================
// LiveEvent round-trips
// ============================================================

#[test]
fn live_event_repo_rebuilt_round_trips() {
    let ev = LiveEvent::Repo(RepoEvent {
        ts: 1700000000,
        payload: RepoEventPayload::RepoRebuilt {},
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "repo");
    assert_eq!(v["kind"], "repo_rebuilt");
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn live_event_repo_unwatched_carries_plan_count() {
    let ev = LiveEvent::Repo(RepoEvent {
        ts: 1700000000,
        payload: RepoEventPayload::RepoUnwatched { plan_count: 4 },
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "repo");
    assert_eq!(v["kind"], "repo_unwatched");
    assert_eq!(v["plan_count"], 4);
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn live_event_plan_worktree_changed_carries_path() {
    let ev = LiveEvent::Plan(PlanEvent {
        ts: 1700000000,
        plan_id: "clank/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        payload: PlanEventPayload::PlanWorktreeChanged {
            path: ".clank/plans/foo.md".into(),
        },
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "plan");
    assert_eq!(v["kind"], "plan_worktree_changed");
    assert_eq!(v["path"], ".clank/plans/foo.md");
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn live_event_plan_feedback_changed_has_no_extra_fields() {
    let ev = LiveEvent::Plan(PlanEvent {
        ts: 1700000000,
        plan_id: "clank/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        payload: PlanEventPayload::FeedbackChanged {},
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "plan");
    assert_eq!(v["kind"], "feedback_changed");
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

// ============================================================
// DiffLine round-trips
// ============================================================

#[test]
fn diff_line_insert_round_trips() {
    let line = DiffLine::Insert {
        content: " hi".into(),
        new_lineno: 7,
    };
    let v = serde_json::to_value(&line).unwrap();
    assert_eq!(v["kind"], "insert");
    assert_eq!(v["new_lineno"], 7);
    assert!(v.get("old_lineno").is_none());
    let back: DiffLine = serde_json::from_value(v).unwrap();
    assert_eq!(line, back);
}

#[test]
fn diff_line_delete_round_trips() {
    let line = DiffLine::Delete {
        content: " bye".into(),
        old_lineno: 3,
    };
    let v = serde_json::to_value(&line).unwrap();
    assert_eq!(v["kind"], "delete");
    assert_eq!(v["old_lineno"], 3);
    assert!(v.get("new_lineno").is_none());
    let back: DiffLine = serde_json::from_value(v).unwrap();
    assert_eq!(line, back);
}

#[test]
fn diff_line_context_round_trips() {
    let line = DiffLine::Context {
        content: " ctx".into(),
        old_lineno: 1,
        new_lineno: 1,
    };
    let v = serde_json::to_value(&line).unwrap();
    assert_eq!(v["kind"], "context");
    assert_eq!(v["old_lineno"], 1);
    assert_eq!(v["new_lineno"], 1);
    let back: DiffLine = serde_json::from_value(v).unwrap();
    assert_eq!(line, back);
}

#[test]
fn diff_line_meta_round_trips() {
    let line = DiffLine::Meta {
        content: "@@ -1,1 +1,1 @@".into(),
    };
    let v = serde_json::to_value(&line).unwrap();
    assert_eq!(v["kind"], "meta");
    assert!(v.get("old_lineno").is_none());
    assert!(v.get("new_lineno").is_none());
    let back: DiffLine = serde_json::from_value(v).unwrap();
    assert_eq!(line, back);
}

// ============================================================
// Decode-failure regressions
// ============================================================

#[test]
fn live_event_rejects_unknown_scope() {
    let v = json!({"scope": "ghost", "ts": 1});
    assert!(serde_json::from_value::<LiveEvent>(v).is_err());
}

#[test]
fn diff_line_rejects_missing_kind() {
    let v = json!({"content": "x", "new_lineno": 1});
    assert!(serde_json::from_value::<DiffLine>(v).is_err());
}

#[test]
fn verdict_rejects_unknown_string() {
    let v = json!("maybe");
    assert!(serde_json::from_value::<Verdict>(v).is_err());
}
