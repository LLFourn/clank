//! Small leaf helpers used across components. Kept tiny by design —
//! prefer extending here over duplicating per file.

/// First 8 chars of a commit SHA (or the full string if shorter).
pub fn short_sha(sha: &str) -> String {
    if sha.len() > 8 {
        sha[..8].to_string()
    } else {
        sha.to_string()
    }
}
