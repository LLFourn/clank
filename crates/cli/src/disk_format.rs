//! Parsing rules for filesystem-truth artifacts: feedback verdict markers
//! and feedback file paths.
//!
//! All functions here are pure. Disk I/O happens in the watcher / runner
//! layer; this module only translates string content and path segments
//! into typed values.

use std::path::{Component, Path, PathBuf};

use crate::lifecycle::{AgentLabel, CommitRef, CommitSha, PlanKey};
use crate::repo_state::Verdict;
use clank_core::feedback_view::{FilenameMode, filename_mode, filename_stem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPath {
    pub author: AgentLabel,
    /// What this feedback file targets — a specific plan's commit
    /// (`Plan`), or a commit outside any plan (`AdHoc`, encoded
    /// under the reserved `_` target segment).
    pub target: FeedbackTarget,
    /// On-disk commit reference (7–40 hex). NOT commit identity
    /// on its own — resolve against the target's reviewable scope
    /// before comparing to `CommitSha`.
    pub target_ref: CommitRef,
    pub raw: PathBuf,
}

/// What a feedback file targets. `AdHoc` covers commits touching
/// no plan file — their feedback lives at the reserved `_` target
/// segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackTarget {
    Plan(PlanKey),
    AdHoc,
}

/// Reserved segment used in feedback paths to indicate ad hoc
/// (no-plan) reviewability. `PlanKey::parse("_")` rejects this
/// token; the path parser checks it before falling through to
/// `PlanKey::parse`.
pub const AD_HOC_FEEDBACK_KEY: &str = "_";

impl FeedbackPath {
    /// Convenience: the plan key this feedback is attributed to,
    /// or `None` for ad hoc feedback.
    pub fn plan_key(&self) -> Option<&PlanKey> {
        match &self.target {
            FeedbackTarget::Plan(key) => Some(key),
            FeedbackTarget::AdHoc => None,
        }
    }
}

/// Parse a path RELATIVE TO `<repo>/.clank/` into a
/// `FeedbackPath`. The single parser used by every consumer
/// (`feedback_scan`, `git_io::collect_feedback_files`,
/// `fs_watcher::path_to_signal`). No caller hand-rolls
/// segment splitting.
///
/// Accepted shape:
///
/// `agents/<author>/feedback/<plan-or-_>/<commit-ref>.md`
///
/// - `<author>`     parses through `AgentLabel::parse`.
/// - `<plan-or-_>`  parses through `PlanKey::parse`, OR matches
///   the reserved `_` literal (→ `AdHoc`).
/// - `<commit-ref>` parses through `CommitRef::parse` (7–40 hex).
///
/// Returns `None` for any other shape (`.DS_Store`, legacy
/// `feedback/<plan>/<sha>/<author>.md`, etc.).
pub fn parse_feedback_path(rel: &Path) -> Option<FeedbackPath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    let (agents_lit, author_seg, feedback_lit, target_seg, file_seg) = match segments.as_slice() {
        [a, b, c, d, e] => (*a, *b, *c, *d, *e),
        _ => return None,
    };

    if agents_lit.to_str()? != "agents" || feedback_lit.to_str()? != "feedback" {
        return None;
    }

    let author = AgentLabel::parse(author_seg.to_str()?).ok()?;

    let target_str = target_seg.to_str()?;
    let target = if target_str == AD_HOC_FEEDBACK_KEY {
        FeedbackTarget::AdHoc
    } else {
        FeedbackTarget::Plan(PlanKey::parse(target_str).ok()?)
    };

    let file_str = file_seg.to_str()?;
    let stem = file_str.strip_suffix(".md")?;
    let target_ref = CommitRef::parse(stem).ok()?;

    Some(FeedbackPath {
        author,
        target,
        target_ref,
        raw: rel.to_path_buf(),
    })
}

/// A parsed `.clank/finished/<plan-stem>/<author>.md` path — one
/// approving-reviewer entry inside a finalize snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizePath {
    pub plan_key: PlanKey,
    pub author: AgentLabel,
    pub raw: PathBuf,
}

/// Parse a path relative to `<repo>/.clank/finished/` into a
/// `FinalizePath`. Expected shape:
///
/// `<plan-stem>/<author>.md` (flat — no per-SHA subdirectory).
///
/// Returns `None` for any other shape.
pub fn parse_finalize_path(rel: &Path) -> Option<FinalizePath> {
    let segments: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    let (stem, file_seg) = match segments.as_slice() {
        [stem, file] => (*stem, *file),
        _ => return None,
    };
    let author = file_seg.to_str()?.strip_suffix(".md")?;
    if author.is_empty() {
        return None;
    }
    Some(FinalizePath {
        plan_key: PlanKey::parse(stem.to_str()?).ok()?,
        author: AgentLabel::parse(author).ok()?,
        raw: rel.to_path_buf(),
    })
}

