//! Phase 2 contract tests for `trinity-core`.
//!
//! Three categories:
//!
//! 1. **Wire-string pinning.** For every closed-vocab enum, every
//!    variant serializes to an exact snake_case string. A
//!    developer who renames `WaitingReason::CommitNeedsReview`
//!    to `WaitingReason::NeedsReview` flips the wire string from
//!    `"commit_needs_review"` to `"needs_review"` — this test
//!    fails at the renamer's commit.
//!
//! 2. **Tagged-enum round-trip.** Every variant of every
//!    `#[serde(tag = "kind")]` enum round-trips through
//!    `to_value` → `from_value`. Catches drift between the tag
//!    string and the Rust variant name.
//!
//! 3. **Decode-failure regression.** Representative malformed
//!    payloads (null on a required field, missing kind on a
//!    tagged enum, unknown variant string) fail to decode. The
//!    daemon's null-bug regression from `055d387` is the
//!    template — pinned here so the wire crate enforces the
//!    invariant.

use serde_json::json;
use trinity_core::api::*;
use trinity_core::ids::*;
use trinity_core::vocab::*;

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
fn posture_wire_strings() {
    assert_wire(Posture::Planning, "planning");
    assert_wire(Posture::Implementing, "implementing");
}

#[test]
fn plan_worktree_status_wire_strings() {
    assert_wire(PlanWorktreeStatus::Clean, "clean");
    assert_wire(PlanWorktreeStatus::BodyDirty, "body_dirty");
    assert_wire(PlanWorktreeStatus::PlanFileMissing, "plan_file_missing");
}

#[test]
fn commit_kind_wire_strings() {
    assert_wire(CommitKind::PlanOnly, "plan_only");
    assert_wire(CommitKind::CodeOnly, "code_only");
    assert_wire(CommitKind::Mixed, "mixed");
    assert_wire(CommitKind::MultiPlan, "multi_plan");
    assert_wire(CommitKind::Finalize, "finalize");
    assert_wire(CommitKind::Unattributed, "unattributed");
}

#[test]
fn plan_touch_kind_wire_strings() {
    assert_wire(PlanTouchKind::Intro, "intro");
    assert_wire(PlanTouchKind::Revision, "revision");
}

#[test]
fn review_target_phase_wire_strings() {
    assert_wire(ReviewTargetPhase::Plan, "plan");
    assert_wire(ReviewTargetPhase::Impl, "impl");
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
fn review_gate_state_wire_strings() {
    assert_wire(ReviewGateState::Ready, "ready");
    assert_wire(ReviewGateState::NeedsReview, "needs_review");
    assert_wire(ReviewGateState::ChangesRequested, "changes_requested");
}

#[test]
fn review_gate_state_maps_commit_gate_state() {
    assert_eq!(
        ReviewGateState::from(CommitGateState::Approved),
        ReviewGateState::Ready
    );
    assert_eq!(
        ReviewGateState::from(CommitGateState::Unreviewed),
        ReviewGateState::NeedsReview
    );
    assert_eq!(
        ReviewGateState::from(CommitGateState::ChangesRequested),
        ReviewGateState::ChangesRequested
    );
}

#[test]
fn waiting_role_wire_strings() {
    assert_wire(WaitingRole::Master, "master");
    assert_wire(WaitingRole::Reviewers, "reviewers");
    assert_wire(WaitingRole::None, "none");
}

#[test]
fn waiting_reason_wire_strings() {
    assert_wire(WaitingReason::SessionFinished, "session_finished");
    assert_wire(WaitingReason::CommitPlanRevision, "commit_plan_revision");
    assert_wire(
        WaitingReason::AddressCommitChanges,
        "address_commit_changes",
    );
    assert_wire(
        WaitingReason::ReadyToStartImplementation,
        "ready_to_start_implementation",
    );
    assert_wire(WaitingReason::CommitNeedsReview, "commit_needs_review");
}

// ============================================================
// `as_str` agrees with wire string
// ============================================================

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
    check!(Posture::Planning);
    check!(Posture::Implementing);
    check!(CommitKind::PlanOnly);
    check!(CommitKind::Finalize);
    check!(Verdict::Approve);
    check!(WaitingReason::CommitNeedsReview);
    check!(WaitingRole::Master);
    check!(CommitGateState::ChangesRequested);
    check!(PlanWorktreeStatus::PlanFileMissing);
    check!(PlanTouchKind::Revision);
    check!(ReviewTargetPhase::Impl);
}

