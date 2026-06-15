//! Pure types + parsers for clank-pr-review-mode's local state
//! (`.clank/pr-reviews/<pr>/`).
//!
//! The GitHub pending review holds the comment substance; these
//! files are the AUTHORITATIVE verdict + round-coordination layer
//! that `clank status`, the gate, and the wake all read. No git IO,
//! no GitHub IO — the CLI does the filesystem/network; this module
//! is the wire shapes and the convergence arithmetic.

use serde::{Deserialize, Serialize};

use crate::ids::AgentLabel;
use crate::vocab::Verdict;

/// `.clank/pr-reviews/<pr>/pr.json` — the review target plus the
/// round/submit coordination state.
///
/// Both id forms are stored because they live in different API
/// namespaces (the node-vs-numeric footgun): `review_id` is the
/// NUMERIC id REST submit/delete take; `review_node_id` is the
/// GraphQL node id reply-creation takes. Both are `None` until the
/// pending review is created (a later phase).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewState {
    /// `owner/name` slug the gh API calls target.
    pub repo: String,
    /// PR number.
    pub number: u32,
    /// Pinned `pull/<n>/head` sha; comments anchor here.
    pub head_sha: String,
    /// Numeric id of the team's single pending review (REST).
    #[serde(default)]
    pub review_id: Option<u64>,
    /// GraphQL node id of the same review (reply creation).
    #[serde(default)]
    pub review_node_id: Option<String>,
    /// Master's current revision counter. Bumped whenever master
    /// changes the pending comments; reviewers stamp the round they
    /// reviewed so a post-approval edit reopens the gate.
    #[serde(default)]
    pub round: u64,
    /// TOCTOU freeze during submit: reviewers hold off while true.
    #[serde(default)]
    pub submitting: bool,
}

impl PrReviewState {
    /// Fresh state at round 0 with no pending review yet.
    pub fn new(repo: impl Into<String>, number: u32, head_sha: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            number,
            head_sha: head_sha.into(),
            review_id: None,
            review_node_id: None,
            round: 0,
            submitting: false,
        }
    }
}

/// One reviewer's `reviews/<label>.md`: the authoritative verdict
/// for the round they last reviewed, plus a free-text summary.
///
/// On-disk format is a tiny `key: value` frontmatter terminated by
/// a blank line, then the summary body:
///
/// ```text
/// verdict: finished
/// round: 3
///
/// codex: gh incantations all correct; ready to publish.
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerVerdict {
    pub verdict: Verdict,
    pub reviewed_round: u64,
    pub summary: String,
}

/// Why a `reviews/<label>.md` file couldn't be parsed. Callers
/// surface this as a corrupt-file diagnostic, never a silent
/// default (a misread verdict could publish prematurely).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewerVerdictError {
    MissingVerdict,
    MissingRound,
    UnknownVerdict(String),
    BadRound(String),
}

impl std::fmt::Display for ReviewerVerdictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewerVerdictError::MissingVerdict => f.write_str("missing `verdict:` field"),
            ReviewerVerdictError::MissingRound => f.write_str("missing `round:` field"),
            ReviewerVerdictError::UnknownVerdict(v) => write!(
                f,
                "unknown verdict `{v}` (want approve|finished|request_changes)"
            ),
            ReviewerVerdictError::BadRound(v) => {
                write!(f, "round `{v}` is not a non-negative integer")
            }
        }
    }
}

impl std::error::Error for ReviewerVerdictError {}

impl ReviewerVerdict {
    pub fn new(verdict: Verdict, reviewed_round: u64, summary: impl Into<String>) -> Self {
        Self {
            verdict,
            reviewed_round,
            summary: summary.into(),
        }
    }

    /// Parse the on-disk file body. Frontmatter keys are
    /// case-insensitive on the key; the verdict value matches the
    /// snake_case `Verdict::as_str` vocab. `Unmarked` is rejected —
    /// a reviewer file always carries a real verdict.
    pub fn parse(body: &str) -> Result<Self, ReviewerVerdictError> {
        let mut verdict: Option<Verdict> = None;
        let mut round: Option<u64> = None;
        let mut frontmatter_done = false;
        let mut summary_lines: Vec<&str> = Vec::new();
        for line in body.lines() {
            if frontmatter_done {
                summary_lines.push(line);
                continue;
            }
            if line.trim().is_empty() {
                frontmatter_done = true;
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                // A non-blank, non-`key: value` line ends the
                // frontmatter and is the first summary line.
                frontmatter_done = true;
                summary_lines.push(line);
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "verdict" => {
                    verdict = Some(match value {
                        "approve" => Verdict::Approve,
                        "finished" => Verdict::Finished,
                        "request_changes" => Verdict::RequestChanges,
                        other => return Err(ReviewerVerdictError::UnknownVerdict(other.into())),
                    })
                }
                "round" => {
                    round = Some(
                        value
                            .parse()
                            .map_err(|_| ReviewerVerdictError::BadRound(value.into()))?,
                    )
                }
                _ => {}
            }
        }
        Ok(Self {
            verdict: verdict.ok_or(ReviewerVerdictError::MissingVerdict)?,
            reviewed_round: round.ok_or(ReviewerVerdictError::MissingRound)?,
            summary: summary_lines.join("\n").trim().to_string(),
        })
    }

