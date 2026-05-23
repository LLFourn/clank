//! Parse + validate feedback file bodies.
//!
//! A feedback file body is a markdown text whose first non-blank
//! line is the verdict header — exactly `APPROVE` or
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
//! let body = "APPROVE\n\nLGTM\n";
//! assert_eq!(parse_verdict(body), Verdict::Approve);
//!
//! let parsed = FeedbackBody::parse(body);
//! assert!(parsed.validate_matches(Verdict::Approve).is_ok());
//! assert!(parsed.validate_matches(Verdict::RequestChanges).is_err());
//! ```

use crate::vocab::Verdict;

/// Parse the first non-empty line of a feedback file body as a
/// verdict marker. Only exact uppercase tokens `APPROVE` /
/// `REQUEST_CHANGES` count; everything else is `Unmarked`.
/// Leading whitespace on the header line is tolerated.
pub fn parse_verdict(body: &str) -> Verdict {
    match body.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some("APPROVE") => Verdict::Approve,
        Some("REQUEST_CHANGES") => Verdict::RequestChanges,
        _ => Verdict::Unmarked,
    }
}

/// A parsed feedback body — the detected verdict header.
///
/// Use [`FeedbackBody::parse`] to construct, then
/// [`FeedbackBody::validate_matches`] when writing a file to
/// enforce that the body matches the verdict the caller claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackBody {
    pub verdict: Verdict,
}

impl FeedbackBody {
    pub fn parse(body: &str) -> Self {
        Self {
            verdict: parse_verdict(body),
        }
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
    /// most common case is `expected: Approve, actual: Unmarked`
    /// — caller forgot the `APPROVE` line.
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
                "verdict `unmarked` cannot be written; pass --verdict approve \
                 or --verdict request-changes",
            ),
        }
    }
}

impl std::error::Error for VerdictMismatch {}

fn header_for(v: Verdict) -> &'static str {
    match v {
        Verdict::Approve => "APPROVE",
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
    fn validate_matches_approve_ok() {
        let b = FeedbackBody::parse("APPROVE\n\nlgtm\n");
        assert!(b.validate_matches(Verdict::Approve).is_ok());
    }

    #[test]
    fn validate_matches_request_changes_ok() {
        let b = FeedbackBody::parse("REQUEST_CHANGES\n\nplease fix\n");
        assert!(b.validate_matches(Verdict::RequestChanges).is_ok());
    }

    #[test]
    fn validate_matches_rejects_mismatch() {
        let b = FeedbackBody::parse("APPROVE\nlgtm\n");
        let err = b.validate_matches(Verdict::RequestChanges).unwrap_err();
        assert!(matches!(
            err,
            VerdictMismatch::HeaderMismatch {
                expected: Verdict::RequestChanges,
                actual: Verdict::Approve
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("REQUEST_CHANGES"), "{msg}");
    }

    #[test]
    fn validate_matches_rejects_unmarked_body() {
        let b = FeedbackBody::parse("just prose\n");
        let err = b.validate_matches(Verdict::Approve).unwrap_err();
        assert!(matches!(
            err,
            VerdictMismatch::HeaderMismatch {
                expected: Verdict::Approve,
                actual: Verdict::Unmarked
            }
        ));
    }

    #[test]
    fn validate_matches_rejects_unmarked_expected() {
        let b = FeedbackBody::parse("APPROVE\n");
        let err = b.validate_matches(Verdict::Unmarked).unwrap_err();
        assert!(matches!(err, VerdictMismatch::UnmarkedNotAllowed));
    }
}
