//! Parse + validate feedback file bodies.
//!
//! A feedback file body is a markdown text whose first non-blank
//! line is the verdict header — exactly `CONTINUE`, `FINISHED`, or
//! `REQUEST_CHANGES`. Anything else parses as [`Verdict::Unmarked`].
//!
//! Used by:
//! - `clank feedback write` — validates that the body the user
//!   passes matches the `--verdict` they claim.
//! - The feedback scanner (via [`parse_verdict`]) — derives a
//!   verdict for already-on-disk files.
//!
//! Pure: no I/O, no clock.
//!
//! Examples:
//!
//! ```
//! use clank_core::feedback_body::{FeedbackBody, parse_verdict};
//! use clank_core::Verdict;
//!
//! let body = "CONTINUE\n\nLGTM\n";
//! assert_eq!(parse_verdict(body), Verdict::Continue);
//!
//! let parsed = FeedbackBody::parse(body);
//! assert!(parsed.validate_matches(Verdict::Continue).is_ok());
//! assert!(parsed.validate_matches(Verdict::RequestChanges).is_err());
//! ```

use crate::vocab::Verdict;

/// Exact on-disk marker for a gate-preservation record written when a
/// reviewer joins the roster after a commit was made. These records carry a
/// verdict so the existing gate fold can consume them, but they are not human
/// reviews. Keep the marker on its own line so prose cannot match by accident.
pub const ROSTER_STAND_IN_MARKER: &str = "<!-- clank:roster-stand-in:v1 -->";

/// Whether `body` is a synthetic roster stand-in rather than a human review.
pub fn is_roster_stand_in(body: &str) -> bool {
    body.lines()
        .any(|line| line.trim() == ROSTER_STAND_IN_MARKER)
}

/// Parse the first non-empty line of a feedback file body as a
/// verdict marker. The line starts with `CONTINUE`, `FINISHED`, or
/// `REQUEST_CHANGES`, optionally followed by a space and a
/// one-line summary. Everything else is `Unmarked`.
///
/// Legacy `APPROVE` (the pre-rename name of `CONTINUE`) is still
/// accepted — feedback files written before the APPROVE→CONTINUE
/// rename must keep gating identically. This is the load-bearing
/// back-compat: it's an on-disk format, not serde.
pub fn parse_verdict(body: &str) -> Verdict {
    let first = body.lines().map(str::trim).find(|line| !line.is_empty());
    match first {
        Some(l) if l == "CONTINUE" || l.starts_with("CONTINUE ") => Verdict::Continue,
        Some(l) if l == "APPROVE" || l.starts_with("APPROVE ") => Verdict::Continue,
        Some(l) if l == "FINISHED" || l.starts_with("FINISHED ") => Verdict::Finished,
        Some(l) if l == "REQUEST_CHANGES" || l.starts_with("REQUEST_CHANGES ") => {
            Verdict::RequestChanges
        }
        _ => Verdict::Unmarked,
    }
}

/// Extract the one-line summary from the verdict line.
/// Returns the text after the verdict keyword, or empty string
/// if no summary is present.
pub fn parse_summary(body: &str) -> &str {
    let first = body.lines().map(str::trim).find(|line| !line.is_empty());
    match first {
        Some(l) if l.starts_with("CONTINUE ") => l["CONTINUE ".len()..].trim(),
        Some(l) if l.starts_with("APPROVE ") => l["APPROVE ".len()..].trim(),
        Some(l) if l.starts_with("FINISHED ") => l["FINISHED ".len()..].trim(),
        Some(l) if l.starts_with("REQUEST_CHANGES ") => l["REQUEST_CHANGES ".len()..].trim(),
        _ => "",
    }
}

/// Strip a leading restatement of `verdict` from a review message
/// (`"CONTINUE: looks good"` → `"looks good"`). Reviewers habitually
/// restate the verdict at the head of their message even though the
/// file header already carries it; the declared verdict is the single
/// source of truth, so `feedback write` normalizes the duplicate away
/// at compose time.
///
/// Deliberately conservative: the token must match the DECLARED
/// verdict (case-insensitive; `REQUEST_CHANGES` also with space or
/// hyphen between the words) and must be followed by a punctuation
/// separator (`:`, `—`, `–`, `-`) — a bare verdict word followed by
/// prose ("Continue polishing the API") is a summary, not a
/// restatement. Strips once, at the very start only; everything after
/// the separator (including newlines — a body must not be promoted
/// into the summary line) is returned as-is.
pub fn strip_verdict_restatement(verdict: Verdict, message: &str) -> &str {
    let Some(after_token) = strip_token(verdict, message) else {
        return message;
    };
    let at_sep = after_token.trim_start_matches([' ', '\t']);
    let Some(after_sep) = at_sep.strip_prefix([':', '—', '–', '-']) else {
        return message;
    };
    after_sep.trim_start_matches([' ', '\t'])
}

