//! Parsing rules for filesystem-truth artifacts: feedback verdict markers
//! and feedback file paths.
//!
//! All functions here are pure. Disk I/O happens in the watcher / runner
//! layer; this module only translates string content and path segments
//! into typed values.

use std::path::{Component, Path, PathBuf};

use crate::lifecycle::{AgentLabel, CommitSha, PlanKey};
use crate::repo_state::Verdict;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPath {
    /// Directory name = filename stem. Same value as [`PlanKey`] from the
    /// plan file's path; the on-disk feedback layout is keyed by stem
    /// (see plan-path-identity §3).
    pub plan_key: PlanKey,
    pub phase: FeedbackPhase,
    pub target_sha: Option<CommitSha>,
    pub author: AgentLabel,
    pub raw: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackPhase {
    Plan,
    Impl,
}

impl FeedbackPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackPhase::Plan => "plan",
            FeedbackPhase::Impl => "impl",
        }
    }
}

/// Parse a path relative to `<repo>/.trinity/feedback/` into a `FeedbackPath`.
/// Expected shapes:
/// - `<session>/<plan|impl>/<author>.md` — flat drop, no SHA
/// - `<session>/<plan|impl>/<target-sha>/<author>.md` — canonical
///
/// Returns `None` if the path doesn't match either shape (e.g. a stray file
/// like `.DS_Store`, a directory, or extra path segments).
pub fn parse_feedback_path(rel: &Path) -> Option<FeedbackPath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    let (session, phase_seg, sha_seg, file_seg) = match segments.as_slice() {
        [session, phase, file] => (*session, *phase, None, *file),
        [session, phase, sha, file] => (*session, *phase, Some(*sha), *file),
        _ => return None,
    };

    let session_str = session.to_str()?;
    let phase = match phase_seg.to_str()? {
        "plan" => FeedbackPhase::Plan,
        "impl" => FeedbackPhase::Impl,
        _ => return None,
    };
    let file_str = file_seg.to_str()?;
    let author = file_str.strip_suffix(".md")?;
    if author.is_empty() {
        return None;
    }

    let target_sha = match sha_seg {
        Some(s) => {
            let sha_str = s.to_str()?;
            if !is_sha_segment(sha_str) {
                return None;
            }
            Some(CommitSha::from(sha_str.to_string()))
        }
        None => None,
    };

    Some(FeedbackPath {
        plan_key: PlanKey::from(session_str.to_string()),
        phase,
        target_sha,
        author: AgentLabel::from(author.to_string()),
        raw: rel.to_path_buf(),
    })
}

/// Plausible SHA-1 segment: hex string of length 7–40. Trinity accepts
/// shortened SHAs in path segments since git accepts them and reviewers
/// commonly paste short SHAs.
fn is_sha_segment(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Parse the first non-empty line of a feedback file body as a verdict
/// marker. Only exact uppercase tokens `APPROVE` / `REQUEST_CHANGES` count;
/// everything else is `Unmarked`.
pub fn parse_verdict(body: &str) -> Verdict {
    match body.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some("APPROVE") => Verdict::Approve,
        Some("REQUEST_CHANGES") => Verdict::RequestChanges,
        _ => Verdict::Unmarked,
    }
}

/// Determine whether a plan file's path indicates the session is "done"
/// (under `.trinity/plans/done/`).
pub fn plan_path_is_done(plan_path_rel: &Path) -> bool {
    plan_path_rel
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s == "done"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn flat_feedback_path_no_sha() {
        let parsed = parse_feedback_path(&p("foo/plan/alice.md")).unwrap();
        assert_eq!(parsed.plan_key.as_str(), "foo");
        assert_eq!(parsed.phase, FeedbackPhase::Plan);
        assert!(parsed.target_sha.is_none());
        assert_eq!(parsed.author.as_str(), "alice");
    }

    #[test]
    fn canonical_feedback_path_with_sha() {
        let parsed = parse_feedback_path(&p("foo/impl/abc1234/bob.md")).unwrap();
        assert_eq!(parsed.plan_key.as_str(), "foo");
        assert_eq!(parsed.phase, FeedbackPhase::Impl);
        assert_eq!(parsed.target_sha.unwrap().as_str(), "abc1234");
        assert_eq!(parsed.author.as_str(), "bob");
    }

    #[test]
    fn feedback_path_full_sha_accepted() {
        let parsed =
            parse_feedback_path(&p("foo/plan/abcdef0123456789abcdef0123456789abcdef01/x.md"))
                .unwrap();
        assert_eq!(
            parsed.target_sha.unwrap().as_str(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn feedback_path_short_sha_below_minimum_rejected() {
        // 6 hex chars — below the 7-char minimum
        assert!(parse_feedback_path(&p("foo/plan/abc012/x.md")).is_none());
    }

    #[test]
    fn feedback_path_non_hex_segment_rejected() {
        assert!(parse_feedback_path(&p("foo/plan/notasha1/x.md")).is_none());
    }

    #[test]
    fn feedback_path_too_few_segments_rejected() {
        assert!(parse_feedback_path(&p("foo/alice.md")).is_none());
    }

    #[test]
    fn feedback_path_extra_segments_rejected() {
        assert!(parse_feedback_path(&p("foo/plan/abc1234/extra/x.md")).is_none());
    }

    #[test]
    fn feedback_path_wrong_phase_rejected() {
        assert!(parse_feedback_path(&p("foo/review/alice.md")).is_none());
    }

    #[test]
    fn feedback_path_non_md_rejected() {
        assert!(parse_feedback_path(&p("foo/plan/alice.txt")).is_none());
    }

    #[test]
    fn parse_verdict_approve() {
        assert_eq!(parse_verdict("APPROVE\n\nbody\n"), Verdict::Approve);
    }

    #[test]
    fn parse_verdict_request_changes() {
        assert_eq!(
            parse_verdict("REQUEST_CHANGES\n\nbody\n"),
            Verdict::RequestChanges
        );
    }

    #[test]
    fn parse_verdict_leading_whitespace() {
        assert_eq!(
            parse_verdict("\n\n   APPROVE   \n\nbody\n"),
            Verdict::Approve
        );
    }

    #[test]
    fn parse_verdict_lowercase_is_unmarked() {
        assert_eq!(parse_verdict("approve\n\nbody\n"), Verdict::Unmarked);
    }

    #[test]
    fn parse_verdict_prose_is_unmarked() {
        assert_eq!(parse_verdict("This looks good to me.\n"), Verdict::Unmarked);
    }

    #[test]
    fn parse_verdict_empty_body() {
        assert_eq!(parse_verdict(""), Verdict::Unmarked);
    }

    #[test]
    fn plan_path_is_done_active() {
        assert!(!plan_path_is_done(&p(".trinity/plans/foo.md")));
    }

    #[test]
    fn plan_path_is_done_in_done_subdir() {
        assert!(plan_path_is_done(&p(".trinity/plans/done/foo.md")));
    }
}
