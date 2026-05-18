//! Typed fetch wrappers around the daemon's `/api/*` surface.
//!
//! DTOs are imported from `trinity-core` — the single shared
//! definition of every public response shape across daemon and
//! frontend. A rename like `WaitingReason::CommitNeedsReview ->
//! NeedsReview` fails at compile time everywhere it matters.
//!
//! This module owns only the FETCH machinery — URL construction,
//! `gloo-net` wrappers, and the `FetchError` enum. Wire-shape
//! changes happen in `trinity-core`, not here.

// Type aliases preserve the historical frontend-local names where
// they don't match the wire crate's naming.
pub use trinity_core::dto::{
    CommitDetail, CommitDetailResponse, CommitRowDetail, DiffHunk, DiffLine,
    DiffResponse as DiffPage, Feedback as CommitFeedback, FileDiff, FileDiffMode, FinalizeApproval,
    ListPlansResponse as PlansIndex, PlanConflict as PlanConflictRow,
    PlanDetailResponse as PlanDetail, PlanRevisionResponse as PlanRevisionPage, PlanRow, PrHint,
    PrHintOption, RepoListResponse as ReposIndex, RepoRow, ReviewGate, TimelineEvent, WaitingOn,
};
pub use trinity_core::vocab::{DiffLineKind, PlanLifecycle, Verdict, WaitingRole};

#[derive(Debug, Clone)]
pub enum FetchError {
    Network(String),
    Status(u16),
    Decode(String),
}

impl core::fmt::Display for FetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FetchError::Network(s) => write!(f, "network: {s}"),
            FetchError::Status(c) => write!(f, "HTTP {c}"),
            FetchError::Decode(s) => write!(f, "decode: {s}"),
        }
    }
}

pub async fn fetch_plans() -> Result<PlansIndex, FetchError> {
    let resp = gloo_net::http::Request::get("/api/plans")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlansIndex>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_plan(plan_id: String) -> Result<PlanDetail, FetchError> {
    let url = format!("/api/plan/{plan_id}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlanDetail>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_plan_revision(
    plan_id: String,
    sha: String,
) -> Result<PlanRevisionPage, FetchError> {
    let url = format!("/api/plan/{plan_id}/revision/{sha}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlanRevisionPage>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_commit_diff(
    plan_id: String,
    sha: String,
) -> Result<CommitDetailResponse, FetchError> {
    let url = format!("/api/plan/{plan_id}/commit/{sha}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<CommitDetailResponse>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_diff(plan_id: String, from: String, to: String) -> Result<DiffPage, FetchError> {
    let url = format!("/api/plan/{plan_id}/diff/{from}/{to}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<DiffPage>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_repos() -> Result<ReposIndex, FetchError> {
    let resp = gloo_net::http::Request::get("/api/repos")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<ReposIndex>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn delete_repo(
    basename: String,
) -> Result<trinity_core::dto::DeleteRepoOutcome, FetchError> {
    let url = format!("/api/repos/{basename}");
    let resp = gloo_net::http::Request::delete(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<trinity_core::dto::DeleteRepoOutcome>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[cfg(test)]
mod dto_roundtrip_tests {
    //! DTO contract tests. These are the same tests that caught the
    //! `finalize_snapshot: null` regression in commit `055d387`,
    //! kept here as a sanity check that the wire-crate types still
    //! handle the daemon's current emission shape. Most coverage
    //! now lives in `crates/trinity-core/tests/round_trip.rs`.
    use super::*;
    use trinity_core::vocab::CommitKind;

    #[test]
    fn finalize_commit_response_decodes() {
        let wire = r#"{
            "repo": "/r",
            "plan_id": "trinity/foo.md",
            "slug": "foo",
            "commit_sha": "abcdef",
            "subject": "Finalize foo",
            "message_body": "",
            "diff_files": [],
            "kind": "finalize",
            "snapshot": [
                {"author": "alice", "filename": "alice.md", "body_html": "<p>lgtm</p>"}
            ]
        }"#;
        let page: CommitDetailResponse =
            serde_json::from_str(wire).expect("decode finalize commit");
        match page.detail {
            CommitDetail::Finalize { snapshot } => {
                assert_eq!(snapshot.len(), 1);
                assert_eq!(snapshot[0].author, "alice");
            }
            _ => panic!("expected Finalize variant"),
        }
    }

    #[test]
    fn reviewable_commit_response_decodes() {
        let wire = r#"{
            "repo": "/r",
            "plan_id": "trinity/foo.md",
            "slug": "foo",
            "commit_sha": "abcdef",
            "subject": "Plan revision",
            "message_body": "",
            "diff_files": [],
            "kind": "plan_only",
            "feedback": []
        }"#;
        let page: CommitDetailResponse =
            serde_json::from_str(wire).expect("decode reviewable commit");
        assert!(matches!(page.detail, CommitDetail::PlanOnly { .. }));
    }

    #[test]
    fn missing_kind_fails_to_decode() {
        let wire = r#"{
            "repo": "/r",
            "plan_id": "trinity/foo.md",
            "slug": "foo",
            "commit_sha": "abcdef",
            "subject": "",
            "message_body": "",
            "diff_files": [],
            "feedback": []
        }"#;
        let result: Result<CommitDetailResponse, _> = serde_json::from_str(wire);
        assert!(result.is_err(), "missing kind must fail; got {result:?}");
    }

    /// Pin that the tagged-enum unknown-kind fails. Equivalent to the
    /// `null_finalize_snapshot_fails_to_decode` test in `055d387` but
    /// for the new wire shape.
    #[test]
    fn unknown_kind_fails_to_decode() {
        let wire = r#"{
            "repo": "/r",
            "plan_id": "trinity/foo.md",
            "slug": "foo",
            "commit_sha": "abcdef",
            "subject": "",
            "message_body": "",
            "diff_files": [],
            "kind": "donemove",
            "feedback": []
        }"#;
        let result: Result<CommitDetailResponse, _> = serde_json::from_str(wire);
        assert!(result.is_err(), "unknown kind must fail; got {result:?}");
    }

    /// CommitKind enum can be passed around as a value, e.g. for
    /// matching in components.
    #[test]
    fn commit_kind_enum_round_trip() {
        let v = serde_json::to_value(CommitKind::PlanOnly).unwrap();
        assert_eq!(v.as_str(), Some("plan_only"));
        let back: CommitKind = serde_json::from_value(v).unwrap();
        assert_eq!(back, CommitKind::PlanOnly);
    }
}