/// Case-insensitive match of `verdict`'s token at the start of `s`,
/// returning the remainder. `None` for no match (including
/// [`Verdict::Unmarked`], which has no token).
fn strip_token(verdict: Verdict, s: &str) -> Option<&str> {
    fn strip_ci<'a>(s: &'a str, token: &str) -> Option<&'a str> {
        let (head, tail) = s.split_at_checked(token.len())?;
        head.eq_ignore_ascii_case(token).then_some(tail)
    }
    match verdict {
        Verdict::Continue => strip_ci(s, "CONTINUE"),
        Verdict::Finished => strip_ci(s, "FINISHED"),
        Verdict::RequestChanges => ["REQUEST_CHANGES", "REQUEST CHANGES", "REQUEST-CHANGES"]
            .iter()
            .find_map(|t| strip_ci(s, t)),
        Verdict::Unmarked => None,
    }
}

/// A parsed feedback body — the detected verdict header.
///
/// Use [`FeedbackBody::parse`] to construct, then
/// [`FeedbackBody::validate_matches`] when writing a file to
/// enforce that the body matches the verdict the caller claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackBody {
    pub verdict: Verdict,
    body: String,
}

impl FeedbackBody {
    pub fn parse(body: &str) -> Self {
        Self {
            verdict: parse_verdict(body),
            body: body.to_string(),
        }
    }

    /// One-line summary from the verdict line. Returns the text
    /// after `CONTINUE ` / `REQUEST_CHANGES ` on the first line.
    pub fn summary(&self) -> String {
        parse_summary(&self.body).to_string()
    }

    /// Full review body after the verdict+summary line, trimmed.
    pub fn details(&self) -> String {
        self.body
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    /// True for a machine-written roster stand-in. Callers that display or
    /// audit feedback can distinguish non-participation from an actual review
    /// without interpreting prose.
    pub fn is_roster_stand_in(&self) -> bool {
        is_roster_stand_in(&self.body)
    }

    /// Confirm that the parsed verdict matches what the caller
    /// claimed. `expected` of `Verdict::Unmarked` is rejected —
    /// you cannot intentionally write an unmarked feedback file.
    pub fn validate_matches(&self, expected: Verdict) -> Result<(), VerdictMismatch> {
        if expected == Verdict::Unmarked {
            return Err(VerdictMismatch::UnmarkedNotAllowed);
        }
        if self.verdict != expected {
            return Err(VerdictMismatch::HeaderMismatch {
                expected,
                actual: self.verdict,
            });
        }
        Ok(())
    }
}

/// Why a feedback body's header didn't match what was claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictMismatch {
    /// The body's verdict header doesn't match `expected`. The
    /// most common case is `expected: Continue, actual: Unmarked`
    /// — caller forgot the `CONTINUE` line.
    HeaderMismatch { expected: Verdict, actual: Verdict },
    /// Caller asked to write a file with `Verdict::Unmarked`. We
    /// don't accept that — every written file must carry a real
    /// verdict.
    UnmarkedNotAllowed,
}

impl std::fmt::Display for VerdictMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerdictMismatch::HeaderMismatch { expected, actual } => write!(
                f,
                "expected first non-blank line to be `{header}` (matching \
                 --verdict {expected}); found verdict `{actual}` instead",
                header = header_for(*expected),
                expected = expected,
                actual = actual,
            ),
            VerdictMismatch::UnmarkedNotAllowed => f.write_str(
                "verdict `unmarked` cannot be written; pass --verdict continue \
                 or --verdict request-changes",
            ),
        }
    }
}

impl std::error::Error for VerdictMismatch {}

