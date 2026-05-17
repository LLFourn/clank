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
    pub target_sha: CommitSha,
    pub author: AgentLabel,
    pub raw: PathBuf,
}

/// Parse a path relative to `<repo>/.trinity/feedback/` into a
/// `FeedbackPath`. Expected shape:
///
/// `<plan-key>/<target-sha>/<author>.md`
///
/// Returns `None` for any other shape (a stray `.DS_Store`, the legacy
/// `<plan-key>/commits/<sha>/<author>.md` layout, etc.).
pub fn parse_feedback_path(rel: &Path) -> Option<FeedbackPath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    let (session, sha_seg, file_seg) = match segments.as_slice() {
        [session, sha, file] => (*session, *sha, *file),
        _ => return None,
    };

    let session_str = session.to_str()?;
    let file_str = file_seg.to_str()?;
    let author = file_str.strip_suffix(".md")?;
    if author.is_empty() {
        return None;
    }
    let sha_str = sha_seg.to_str()?;
    if !is_sha_segment(sha_str) {
        return None;
    }

    Some(FeedbackPath {
        plan_key: PlanKey::parse(session_str).ok()?,
        target_sha: CommitSha::parse(sha_str).ok()?,
        author: AgentLabel::parse(author).ok()?,
        raw: rel.to_path_buf(),
    })
}

/// Detect the pre-event-log-and-finished feedback path shape
/// `<plan-key>/commits/<target-sha>/<author>.md`. Used at watcher startup
/// to surface a one-shot warning so operators don't silently lose
/// in-flight feedback after the path rename.
pub fn is_legacy_commits_feedback_path(rel: &Path) -> bool {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    matches!(
        segments.as_slice(),
        [_session, kind, _sha, file]
            if kind.to_str() == Some("commits")
            && file.to_str().is_some_and(|s| s.ends_with(".md"))
    )
}

/// Build the canonical relative feedback path
/// `<plan-key>/<target-sha>/<author>.md` (the part under
/// `.trinity/feedback/`).
pub fn canonical_feedback_path(
    plan_key: &PlanKey,
    target_sha: &CommitSha,
    author: &AgentLabel,
) -> PathBuf {
    PathBuf::from(format!(
        "{}/{}/{}.md",
        plan_key.as_str(),
        target_sha.as_str(),
        author.as_str()
    ))
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
    fn canonical_path_parses() {
        let parsed = parse_feedback_path(&p("foo/abc1234/bob.md")).unwrap();
        assert_eq!(parsed.plan_key.as_str(), "foo");
        assert_eq!(parsed.target_sha.as_str(), "abc1234");
        assert_eq!(parsed.author.as_str(), "bob");
    }

    #[test]
    fn full_sha_accepted() {
        let parsed = parse_feedback_path(&p(
            "foo/abcdef0123456789abcdef0123456789abcdef01/x.md",
        ))
        .unwrap();
        assert_eq!(
            parsed.target_sha.as_str(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn short_sha_below_minimum_rejected() {
        assert!(parse_feedback_path(&p("foo/abc012/x.md")).is_none());
    }

    #[test]
    fn non_hex_sha_rejected() {
        assert!(parse_feedback_path(&p("foo/notasha1/x.md")).is_none());
    }

    #[test]
    fn legacy_commits_segment_rejected() {
        assert!(parse_feedback_path(&p("foo/commits/abc1234/alice.md")).is_none());
    }

    #[test]
    fn legacy_plan_impl_segments_rejected() {
        assert!(parse_feedback_path(&p("foo/plan/abc1234/alice.md")).is_none());
        assert!(parse_feedback_path(&p("foo/impl/abc1234/alice.md")).is_none());
    }

    #[test]
    fn too_few_segments_rejected() {
        assert!(parse_feedback_path(&p("foo/alice.md")).is_none());
    }

    #[test]
    fn extra_segments_rejected() {
        assert!(parse_feedback_path(&p("foo/abc1234/extra/x.md")).is_none());
    }

    #[test]
    fn non_md_rejected() {
        assert!(parse_feedback_path(&p("foo/abc1234/alice.txt")).is_none());
    }

    #[test]
    fn is_legacy_commits_feedback_path_detects_old_shape() {
        assert!(is_legacy_commits_feedback_path(&p(
            "foo/commits/abc1234/alice.md"
        )));
    }

    #[test]
    fn is_legacy_commits_feedback_path_rejects_new_shape() {
        assert!(!is_legacy_commits_feedback_path(&p("foo/abc1234/alice.md")));
    }

    #[test]
    fn canonical_feedback_path_builds_expected_shape() {
        let key = PlanKey::parse("foo").unwrap();
        let sha = CommitSha::parse("abc1234").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            canonical_feedback_path(&key, &sha, &author),
            PathBuf::from("foo/abc1234/alice.md")
        );
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