/// True iff the first non-empty line, trimmed, starts with `APPROVE`
/// (the bare token, optionally followed by other characters). Used by
/// the finalize rule's per-file check.
pub fn finalize_first_line_starts_with_approve(first_line: &str) -> bool {
    first_line.trim_start().starts_with("APPROVE")
}

/// Render the target slot (`<plan-or-_>`) as a path segment.
pub fn target_segment(target: &FeedbackTarget) -> &str {
    match target {
        FeedbackTarget::Plan(key) => key.as_str(),
        FeedbackTarget::AdHoc => AD_HOC_FEEDBACK_KEY,
    }
}

/// Build the canonical relative feedback path
/// `agents/<author>/feedback/<target>/<ref>.md` (the part under
/// `<repo>/.clank/`). The caller supplies `scope_shas` — every
/// reviewable commit in this target's scope — so the writer can
/// pick the right per-target filename mode (short vs full SHA).
pub fn canonical_feedback_path(
    author: &AgentLabel,
    target: &FeedbackTarget,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> PathBuf {
    let mode = filename_mode(scope_shas);
    let stem = filename_stem(target_sha, mode);
    PathBuf::from(format!(
        "agents/{}/feedback/{}/{}.md",
        author.as_str(),
        target_segment(target),
        stem
    ))
}

/// Build the repo-relative wire-form feedback path
/// `.clank/agents/<author>/feedback/<target>/<ref>.md`. This is
/// the string that lives on `WaitItem::Reviewer.feedback_path`
/// (and historically on `WorkPayload` / `StaleReview` shapes).
/// Single source of truth for the wire shape.
pub fn feedback_path_wire(
    author: &AgentLabel,
    target: &FeedbackTarget,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> String {
    format!(
        ".clank/{}",
        canonical_feedback_path(author, target, target_sha, scope_shas).display()
    )
}

/// Re-export for tests that want to use the mode helper
/// directly. Kept as part of the disk_format surface since
/// callers building feedback paths use it.
pub use clank_core::feedback_view::FilenameMode as PlanFilenameMode;
#[allow(dead_code)]
fn _filename_mode_marker(_: FilenameMode) {}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn plan_scoped_short_ref_parses() {
        let parsed = parse_feedback_path(&p("agents/bob/feedback/foo/abc1234.md")).unwrap();
        assert_eq!(parsed.plan_key().unwrap().as_str(), "foo");
        assert_eq!(parsed.target_ref.as_str(), "abc1234");
        assert_eq!(parsed.author.as_str(), "bob");
    }

    #[test]
    fn plan_scoped_full_sha_parses() {
        let parsed = parse_feedback_path(&p(
            "agents/bob/feedback/foo/abcdef0123456789abcdef0123456789abcdef01.md",
        ))
        .unwrap();
        assert_eq!(
            parsed.target_ref.as_str(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn ad_hoc_short_ref_parses() {
        let parsed = parse_feedback_path(&p("agents/codex/feedback/_/abc1234.md")).unwrap();
        assert_eq!(parsed.target, FeedbackTarget::AdHoc);
        assert!(parsed.plan_key().is_none());
        assert_eq!(parsed.target_ref.as_str(), "abc1234");
        assert_eq!(parsed.author.as_str(), "codex");
    }

    #[test]
    fn ad_hoc_full_sha_parses() {
        let parsed = parse_feedback_path(&p(
            "agents/codex/feedback/_/abcdef0123456789abcdef0123456789abcdef01.md",
        ))
        .unwrap();
        assert_eq!(parsed.target, FeedbackTarget::AdHoc);
    }

    #[test]
    fn missing_agents_prefix_rejected() {
        assert!(parse_feedback_path(&p("bob/feedback/foo/abc1234.md")).is_none());
    }

    #[test]
    fn missing_feedback_segment_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/foo/abc1234.md")).is_none());
    }

    #[test]
    fn invalid_author_rejected() {
        assert!(parse_feedback_path(&p("agents/.hidden/feedback/foo/abc1234.md")).is_none());
    }

    #[test]
    fn invalid_plan_key_rejected() {
        // PlanKey rejects leading dot.
        assert!(parse_feedback_path(&p("agents/bob/feedback/.bad/abc1234.md")).is_none());
    }

    #[test]
    fn short_ref_below_minimum_rejected() {
        // CommitRef requires >= 7 chars.
        assert!(parse_feedback_path(&p("agents/bob/feedback/foo/abc012.md")).is_none());
    }

    #[test]
    fn non_hex_ref_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/foo/notasha1.md")).is_none());
    }

    #[test]
    fn extra_segments_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/foo/abc1234/extra.md")).is_none());
    }

    #[test]
    fn non_md_rejected() {
        assert!(parse_feedback_path(&p("agents/bob/feedback/foo/abc1234.txt")).is_none());
    }

    #[test]
    fn legacy_layout_rejected() {
        // Old `.clank/feedback/<plan>/<sha>/<author>.md` shape no
        // longer parses — the parser intentionally only accepts
        // the new agent-keyed layout.
        assert!(parse_feedback_path(&p("foo/abc1234/bob.md")).is_none());
        assert!(parse_feedback_path(&p("feedback/foo/abc1234/bob.md")).is_none());
    }

    #[test]
    fn parse_finalize_path_flat_shape() {
        let parsed = parse_finalize_path(&p("foo/alice.md")).unwrap();
        assert_eq!(parsed.plan_key.as_str(), "foo");
        assert_eq!(parsed.author.as_str(), "alice");
    }

    #[test]
    fn parse_finalize_path_rejects_per_sha_subdir() {
        assert!(parse_finalize_path(&p("foo/abc1234/alice.md")).is_none());
    }

    #[test]
    fn parse_finalize_path_rejects_top_level_md() {
        assert!(parse_finalize_path(&p("alice.md")).is_none());
    }

    #[test]
    fn parse_finalize_path_rejects_non_md() {
        assert!(parse_finalize_path(&p("foo/alice.txt")).is_none());
    }

    #[test]
    fn finalize_first_line_approve_passes() {
        assert!(finalize_first_line_starts_with_approve("APPROVE"));
        assert!(finalize_first_line_starts_with_approve(
            "APPROVE — looks good"
        ));
        assert!(finalize_first_line_starts_with_approve("  APPROVE"));
    }

    #[test]
    fn finalize_first_line_other_fails() {
        assert!(!finalize_first_line_starts_with_approve("REQUEST_CHANGES"));
        assert!(!finalize_first_line_starts_with_approve("approve"));
        assert!(!finalize_first_line_starts_with_approve(""));
    }

    #[test]
    fn canonical_feedback_path_short_mode() {
        let key = PlanKey::parse("foo").unwrap();
        let target = FeedbackTarget::Plan(key);
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            canonical_feedback_path(&author, &target, &sha, &[sha.clone()]),
            PathBuf::from("agents/alice/feedback/foo/abcdef0.md")
        );
    }

    #[test]
    fn canonical_feedback_path_long_mode_on_collision() {
        let key = PlanKey::parse("foo").unwrap();
        let target = FeedbackTarget::Plan(key);
        let a = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let b = CommitSha::parse("abcdef0222222222222222222222222222222222").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        // Two commits share `abcdef0` — scope is in long mode.
        assert_eq!(
            canonical_feedback_path(&author, &target, &a, &[a.clone(), b.clone()]),
            PathBuf::from(format!("agents/alice/feedback/foo/{}.md", a.as_str()))
        );
    }

    #[test]
    fn canonical_feedback_path_ad_hoc() {
        let target = FeedbackTarget::AdHoc;
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("codex").unwrap();
        assert_eq!(
            canonical_feedback_path(&author, &target, &sha, &[sha.clone()]),
            PathBuf::from("agents/codex/feedback/_/abcdef0.md")
        );
    }

    #[test]
    fn feedback_path_wire_prepends_dot_clank() {
        let target = FeedbackTarget::Plan(PlanKey::parse("foo").unwrap());
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        assert_eq!(
            feedback_path_wire(&author, &target, &sha, &[sha.clone()]),
            ".clank/agents/alice/feedback/foo/abcdef0.md"
        );
    }

    #[test]
    fn canonical_and_parse_round_trip() {
        let key = PlanKey::parse("foo").unwrap();
        let target = FeedbackTarget::Plan(key.clone());
        let sha = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let author = AgentLabel::parse("alice").unwrap();
        let built = canonical_feedback_path(&author, &target, &sha, &[sha.clone()]);
        let parsed = parse_feedback_path(&built).expect("round-trip parse");
        assert_eq!(parsed.author, author);
        assert_eq!(parsed.target, target);
        assert_eq!(parsed.target_ref.as_str(), "abcdef0");
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
}
