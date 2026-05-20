//! Phase 0 of `trinity-core-unification.md`: schema baselines for
//! every top-level wire shape. A checked-in JSON file pins each
//! shape's field-key skeleton; deviations fail loudly with a diff.
//!
//! On first run / when the wire DTO genuinely changes, set
//! `UPDATE_SNAPSHOTS=1` to write the file with the new skeleton.
//! Schema updates should ONLY land alongside an entry in the plan's
//! "Wire-shape changes" enumeration.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use trinity_core::api::*;
use trinity_core::ids::*;
use trinity_core::vocab::*;

/// Skeleton of a JSON value: object → ordered keys + recursive skeletons;
/// array → first-element skeleton (we assume homogeneous arrays); leaves
/// reduce to their type tag (`"string"`, `"integer"`, etc).
fn skeleton(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for k in keys {
                out.insert(k.clone(), skeleton(&map[&k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            if let Some(first) = items.first() {
                Value::Array(vec![skeleton(first)])
            } else {
                Value::Array(vec![Value::String("<empty>".into())])
            }
        }
        Value::String(_) => Value::String("string".into()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Value::String("integer".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::Bool(_) => Value::String("bool".into()),
        Value::Null => Value::String("null".into()),
    }
}

fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wire_snapshots")
}

fn assert_schema(name: &str, value: &Value) {
    let dir = snapshots_dir();
    let path = dir.join(format!("{name}.schema.json"));
    let actual = serde_json::to_string_pretty(&skeleton(value)).unwrap();

    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        fs::create_dir_all(&dir).expect("create wire_snapshots dir");
        fs::write(&path, format!("{actual}\n")).expect("write snapshot");
        return;
    }

    // Missing baseline is a hard failure — silently writing it would
    // let a deleted (or never-committed) snapshot pass CI.
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "wire snapshot missing: {}\n\
             If this is a new wire shape, generate it with:\n  \
               UPDATE_SNAPSHOTS=1 cargo test -p trinity-core --test wire_snapshots",
            path.display(),
        )
    });
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "schema drift for {name}; run with UPDATE_SNAPSHOTS=1 to refresh"
    );
}

// ============================================================
// Fixtures
// ============================================================

fn feedback_fixture() -> Feedback {
    Feedback {
        author: AgentLabel::parse("alice").unwrap(),
        verdict: Verdict::Approve,
        body: "APPROVE\n\nlgtm\n".into(),
        path: ".trinity/feedback/foo/abc/alice.md".into(),
        created_at: 1_700_000_000,
    }
}

fn commit_gate_fixture() -> CommitGate {
    let alice = AgentLabel::parse("alice").unwrap();
    CommitGate {
        state: CommitGateState::Approved,
        participants: vec![alice.clone()],
        approvers: vec![alice.clone()],
        requesters: vec![],
        ambiguous: vec![],
        missing: vec![],
        feedback: BTreeMap::from([(alice, feedback_fixture())]),
    }
}

fn pr_hint_fixture() -> PrHint {
    PrHint {
        plan_intro: "def".into(),
        plan_intro_parent: Some("123".into()),
        implementation_commits: vec!["abc".into()],
        options: vec![PrHintOption {
            kind: PrHintOptionKind::KeepPlanInPr,
            base: "main".into(),
            command: "git merge --squash trinity/foo".into(),
        }],
        suggested_message: "Land plan: foo".into(),
    }
}

fn waiting_on_fixture() -> WaitingOn {
    WaitingOn {
        role: WaitingRole::Reviewers,
        reason: WaitingReason::CommitNeedsReview,
        agents: vec![AgentLabel::parse("alice").unwrap()],
        description: "alice has not voted".into(),
    }
}

fn archived_fixture() -> ArchivedCycle {
    ArchivedCycle {
        closer: CommitSha::parse("abc1234").unwrap(),
        approver_count: 2,
    }
}

fn commit_row_fixture() -> CommitRow {
    CommitRow::PlanOnly {
        sha: "abc".into(),
        gate: commit_gate_fixture(),
        feedback: vec![feedback_fixture()],
    }
}

fn timeline_review_fixture() -> TimelineEvent {
    TimelineEvent::Review {
        target: "abc".into(),
        author: "alice".into(),
        verdict: Verdict::Approve,
        phase: ReviewTargetPhase::Plan,
        created_at: 1_700_000_000,
    }
}

