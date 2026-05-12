//! Pure status derivation for a `feedback_files` row. The single
//! function lives here so MCP (`get_context`) and the web UI render
//! identical statuses; the watcher-ingest path also uses this when
//! materialising a `FeedbackFileSnapshot`.
//!
//! The status is read-time only — never persisted. It compares the
//! row's columns against the kind's *expected* target (the latest
//! plan_revision for `plan` rows; the HEAD-derived implementation
//! commit for `impl` rows). Comparing against the wrong target was
//! the load-bearing bug in earlier review rounds; centralising the
//! rule here means a future surface (SSE, CLI, etc.) can't drift.

use crate::daemon::service::ActiveTarget;
use crate::domain::FeedbackFileStatus;
use crate::storage::feedback_files::FeedbackFile;

/// Resolve a `feedback_files` row to its derived status.
///
/// - `!file_exists` (file gone from disk at read time, even if the
///   watcher hasn't processed the delete yet) → `Missing`. Read time
///   is the source of truth so `get_context` cannot return
///   `exists: false` with `status: current` during the debounce
///   window or after a missed event.
/// - `parse_error.is_some()` → `ParseError`.
/// - `last_observed_hash.is_none()` (watcher saw deletion) → `Missing`.
/// - Ingest target matches expected AND `last_ingested_hash == last_observed_hash` → `Current`.
/// - Has been ingested at least once but the above doesn't hold → `Stale`.
/// - Otherwise → `NotYetIngested`.
pub fn derive_feedback_file_status(
    row: &FeedbackFile,
    expected_target_for_row_kind: Option<&ActiveTarget>,
    file_exists: bool,
) -> FeedbackFileStatus {
    if !file_exists {
        return FeedbackFileStatus::Missing;
    }
    if row.parse_error.is_some() {
        return FeedbackFileStatus::ParseError;
    }
    if row.last_observed_hash.is_none() {
        return FeedbackFileStatus::Missing;
    }
    let target_matches = match (
        row.last_ingested_target_kind.as_deref(),
        row.last_ingested_target_id.as_deref(),
        expected_target_for_row_kind,
    ) {
        (Some(k), Some(id), Some(target)) => k == target.kind.as_str() && id == target.id,
        _ => false,
    };
    let hash_synced = match (&row.last_ingested_hash, &row.last_observed_hash) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if target_matches && hash_synced {
        FeedbackFileStatus::Current
    } else if row.last_ingested_hash.is_some() {
        FeedbackFileStatus::Stale
    } else {
        FeedbackFileStatus::NotYetIngested
    }
}
