//! Typed view of one plan's review feedback.
//!
//! Pure data. CLI scans
//! `.clank/agents/<author>/feedback/<plan>/<commit-ref>.md`
//! files (see `clank::feedback_scan`) and hands the result here;
//! `wait::compute_gate` (and `RepoState::derive_status`) consume it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, ContentHash};
use crate::vocab::Verdict;

/// Per-target filename mode for feedback paths.
///
/// `Short` ⇔ every commit in the scope has a unique 7-char
/// prefix, so the on-disk stem is `<full>[..7]`. `Long` ⇔ at
/// least two commits share the same 7-char prefix, so every
/// stem in that scope uses the full 40-char SHA.
///
/// Per-target, deterministic from the reviewable-commit list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilenameMode {
    Short,
    Long,
}

/// Compute the per-target filename mode from the reviewable
/// commits in that target's scope. The writer uses this to
/// pick the stem length; the reader doesn't need it (it just
/// parses any 7–40 hex stem and resolves via `CommitRef`).
pub fn filename_mode(reviewable_shas: &[CommitSha]) -> FilenameMode {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for sha in reviewable_shas {
        let short = &sha.as_str()[..7.min(sha.as_str().len())];
        if !seen.insert(short) {
            return FilenameMode::Long;
        }
    }
    FilenameMode::Short
}

/// Pick the on-disk stem for `target_sha` given the scope's
/// mode. Pure — the writer calls this; the reader doesn't.
pub fn filename_stem<'a>(target_sha: &'a CommitSha, mode: FilenameMode) -> &'a str {
    match mode {
        FilenameMode::Short => &target_sha.as_str()[..7.min(target_sha.as_str().len())],
        FilenameMode::Long => target_sha.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }

    #[test]
    fn empty_scope_is_short() {
        assert_eq!(filename_mode(&[]), FilenameMode::Short);
    }

    #[test]
    fn singleton_scope_is_short() {
        assert_eq!(filename_mode(&[sha("aaaa111")]), FilenameMode::Short);
    }

    #[test]
    fn unique_7_char_prefixes_are_short() {
        assert_eq!(
            filename_mode(&[sha("aaaa111"), sha("bbbb222"), sha("cccc333")]),
            FilenameMode::Short
        );
    }

    #[test]
    fn duplicate_7_char_prefix_forces_long() {
        // Two commits whose full SHAs share the same first 7 chars.
        let a = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let b = CommitSha::parse("abcdef0222222222222222222222222222222222").unwrap();
        assert_eq!(filename_mode(&[a, b]), FilenameMode::Long);
    }

    #[test]
    fn filename_stem_short_takes_first_seven() {
        let s = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        assert_eq!(filename_stem(&s, FilenameMode::Short), "abcdef0");
    }

    #[test]
    fn filename_stem_long_takes_full() {
        let s = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        assert_eq!(
            filename_stem(&s, FilenameMode::Long),
            "abcdef0111111111111111111111111111111111"
        );
    }
}

/// All feedback the CLI scanned for one plan, indexed by commit.
///
/// `per_commit` is in chronological order (same order as the plan's
/// reviewable timeline). Older commits feed the cumulative
/// participant set; the latest one's entries decide the gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackView {
    pub per_commit: Vec<CommitFeedback>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFeedback {
    pub sha: CommitSha,
    pub entries: BTreeMap<AgentLabel, FeedbackEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackEntry {
    pub verdict: Verdict,
    pub body_hash: ContentHash,
    /// Path relative to the repo root. Round-trips for the CLI to
    /// re-read when sealing approvals.
    pub source_path: String,
}