    /// Render to the on-disk format. Round-trips through `parse`.
    pub fn render(&self) -> String {
        format!(
            "verdict: {}\nround: {}\n\n{}\n",
            self.verdict.as_str(),
            self.reviewed_round,
            self.summary.trim()
        )
    }

    /// A reviewer is CURRENT for `round` when they reviewed exactly
    /// it — an older `reviewed_round` means master has revised since
    /// and the approval is stale.
    pub fn is_current(&self, round: u64) -> bool {
        self.reviewed_round == round
    }
}

/// Reviewers not yet satisfied for `round`: those missing a verdict
/// file, those whose `reviewed_round` is behind (stale), or those
/// whose current verdict is not FINISHED. The authoritative
/// "who are we waiting on" for `clank status` and the gate.
///
/// `reviewers` is the team's reviewer set; `verdicts` is whatever
/// was parsed from `reviews/*.md` (absent label = no file yet).
pub fn pending_reviewers(
    round: u64,
    reviewers: &[AgentLabel],
    verdicts: &[(AgentLabel, ReviewerVerdict)],
) -> Vec<AgentLabel> {
    reviewers
        .iter()
        .filter(|label| {
            match verdicts.iter().find(|(l, _)| l == *label) {
                Some((_, v)) => !(v.is_current(round) && v.verdict == Verdict::Finished),
                None => true, // no file → still waiting
            }
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(s: &str) -> AgentLabel {
        AgentLabel::parse(s).unwrap()
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut s = PrReviewState::new("owner/repo", 123, "deadbeef");
        s.round = 4;
        s.review_id = Some(4500050869);
        s.review_node_id = Some("PRR_kwabc".into());
        s.submitting = true;
        let json = serde_json::to_string(&s).unwrap();
        let back: PrReviewState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn state_tolerates_missing_optionals() {
        // A pr.json written before review creation: no ids, no flag.
        let json = r#"{"repo":"o/r","number":7,"head_sha":"abc"}"#;
        let s: PrReviewState = serde_json::from_str(json).unwrap();
        assert_eq!(s.review_id, None);
        assert_eq!(s.review_node_id, None);
        assert_eq!(s.round, 0);
        assert!(!s.submitting);
    }

    #[test]
    fn verdict_round_trips() {
        for v in [Verdict::Approve, Verdict::Finished, Verdict::RequestChanges] {
            let rv = ReviewerVerdict::new(v, 3, "a multi-line\n\nsummary here");
            let parsed = ReviewerVerdict::parse(&rv.render()).unwrap();
            assert_eq!(parsed, rv, "round-trip for {v:?}");
        }
    }

    #[test]
    fn verdict_parses_real_file() {
        let body = "verdict: finished\nround: 5\n\nready to publish.\n";
        let rv = ReviewerVerdict::parse(body).unwrap();
        assert_eq!(rv.verdict, Verdict::Finished);
        assert_eq!(rv.reviewed_round, 5);
        assert_eq!(rv.summary, "ready to publish.");
    }

    #[test]
    fn verdict_rejects_corrupt_files() {
        assert_eq!(
            ReviewerVerdict::parse("round: 1\n\nx"),
            Err(ReviewerVerdictError::MissingVerdict)
        );
        assert_eq!(
            ReviewerVerdict::parse("verdict: finished\n\nx"),
            Err(ReviewerVerdictError::MissingRound)
        );
        assert_eq!(
            ReviewerVerdict::parse("verdict: lgtm\nround: 1\n"),
            Err(ReviewerVerdictError::UnknownVerdict("lgtm".into()))
        );
        assert_eq!(
            ReviewerVerdict::parse("verdict: finished\nround: soon\n"),
            Err(ReviewerVerdictError::BadRound("soon".into()))
        );
    }

    #[test]
    fn pending_excludes_only_current_finished() {
        let reviewers = [label("codex"), label("ruthless")];
        // codex finished THIS round; ruthless finished an OLD round.
        let verdicts = [
            (
                label("codex"),
                ReviewerVerdict::new(Verdict::Finished, 3, ""),
            ),
            (
                label("ruthless"),
                ReviewerVerdict::new(Verdict::Finished, 2, ""),
            ),
        ];
        // ruthless is stale (reviewed round 2, we're on 3) → still pending.
        assert_eq!(
            pending_reviewers(3, &reviewers, &verdicts),
            vec![label("ruthless")]
        );
    }

    #[test]
    fn pending_includes_missing_and_request_changes() {
        let reviewers = [label("codex"), label("ruthless")];
        let verdicts = [(
            label("codex"),
            ReviewerVerdict::new(Verdict::RequestChanges, 3, "no"),
        )];
        // codex wants changes (current round), ruthless has no file →
        // both pending.
        let pending = pending_reviewers(3, &reviewers, &verdicts);
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&label("codex")));
        assert!(pending.contains(&label("ruthless")));
    }

    #[test]
    fn converged_when_all_current_and_finished() {
        let reviewers = [label("codex"), label("ruthless")];
        let verdicts = [
            (
                label("codex"),
                ReviewerVerdict::new(Verdict::Finished, 3, ""),
            ),
            (
                label("ruthless"),
                ReviewerVerdict::new(Verdict::Finished, 3, ""),
            ),
        ];
        assert!(pending_reviewers(3, &reviewers, &verdicts).is_empty());
    }
}