// ============================================================
// Tagged-enum round-trip
// ============================================================

#[test]
fn timeline_event_commit_plan_round_trips() {
    let event = TimelineEvent::CommitPlan {
        sha: "abc123".into(),
        subject: "Plan revision: add foo".into(),
        plan_touch: PlanTouchKind::Revision,
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "commit_plan");
    assert_eq!(v["plan_touch"], "revision");
    let back: TimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn timeline_event_commit_finalize_round_trips() {
    let event = TimelineEvent::CommitFinalize {
        sha: "deadbeef".into(),
        subject: "Finalize foo".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "commit_finalize");
    let back: TimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn timeline_event_review_round_trips() {
    let event = TimelineEvent::Review {
        target: "abc123".into(),
        author: "codex".into(),
        verdict: Verdict::Approve,
        phase: ReviewTargetPhase::Plan,
        created_at: 1700000000,
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "review");
    assert_eq!(v["verdict"], "approve");
    assert_eq!(v["phase"], "plan");
    let back: TimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn commit_detail_finalize_flattens_kind_to_root() {
    let response = CommitDetailResponse {
        repo: "/r".into(),
        plan_id: "trinity/foo.md".into(),
        slug: "foo".into(),
        commit_sha: "abc".into(),
        subject: "Finalize foo".into(),
        message_body: "".into(),
        diff_files: vec![],
        detail: CommitDetail::Finalize {
            snapshot: vec![FinalizeApproval {
                author: "alice".into(),
                filename: "alice.md".into(),
                body: "APPROVE\n\nlgtm\n".into(),
            }],
        },
    };
    let v = serde_json::to_value(&response).unwrap();
    // Critical: ONE `kind` at the top level, not nested under
    // `detail`. `#[serde(flatten)]` over the tagged enum hoists
    // it.
    assert_eq!(v["kind"], "finalize");
    assert!(v.get("detail").is_none(), "expected flatten, got nested");
    assert!(v["snapshot"].is_array());
    let back: CommitDetailResponse = serde_json::from_value(v).unwrap();
    assert_eq!(response, back);
}

#[test]
fn commit_detail_plan_only_round_trips() {
    let response = CommitDetailResponse {
        repo: "/r".into(),
        plan_id: "trinity/foo.md".into(),
        slug: "foo".into(),
        commit_sha: "abc".into(),
        subject: "Plan revision".into(),
        message_body: "".into(),
        diff_files: vec![],
        detail: CommitDetail::PlanOnly { feedback: vec![] },
    };
    let v = serde_json::to_value(&response).unwrap();
    assert_eq!(v["kind"], "plan_only");
    assert!(v["feedback"].is_array());
    let back: CommitDetailResponse = serde_json::from_value(v).unwrap();
    assert_eq!(response, back);
}

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
        plan_id: "trinity/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        payload: PlanEventPayload::PlanWorktreeChanged {
            path: ".trinity/plans/foo.md".into(),
        },
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "plan");
    assert_eq!(v["kind"], "plan_worktree_changed");
    assert_eq!(v["path"], ".trinity/plans/foo.md");
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn live_event_plan_feedback_changed_has_no_extra_fields() {
    let ev = LiveEvent::Plan(PlanEvent {
        ts: 1700000000,
        plan_id: "trinity/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        payload: PlanEventPayload::FeedbackChanged {},
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["scope"], "plan");
    assert_eq!(v["kind"], "feedback_changed");
    let back: LiveEvent = serde_json::from_value(v).unwrap();
    assert_eq!(ev, back);
}

#[test]
fn work_payload_write_feedback_round_trips() {
    let payload = WorkPayload {
        plan_id: "trinity/foo.md".into(),
        repo: "/r".into(),
        action: ExpectedAction::WriteFeedback {
            path: ".trinity/feedback/foo/abc/codex.md".into(),
            target_sha: "abc".into(),
            plan_file: PlanFile {
                path: ".trinity/plans/foo.md".into(),
                content: None,
            },
        },
    };
    let v = serde_json::to_value(&payload).unwrap();
    // The action is flatten'd into WorkPayload — `kind` sits at the
    // top level alongside plan_id / repo, not nested under `action`.
    assert_eq!(v["plan_id"], "trinity/foo.md");
    assert_eq!(v["kind"], "write_feedback");
    assert_eq!(v["target_sha"], "abc");
    assert_eq!(v["path"], ".trinity/feedback/foo/abc/codex.md");
    assert_eq!(v["plan_file"]["path"], ".trinity/plans/foo.md");
    assert!(v["plan_file"].get("content").is_none());
    let back: WorkPayload = serde_json::from_value(v).unwrap();
    assert_eq!(payload, back);
}

#[test]
fn work_payload_session_finished_round_trips() {
    let payload = WorkPayload {
        plan_id: "trinity/foo.md".into(),
        repo: "/r".into(),
        action: ExpectedAction::SessionFinished,
    };
    let v = serde_json::to_value(&payload).unwrap();
    assert_eq!(v["kind"], "session_finished");
    let back: WorkPayload = serde_json::from_value(v).unwrap();
    assert_eq!(payload, back);
}

// ============================================================
// Decode-failure regressions
// ============================================================

#[test]
fn unknown_lifecycle_string_fails_to_decode() {
    let wire = json!("closed");
    let result: Result<PlanLifecycle, _> = serde_json::from_value(wire);
    assert!(
        result.is_err(),
        "renamed variant string must fail to decode; got {result:?}"
    );
}

#[test]
fn unknown_waiting_reason_fails_to_decode() {
    let wire = json!("needs_review");
    let result: Result<WaitingReason, _> = serde_json::from_value(wire);
    assert!(
        result.is_err(),
        "wire-string drift (commit_needs_review → needs_review) must fail; got {result:?}"
    );
}

#[test]
fn commit_detail_missing_kind_fails_to_decode() {
    let wire = json!({
        "repo": "/r",
        "plan_id": "trinity/foo.md",
        "slug": "foo",
        "commit_sha": "abc",
        "subject": "",
        "message_body": "",
        "diff_files": [],
        "feedback": []
    });
    let result: Result<CommitDetailResponse, _> = serde_json::from_value(wire);
    assert!(
        result.is_err(),
        "missing `kind` discriminator must fail; got {result:?}"
    );
}

#[test]
fn commit_detail_null_kind_fails_to_decode() {
    let wire = json!({
        "repo": "/r",
        "plan_id": "trinity/foo.md",
        "slug": "foo",
        "commit_sha": "abc",
        "subject": "",
        "message_body": "",
        "diff_files": [],
        "kind": null
    });
    let result: Result<CommitDetailResponse, _> = serde_json::from_value(wire);
    assert!(result.is_err(), "null `kind` must fail; got {result:?}");
}

#[test]
fn commit_detail_unknown_kind_fails_to_decode() {
    let wire = json!({
        "repo": "/r",
        "plan_id": "trinity/foo.md",
        "slug": "foo",
        "commit_sha": "abc",
        "subject": "",
        "message_body": "",
        "diff_files": [],
        "kind": "donemove"
    });
    let result: Result<CommitDetailResponse, _> = serde_json::from_value(wire);
    assert!(
        result.is_err(),
        "unknown variant string must fail; got {result:?}"
    );
}

#[test]
fn timeline_event_missing_kind_fails_to_decode() {
    let wire = json!({
        "sha": "abc",
        "subject": "x",
        "plan_touch": "revision"
    });
    let result: Result<TimelineEvent, _> = serde_json::from_value(wire);
    assert!(result.is_err(), "timeline event must carry `kind`");
}

// ============================================================
// Identifier newtypes — serde-transparent and validate-on-deserialize
// ============================================================

#[test]
fn ids_serialize_as_plain_strings() {
    let agent = AgentLabel::parse("alice").unwrap();
    let v = serde_json::to_value(&agent).unwrap();
    assert_eq!(v, json!("alice"));

    let key = PlanKey::parse("foo").unwrap();
    let v = serde_json::to_value(&key).unwrap();
    assert_eq!(v, json!("foo"));

    let sha = CommitSha::parse("abc123").unwrap();
    let v = serde_json::to_value(&sha).unwrap();
    assert_eq!(v, json!("abc123"));

    let repo = RepoBasename::parse("trinity").unwrap();
    let v = serde_json::to_value(&repo).unwrap();
    assert_eq!(v, json!("trinity"));

    let pid = PlanId::parse("trinity/foo.md").unwrap();
    let v = serde_json::to_value(&pid).unwrap();
    assert_eq!(v, json!("trinity/foo.md"));
}

#[test]
fn ids_validate_on_deserialize() {
    let bad: Result<AgentLabel, _> = serde_json::from_value(json!("a/b"));
    assert!(bad.is_err(), "deserialize must enforce no-slash");

    let bad: Result<CommitSha, _> = serde_json::from_value(json!("XYZ123"));
    assert!(bad.is_err(), "deserialize must enforce lowercase hex");

    let bad: Result<PlanKey, _> = serde_json::from_value(json!(".hidden"));
    assert!(bad.is_err(), "deserialize must enforce no-leading-dot");

    let bad: Result<PlanId, _> = serde_json::from_value(json!("trinity/.hidden.md"));
    assert!(bad.is_err(), "PlanId must reject leading-dot stems");
}

// ============================================================
// Tagged-enum round-trips for the three tagged-enums-for-non-
// orthogonal-fields conversions. The previously-prose invariants
// (e.g. "old_lineno is Some iff non-Insert/non-Meta") are now
// structural — these round-trips pin the wire shape for each
// variant.
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

fn commit_row_gate_fixture() -> CommitGate {
    use crate::CommitGateState;
    let alice = AgentLabel::parse("alice").unwrap();
    CommitGate {
        state: CommitGateState::Approved,
        participants: vec![alice.clone()],
        approvers: vec![alice.clone()],
        requesters: vec![],
        ambiguous: vec![],
        missing: vec![],
        feedback: std::collections::BTreeMap::new(),
    }
}

#[test]
fn commit_row_plan_only_round_trips() {
    let row = CommitRow::PlanOnly {
        sha: "abc".into(),
        gate: commit_row_gate_fixture(),
        feedback: vec![],
    };
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(v["kind"], "plan_only");
    assert!(v["gate"].is_object());
    let back: CommitRow = serde_json::from_value(v).unwrap();
    assert_eq!(row, back);
}

#[test]
fn commit_row_code_only_round_trips() {
    let row = CommitRow::CodeOnly {
        sha: "abc".into(),
        gate: commit_row_gate_fixture(),
        feedback: vec![],
    };
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(v["kind"], "code_only");
    let back: CommitRow = serde_json::from_value(v).unwrap();
    assert_eq!(row, back);
}

#[test]
fn commit_row_mixed_round_trips() {
    let row = CommitRow::Mixed {
        sha: "abc".into(),
        gate: commit_row_gate_fixture(),
        feedback: vec![],
    };
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(v["kind"], "mixed");
    let back: CommitRow = serde_json::from_value(v).unwrap();
    assert_eq!(row, back);
}

fn timeline_event_sha_fixture() -> CommitSha {
    CommitSha::parse("abcd1234").unwrap()
}

#[test]
fn plan_timeline_event_plan_only_round_trips() {
    let event = trinity_core::model::PlanTimelineEvent::PlanOnly {
        sha: timeline_event_sha_fixture(),
        author_ts: 1_700_000_000,
        subject: "Plan revision".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "plan_only");
    let back: trinity_core::model::PlanTimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn plan_timeline_event_code_only_round_trips() {
    let event = trinity_core::model::PlanTimelineEvent::CodeOnly {
        sha: timeline_event_sha_fixture(),
        author_ts: 1_700_000_000,
        subject: "Implement foo".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "code_only");
    let back: trinity_core::model::PlanTimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn plan_timeline_event_mixed_round_trips() {
    let event = trinity_core::model::PlanTimelineEvent::Mixed {
        sha: timeline_event_sha_fixture(),
        author_ts: 1_700_000_000,
        subject: "Plan + code change".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "mixed");
    let back: trinity_core::model::PlanTimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn plan_timeline_event_multi_plan_round_trips() {
    let event = trinity_core::model::PlanTimelineEvent::MultiPlan {
        sha: timeline_event_sha_fixture(),
        author_ts: 1_700_000_000,
        subject: "Touch two plans".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "multi_plan");
    assert!(
        v.get("gate").is_none(),
        "MultiPlan must not carry gate on the wire"
    );
    let back: trinity_core::model::PlanTimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}

#[test]
fn plan_timeline_event_finalize_round_trips() {
    let event = trinity_core::model::PlanTimelineEvent::Finalize {
        sha: timeline_event_sha_fixture(),
        author_ts: 1_700_000_000,
        subject: "Finalize foo".into(),
    };
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "finalize");
    assert!(
        v.get("gate").is_none(),
        "Finalize must not carry gate on the wire"
    );
    let back: trinity_core::model::PlanTimelineEvent = serde_json::from_value(v).unwrap();
    assert_eq!(event, back);
}