fn plan_row_fixture() -> PlanRow {
    PlanRow {
        repo: "/r".into(),
        plan_id: Some("trinity/foo.md".into()),
        slug: "foo".into(),
        lifecycle: PlanLifecycle::Active,
        current_path: ".trinity/plans/foo.md".into(),
        phase: Posture::Planning,
        plan_worktree_status: PlanWorktreeStatus::Clean,
        waiting_on: waiting_on_fixture(),
        latest_reviewable_sha: Some(CommitSha::parse("abc1234").unwrap()),
        gate_state: Some(trinity_core::vocab::CommitGateState::Unreviewed),
        archived_cycles: vec![archived_fixture()],
        last_activity_ts: 1_700_000_000,
    }
}

fn review_gate_fixture() -> ReviewGate {
    ReviewGate {
        state: ReviewGateState::Ready,
        phase: ReviewTargetPhase::Plan,
        participants: vec!["alice".into()],
        approvals: vec!["alice".into()],
        request_changes: vec![],
        missing_approvals: vec![],
    }
}

fn file_diff_fixture() -> FileDiff {
    FileDiff {
        path: "a.md".into(),
        old_path: Some("a.md".into()),
        additions: 1,
        deletions: 1,
        mode: FileDiffMode::Modified,
        binary: false,
        always_folded: false,
        hunks: vec![DiffHunk {
            header: "@@ -1,1 +1,1 @@".into(),
            lines: vec![DiffLine::Context {
                content: " hi".into(),
                old_lineno: 1,
                new_lineno: 1,
            }],
        }],
    }
}

// ============================================================
// Schema snapshots: one per top-level wire DTO.
// ============================================================

#[test]
fn schema_list_plans_response() {
    let v = serde_json::to_value(ListPlansResponse {
        plans: vec![plan_row_fixture()],
        conflicts: vec![PlanConflict {
            plan_id: Some("trinity/foo.md".into()),
            slug: "foo".into(),
            paths: vec![".trinity/plans/foo.md".into()],
        }],
    })
    .unwrap();
    assert_schema("list_plans_response", &v);
}

#[test]
fn schema_work_context_response() {
    let v = serde_json::to_value(WorkContextResponse {
        work: WorkPayload {
            plan_id: "trinity/foo.md".into(),
            repo: "/r".into(),
            action: ExpectedAction::WriteFeedback {
                path: ".trinity/feedback/foo/abc/alice.md".into(),
                target_sha: "abc".into(),
                plan_file: PlanFile {
                    path: ".trinity/plans/foo.md".into(),
                    content: None,
                },
            },
        },
        current_path: ".trinity/plans/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        phase: Posture::Planning,
        plan_worktree_status: PlanWorktreeStatus::Clean,
        waiting_on: waiting_on_fixture(),
    })
    .unwrap();
    assert_schema("work_context_response", &v);
}

#[test]
fn schema_set_active_work_response() {
    let v = serde_json::to_value(SetActiveWorkResponse {
        ok: true,
        plan_id: "trinity/foo.md".into(),
    })
    .unwrap();
    assert_schema("set_active_work_response", &v);
}

#[test]
fn schema_clear_active_work_response() {
    let v = serde_json::to_value(ClearActiveWorkResponse { ok: true }).unwrap();
    assert_schema("clear_active_work_response", &v);
}

#[test]
fn schema_watch_repo_response_registered() {
    let v = serde_json::to_value(WatchRepoResponse {
        repo: "/abs/path/to/repo".into(),
        basename: "repo".into(),
        status: WatchRepoStatus::Registered,
    })
    .unwrap();
    assert_schema("watch_repo_response_registered", &v);
}

#[test]
fn schema_watch_repo_response_already_watching() {
    let v = serde_json::to_value(WatchRepoResponse {
        repo: "/abs/path/to/repo".into(),
        basename: "repo".into(),
        status: WatchRepoStatus::AlreadyWatching,
    })
    .unwrap();
    assert_schema("watch_repo_response_already_watching", &v);
}

