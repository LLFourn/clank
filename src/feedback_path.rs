//! Single owner of `.trinity/feedback/<session_id>/<kind>/<author>.md`
//! path construction and reverse parsing. Watcher, `register_plan_file`,
//! `get_context`, UI, and tests all go through here — there is no other
//! place in the codebase that builds these paths by string concatenation.

use std::path::{Path, PathBuf};

use crate::domain::FeedbackKind;
use crate::lifecycle::{AgentLabel, SessionId};

/// `<repo_root>/.trinity/feedback/<session_id>/`. The session-level
/// container; not directly watched.
pub fn feedback_root(repo_root: &Path, session_id: &SessionId) -> PathBuf {
    repo_root
        .join(".trinity")
        .join("feedback")
        .join(session_id.as_str())
}

/// `<repo_root>/.trinity/feedback/<session_id>/<kind>/`. Watched by
/// the feedback dispatcher after `register_plan_file`.
pub fn feedback_dir(repo_root: &Path, session_id: &SessionId, kind: FeedbackKind) -> PathBuf {
    feedback_root(repo_root, session_id).join(kind.as_str())
}

/// Convention path for a specific `(session, kind, author)` triple.
pub fn feedback_file_path(
    repo_root: &Path,
    session_id: &SessionId,
    kind: FeedbackKind,
    author: &AgentLabel,
) -> PathBuf {
    feedback_dir(repo_root, session_id, kind).join(format!("{}.md", author.as_str()))
}

/// Same slug rules as `SessionId`: ASCII alphanumerics + `_-.`,
/// 1..=64 chars.
pub fn is_valid_slug(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}
