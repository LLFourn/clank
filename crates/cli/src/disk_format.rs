//! Parsing rules for filesystem-truth artifacts: feedback verdict markers
//! and feedback file paths.
//!
//! All functions here are pure. Disk I/O happens in the watcher / runner
//! layer; this module only translates string content and path segments
//! into typed values.

use std::path::{Component, Path, PathBuf};

use crate::lifecycle::{AgentLabel, CommitRef, CommitSha, PlanKey};
use clank_core::feedback_view::{filename_mode, filename_stem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPath {
    pub author: AgentLabel,
    pub target_ref: CommitRef,
    pub raw: PathBuf,
}

/// Parse a path RELATIVE TO `<repo>/.clank/` into a
/// `FeedbackPath`.
///
/// Accepted shape: `agents/<author>/feedback/<commit-ref>.md`
///
/// Returns `None` for any other shape.
pub fn parse_feedback_path(rel: &Path) -> Option<FeedbackPath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    let (agents_lit, author_seg, feedback_lit, file_seg) = match segments.as_slice() {
        [a, b, c, d] => (*a, *b, *c, *d),
        _ => return None,
    };

    if agents_lit.to_str()? != "agents" || feedback_lit.to_str()? != "feedback" {
        return None;
    }

    let author = AgentLabel::parse(author_seg.to_str()?).ok()?;

    let file_str = file_seg.to_str()?;
    let stem = file_str.strip_suffix(".md")?;
    let target_ref = CommitRef::parse(stem).ok()?;

    Some(FeedbackPath {
        author,
        target_ref,
        raw: rel.to_path_buf(),
    })
}

/// A parsed `.clank/finished/<plan-stem>` path — an empty marker
/// file indicating a plan is finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizePath {
    pub plan_key: PlanKey,
}

/// Parse a path relative to `<repo>/.clank/finished/` into a
/// `FinalizePath`. Expected shape: `<plan-stem>` (a single segment,
/// no extension).
pub fn parse_finalize_path(rel: &Path) -> Option<FinalizePath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    let [stem] = segments.as_slice() else {
        return None;
    };
    Some(FinalizePath {
        plan_key: PlanKey::parse(stem.to_str()?).ok()?,
    })
}

/// Build the canonical relative feedback path
/// `agents/<author>/feedback/<ref>.md` (the part under
/// `<repo>/.clank/`).
pub fn canonical_feedback_path(
    author: &AgentLabel,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> PathBuf {
    let mode = filename_mode(scope_shas);
    let stem = filename_stem(target_sha, mode);
    PathBuf::from(format!(
        "agents/{}/feedback/{}.md",
        author.as_str(),
        stem
    ))
}

/// Build the repo-relative wire-form feedback path
/// `.clank/agents/<author>/feedback/<ref>.md`.
pub fn feedback_path_wire(
    author: &AgentLabel,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> String {
    format!(
        ".clank/{}",
        canonical_feedback_path(author, target_sha, scope_shas).display()
    )
}

pub use clank_core::feedback_body::parse_verdict;

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn flat_short_ref_parses() {
        let parsed = parse_feedback_path(&p("agents/bob/feedback/abc1234.md")).unwrap();
        assert_eq!(parsed.target_ref.as_str(), "abc1234");
        assert_eq!(parsed.author.as_str(), "bob");
    }

    #[test]
    fn flat_full_sha_parses() {
        let parsed = parse_feedback_path(&p(
            "agents/bob/feedback/abcdef0123456789abcdef0123456789abcdef01.md",
        ))
        .unwrap();
        assert_eq!(
            parsed.target_ref.as_str(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn missing_agents_prefix_rejected() {
        assert!(parse_feedback_path(&p("bob/feedback/abc1234.md")).is_none());
    }

    #[test]
    fn missing_feedback_segment_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/foo/abc1234.md")).is_none());
    }

    #[test]
    fn invalid_author_rejected() {
        assert!(parse_feedback_path(&p("agents/.hidden/feedback/abc1234.md")).is_none());
    }

    #[test]
    fn short_ref_below_minimum_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/abc012.md")).is_none());
    }

    #[test]
    fn non_hex_ref_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/notasha1.md")).is_none());
    }

    #[test]
    fn old_plan_scoped_layout_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/foo/abc1234.md")).is_none());
    }

    #[test]
    fn non_md_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/abc1234.txt")).is_none());
    }

    #[test]
    fn legacy_layout_rejected() {
        assert!(parse_feedback_path(&p("foo/abc1234/bob.md")).is_none());
        assert!(parse_feedback_path(&p("feedback/foo/abc1234/bob.md")).is_none());
    }

    #[test]
    fn parse_finalize_path_single_segment() {
        let parsed = parse_finalize_path(&p("foo")).unwrap();
        assert_eq!(parsed.plan_key.as_str(), "foo");
    }

    #[test]
    fn parse_finalize_path_rejects_nested() {
        assert!(parse_finalize_path(&p("foo/alice.md")).is_none());
    }

    #[test]
    fn parse_finalize_path_rejects_invalid_plan_key() {
        assert!(parse_finalize_path(&p(".bad")).is_none());
    }

    #[test]
    fn canonical_feedback_path_short_mode() {
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            canonical_feedback_path(&author, &sha, &[sha.clone()]),
            PathBuf::from("agents/alice/feedback/abcdef0.md")
        );
    }

    #[test]
    fn canonical_feedback_path_long_mode_on_collision() {
        let a = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let b = CommitSha::parse("abcdef0222222222222222222222222222222222").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            canonical_feedback_path(&author, &a, &[a.clone(), b.clone()]),
            PathBuf::from(format!("agents/alice/feedback/{}.md", a.as_str()))
        );
    }

    #[test]
    fn feedback_path_wire_prepends_dot_clank() {
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            feedback_path_wire(&author, &sha, &[sha.clone()]),
            ".clank/agents/alice/feedback/abcdef0.md"
        );
    }

    #[test]
    fn canonical_and_parse_round_trip() {
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        let built = canonical_feedback_path(&author, &sha, &[sha.clone()]);
        let parsed = parse_feedback_path(&built).expect("round-trip parse");
        assert_eq!(parsed.author, author);
        assert_eq!(parsed.target_ref.as_str(), "abcdef0");
    }
}