#[test]
fn schema_plan_detail_response() {
    let v = serde_json::to_value(PlanDetailResponse {
        repo: "/r".into(),
        plan_id: Some("trinity/foo.md".into()),
        slug: "foo".into(),
        lifecycle: PlanLifecycle::Active,
        current_path: ".trinity/plans/foo.md".into(),
        phase: Posture::Planning,
        plan_worktree_status: PlanWorktreeStatus::Clean,
        waiting_on: waiting_on_fixture(),
        review_target: Some(ReviewTarget {
            commit_sha: "abc".into(),
            phase: ReviewTargetPhase::Plan,
        }),
        review_gate: Some(review_gate_fixture()),
        latest_plan_revision: Some(CommitRef {
            commit_sha: "abc".into(),
        }),
        latest_implementation_revision: None,
        plan_revisions: vec!["abc".into()],
        implementation_commits: vec![],
        commits: vec![commit_row_fixture()],
        latest_relevant_commit: Some("abc".into()),
        plan_body: "# hi\n".into(),
        timeline: vec![timeline_review_fixture()],
        pr_hint: Some(pr_hint_fixture()),
        archived_cycles: vec![archived_fixture()],
    })
    .unwrap();
    assert_schema("plan_detail_response", &v);
}

#[test]
fn schema_commit_detail_response_plan_only() {
    let v = serde_json::to_value(CommitDetailResponse {
        repo: "/r".into(),
        plan_id: "trinity/foo.md".into(),
        slug: "foo".into(),
        commit_sha: "abc".into(),
        subject: "subject".into(),
        message_body: "body".into(),
        diff_files: vec![file_diff_fixture()],
        detail: CommitDetail::PlanOnly {
            feedback: vec![feedback_fixture()],
        },
    })
    .unwrap();
    assert_schema("commit_detail_response_plan_only", &v);
}

#[test]
fn schema_commit_detail_response_finalize() {
    let v = serde_json::to_value(CommitDetailResponse {
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
    })
    .unwrap();
    assert_schema("commit_detail_response_finalize", &v);
}

#[test]
fn schema_plan_revision_response() {
    let v = serde_json::to_value(PlanRevisionResponse {
        repo: "/r".into(),
        plan_id: "trinity/foo.md".into(),
        slug: "foo".into(),
        commit_sha: "abc".into(),
        body: "# plan\n".into(),
        plan_intro: "def".into(),
        plan_intro_parent: Some("123".into()),
        previous_sha: None,
        next_sha: Some("ghi".into()),
        feedback: vec![feedback_fixture()],
    })
    .unwrap();
    assert_schema("plan_revision_response", &v);
}

#[test]
fn schema_diff_response() {
    let v = serde_json::to_value(DiffResponse {
        from: "abc".into(),
        to: "def".into(),
        from_path: ".trinity/plans/foo.md".into(),
        to_path: ".trinity/plans/foo.md".into(),
        diff_files: vec![file_diff_fixture()],
    })
    .unwrap();
    assert_schema("diff_response", &v);
}

#[test]
fn schema_repo_list_response() {
    let v = serde_json::to_value(RepoListResponse {
        repos: vec![RepoRow {
            basename: "r".into(),
            root: "/r".into(),
            plan_count: 1,
            last_activity_ts: 1_700_000_000,
        }],
    })
    .unwrap();
    assert_schema("repo_list_response", &v);
}

#[test]
fn schema_delete_repo_outcome() {
    let v = serde_json::to_value(DeleteRepoOutcome {
        ok: true,
        basename: "r".into(),
        plan_count: 1,
        registry_write_error: None,
    })
    .unwrap();
    assert_schema("delete_repo_outcome", &v);
}

#[test]
fn schema_wait_for_work_work() {
    let v = serde_json::to_value(WaitForWorkResponse::Work(WaitWorkPayload {
        work: WorkPayload {
            plan_id: "trinity/foo.md".into(),
            repo: "/r".into(),
            action: ExpectedAction::WriteFeedback {
                path: ".trinity/feedback/foo/abc/alice.md".into(),
                target_sha: "abc".into(),
                plan_file: PlanFile {
                    path: ".trinity/plans/foo.md".into(),
                    content: None,
                },
            },
        },
        stale_reviews: Vec::new(),
    }))
    .unwrap();
    assert_schema("wait_for_work_response_work", &v);
}

#[test]
fn schema_wait_for_work_timeout() {
    let v = serde_json::to_value(WaitForWorkResponse::Timeout(WaitTimeout {
        timed_out: true,
        no_active_plans: false,
        repo: None,
    }))
    .unwrap();
    assert_schema("wait_for_work_response_timeout", &v);
}