fn header_for(v: Verdict) -> &'static str {
    match v {
        Verdict::Continue => "CONTINUE",
        Verdict::Finished => "FINISHED",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Unmarked => {
            debug_assert!(false, "header_for(Unmarked) — guarded by validate_matches");
            "(unmarked)"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_continue() {
        assert_eq!(parse_verdict("CONTINUE\n\nbody\n"), Verdict::Continue);
    }

    #[test]
    fn roster_stand_in_requires_exact_marker_line() {
        let body = format!("CONTINUE synthetic\n\n{ROSTER_STAND_IN_MARKER}\nNo review.\n");
        let parsed = FeedbackBody::parse(&body);
        assert!(parsed.is_roster_stand_in());
        assert!(is_roster_stand_in(&body));
        assert!(!is_roster_stand_in(
            "CONTINUE\n\nmentions <!-- clank:roster-stand-in:v1 --> in prose\n"
        ));
    }

    #[test]
    fn parse_verdict_legacy_approve_is_continue() {
        // Back-compat: feedback files written before the APPROVE→CONTINUE
        // rename must still gate as Continue (on-disk format, not serde).
        assert_eq!(parse_verdict("APPROVE\n\nbody\n"), Verdict::Continue);
        assert_eq!(parse_verdict("APPROVE looks good\n"), Verdict::Continue);
        assert_eq!(parse_summary("APPROVE looks good\n"), "looks good");
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
            parse_verdict("\n\n   CONTINUE   \n\nbody\n"),
            Verdict::Continue
        );
    }

    #[test]
    fn parse_verdict_lowercase_is_unmarked() {
        assert_eq!(parse_verdict("continue\n\nbody\n"), Verdict::Unmarked);
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
    fn strip_verdict_restatement_strips_matching_prefixes() {
        use Verdict::*;
        for (v, msg, want) in [
            (Continue, "CONTINUE: looks good", "looks good"),
            (Continue, "Continue: looks good", "looks good"),
            (Continue, "continue — looks good", "looks good"),
            (Continue, "CONTINUE : spaced separator", "spaced separator"),
            (Finished, "FINISHED — ship it", "ship it"),
            (Finished, "Finished: ship it", "ship it"),
            (RequestChanges, "REQUEST_CHANGES: fix foo", "fix foo"),
            (RequestChanges, "Request changes: fix foo", "fix foo"),
            (RequestChanges, "request-changes: fix foo", "fix foo"),
            (RequestChanges, "REQUEST_CHANGES – fix foo", "fix foo"),
        ] {
            assert_eq!(strip_verdict_restatement(v, msg), want, "input: {msg}");
        }
    }

    #[test]
    fn strip_verdict_restatement_leaves_non_restatements() {
        use Verdict::*;
        for (v, msg) in [
            // No separator: legitimate prose starting with the word.
            (Continue, "Continue polishing the API"),
            // Restated verdict doesn't match the declared one.
            (Continue, "FINISHED: wrong verdict"),
            (Finished, "CONTINUE: wrong verdict"),
            // Word boundary: the token inside a longer word.
            (Continue, "Continued: past tense"),
            // Not at the start.
            (Continue, "looks good; CONTINUE: later"),
            // On a later line.
            (Continue, "summary\nCONTINUE: details"),
            // Unmarked has no token.
            (Unmarked, "CONTINUE: anything"),
        ] {
            assert_eq!(strip_verdict_restatement(v, msg), msg, "input: {msg}");
        }
    }

    #[test]
    fn strip_verdict_restatement_preserves_the_body() {
        // The body after the summary line rides along verbatim.
        assert_eq!(
            strip_verdict_restatement(Verdict::Continue, "Continue: ok\n\nDetails."),
            "ok\n\nDetails."
        );
        // First line empties: the remainder keeps its leading newlines
        // so the body is NOT promoted into the summary line.
        assert_eq!(
            strip_verdict_restatement(Verdict::Continue, "CONTINUE:\n\nDetails."),
            "\n\nDetails."
        );
        // Empty remainder is allowed (header-only first line).
        assert_eq!(
            strip_verdict_restatement(Verdict::Continue, "CONTINUE:"),
            ""
        );
    }

    #[test]
    fn validate_matches_continue_ok() {
        let b = FeedbackBody::parse("CONTINUE\n\nlgtm\n");
        assert!(b.validate_matches(Verdict::Continue).is_ok());
    }

    #[test]
    fn validate_matches_request_changes_ok() {
        let b = FeedbackBody::parse("REQUEST_CHANGES\n\nplease fix\n");
        assert!(b.validate_matches(Verdict::RequestChanges).is_ok());
    }

    #[test]
    fn validate_matches_rejects_mismatch() {
        let b = FeedbackBody::parse("CONTINUE\nlgtm\n");
        let err = b.validate_matches(Verdict::RequestChanges).unwrap_err();
        assert!(matches!(
            err,
            VerdictMismatch::HeaderMismatch {
                expected: Verdict::RequestChanges,
                actual: Verdict::Continue
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("REQUEST_CHANGES"), "{msg}");
    }

    #[test]
    fn validate_matches_rejects_unmarked_body() {
        let b = FeedbackBody::parse("just prose\n");
        let err = b.validate_matches(Verdict::Continue).unwrap_err();
        assert!(matches!(
            err,
            VerdictMismatch::HeaderMismatch {
                expected: Verdict::Continue,
                actual: Verdict::Unmarked
            }
        ));
    }

    #[test]
    fn validate_matches_rejects_unmarked_expected() {
        let b = FeedbackBody::parse("CONTINUE\n");
        let err = b.validate_matches(Verdict::Unmarked).unwrap_err();
        assert!(matches!(err, VerdictMismatch::UnmarkedNotAllowed));
    }
}