#[test]
fn schema_live_event_repo() {
    let v = serde_json::to_value(LiveEvent::Repo(RepoEvent {
        ts: 1_700_000_000,
        payload: RepoEventPayload::RepoUnwatched { plan_count: 4 },
    }))
    .unwrap();
    assert_schema("live_event_repo", &v);
}

#[test]
fn schema_live_event_plan() {
    let v = serde_json::to_value(LiveEvent::Plan(PlanEvent {
        ts: 1_700_000_000,
        plan_id: "trinity/foo.md".into(),
        lifecycle: PlanLifecycle::Active,
        payload: PlanEventPayload::PlanWorktreeChanged {
            path: ".trinity/plans/foo.md".into(),
        },
    }))
    .unwrap();
    assert_schema("live_event_plan", &v);
}

#[test]
fn schema_mcp_error_no_active_plan() {
    let v = serde_json::to_value(McpErrorPayload::NoActivePlan {
        repo: "/r".into(),
        message: "no active plans in /r".into(),
    })
    .unwrap();
    assert_schema("mcp_error_no_active_plan", &v);
}

#[test]
fn schema_mcp_error_ambiguous_plan() {
    let v = serde_json::to_value(McpErrorPayload::AmbiguousPlan {
        message: "multiple active plans".into(),
        candidates: vec![PlanCandidate {
            plan_id: "trinity/foo.md".into(),
            current_path: ".trinity/plans/foo.md".into(),
            lifecycle: PlanLifecycle::Active,
        }],
    })
    .unwrap();
    assert_schema("mcp_error_ambiguous_plan", &v);
}

#[test]
fn schema_mcp_error_unknown_repo() {
    let v = serde_json::to_value(McpErrorPayload::UnknownRepo {
        basename: "r".into(),
    })
    .unwrap();
    assert_schema("mcp_error_unknown_repo", &v);
}

#[test]
fn schema_mcp_error_plan_conflict() {
    let v = serde_json::to_value(McpErrorPayload::PlanConflict {
        slug: "foo".into(),
        paths: vec![".trinity/plans/foo.md".into()],
    })
    .unwrap();
    assert_schema("mcp_error_plan_conflict", &v);
}

#[test]
fn schema_mcp_error_plan_not_committed() {
    let v = serde_json::to_value(McpErrorPayload::PlanNotCommitted {
        plan_id: "trinity/foo.md".into(),
        slug: "foo".into(),
        next_step: "commit it".into(),
    })
    .unwrap();
    assert_schema("mcp_error_plan_not_committed", &v);
}

#[test]
fn schema_mcp_error_plan_not_active() {
    let v = serde_json::to_value(McpErrorPayload::PlanNotActive {
        plan_id: "trinity/foo.md".into(),
        message: "plan trinity/foo.md is finalized".into(),
    })
    .unwrap();
    assert_schema("mcp_error_plan_not_active", &v);
}

#[test]
fn schema_mcp_error_plan_hidden() {
    let v = serde_json::to_value(McpErrorPayload::PlanHidden {
        plan_id: "trinity/foo.md".into(),
        message: "plan file missing from working tree".into(),
    })
    .unwrap();
    assert_schema("mcp_error_plan_hidden", &v);
}

#[test]
fn schema_start_plan_response() {
    let v = serde_json::to_value(StartPlanResponse {
        plan_id: "trinity/foo.md".into(),
        repo: "/r".into(),
        canonical_path: "/r/.trinity/plans/foo.md".into(),
        slug: "foo".into(),
        committed: false,
        next_step: "edit then commit".into(),
    })
    .unwrap();
    assert_schema("start_plan_response", &v);
}

#[test]
fn skeleton_sanity_check() {
    let v = json!({
        "kind": "x",
        "n": 4,
        "list": [{"a": "b"}, {"a": "c"}],
        "empty": [],
        "nested": {"deeper": {"flag": true, "miss": null}},
    });
    let sk = skeleton(&v);
    assert_eq!(sk["kind"], "string");
    assert_eq!(sk["n"], "integer");
    assert_eq!(sk["list"][0]["a"], "string");
    assert_eq!(sk["empty"][0], "<empty>");
    assert_eq!(sk["nested"]["deeper"]["flag"], "bool");
    assert_eq!(sk["nested"]["deeper"]["miss"], "null");
}
