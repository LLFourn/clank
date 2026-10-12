//! Each agent's nudge history: one line per attempt to push work into its
//! terminal, whatever made the attempt.
//!
//! History, never work. It is its own journal beside the agent's events,
//! not an event source, so nothing here can be delivered, acked, counted
//! as a receipt, or replied to. A record is written once the attempt's
//! outcome is fixed; writing it can delay its caller by one appended line
//! and never changes the outcome.

use std::path::{Path, PathBuf};

/// What made the attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Trigger {
    /// `clank nudge`.
    Manual,
    /// A retry after the agent's turn failed on a provider error.
    Retry {
        class: String,
        attempt: u32,
        of: u32,
    },
}

/// What the attempt's transport could tell. Only `Submitted` means the
/// transport accepted the text, and not even that says the agent read it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// Nothing reached the harness.
    Refused { reason: String },
    /// The transport accepted it.
    Submitted,
    /// A recovery helper said it was submitting the wake. It says so before
    /// it exits to wake the harness, so the wake itself is unconfirmed.
    Reported,
    /// It may have reached the harness.
    Indeterminate { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    /// Unix seconds.
    pub(crate) at: u64,
    /// The attempt's identity: distinct for every manual nudge, the
    /// recovery attempt's own for a retry.
    pub(crate) attempt: String,
    pub(crate) trigger: Trigger,
    pub(crate) text: String,
    pub(crate) outcome: Outcome,
}

impl Record {
    /// The row's words after the agent: what happened, then why.
    pub(crate) fn summary(&self) -> String {
        let what = match &self.outcome {
            Outcome::Submitted => "nudged".to_string(),
            Outcome::Reported => "nudge reported by helper · wake unconfirmed".to_string(),
            Outcome::Refused { reason } => format!("nudge refused: {reason}"),
            Outcome::Indeterminate { reason } => format!("nudge indeterminate: {reason}"),
        };
        match &self.trigger {
            Trigger::Manual => format!("{what} · by hand"),
            Trigger::Retry { class, attempt, of } => {
                format!("{what} · retry {attempt}/{of} after {class}")
            }
        }
    }
}

/// A distinct identity for one manual attempt.
pub(crate) fn manual_attempt() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("manual:{}:{nanos}", std::process::id())
}

pub(crate) fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("nudges.jsonl")
}

/// Append `record`, as one write of one line, never retried: a short write
/// leaves a torn record, which the reader skips. Its failure is the
/// caller's to ignore: the attempt it describes has already happened.
pub(crate) fn append(agent_dir: &Path, record: &Record) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::FileExt as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path(agent_dir))?;
    let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    line.push(b'\n');
    // A torn line from an interrupted append ends without a newline; start
    // on a fresh one so this record is not glued onto it.
    let len = file.metadata()?.len();
    let mut last = [0u8];
    if len > 0 && file.read_at(&mut last, len - 1).is_ok() && last[0] != b'\n' {
        line.insert(0, b'\n');
    }
    let written = file.write(&line)?;
    if written < line.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "the record was written short",
        ));
    }
    Ok(())
}

/// Every whole record, oldest first, each attempt once. A malformed or
/// torn line is skipped.
pub(crate) fn read(agent_dir: &Path) -> Vec<Record> {
    // Bytes, line by line: a record torn inside a character is invalid
    // text, and must cost only itself.
    let Ok(bytes) = std::fs::read(path(agent_dir)) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Record>(line).ok())
        .filter(|r| seen.insert(r.attempt.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(attempt: &str, at: u64, text: &str) -> Record {
        Record {
            at,
            attempt: attempt.into(),
            trigger: Trigger::Manual,
            text: text.into(),
            outcome: Outcome::Submitted,
        }
    }

    /// Records read back as written, multiline text whole, each attempt
    /// once however often its line appears; two attempts with the same
    /// text and time stay two.
    #[test]
    fn the_journal_reads_back_each_attempt_once() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record("a", 1, "line one\nline two")).unwrap();
        append(dir.path(), &record("a", 1, "line one\nline two")).unwrap();
        append(dir.path(), &record("b", 1, "line one\nline two")).unwrap();
        let read = read(dir.path());
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].text, "line one\nline two");
        assert_eq!(read[1].attempt, "b");
    }

    /// A record torn inside a character costs only itself.
    #[test]
    fn a_record_torn_inside_a_character_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record("a", 1, "x")).unwrap();
        let mut torn = serde_json::to_vec(&record("t", 2, "naïve")).unwrap();
        let split = torn.iter().position(|b| *b == 0xC3).unwrap() + 1;
        torn.truncate(split);
        let mut bytes = std::fs::read(path(dir.path())).unwrap();
        bytes.extend(torn);
        std::fs::write(path(dir.path()), bytes).unwrap();
        append(dir.path(), &record("b", 3, "y")).unwrap();
        let attempts: Vec<String> = read(dir.path()).into_iter().map(|r| r.attempt).collect();
        assert_eq!(attempts, ["a", "b"]);
    }

    /// A torn last line, and a line that is not a record, are skipped; the
    /// next append starts a fresh line rather than joining the torn one.
    #[test]
    fn a_torn_or_foreign_line_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &record("a", 1, "x")).unwrap();
        std::fs::write(
            path(dir.path()),
            [
                std::fs::read(path(dir.path())).unwrap(),
                b"not a record\n{\"at\":2,\"attem".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        append(dir.path(), &record("b", 3, "y")).unwrap();
        let attempts: Vec<String> = read(dir.path()).into_iter().map(|r| r.attempt).collect();
        assert_eq!(attempts, ["a", "b"]);
    }

    /// The row says what happened: only an accepted submission is
    /// "nudged", and a helper's report never claims the wake.
    #[test]
    fn the_summary_claims_no_more_than_the_outcome() {
        let mut r = record("a", 1, "x");
        assert_eq!(r.summary(), "nudged · by hand");
        r.outcome = Outcome::Reported;
        r.trigger = Trigger::Retry {
            class: "overloaded".into(),
            attempt: 2,
            of: 3,
        };
        assert_eq!(
            r.summary(),
            "nudge reported by helper · wake unconfirmed · retry 2/3 after overloaded"
        );
        r.outcome = Outcome::Indeterminate {
            reason: "the interrupt was not confirmed".into(),
        };
        assert!(
            r.summary()
                .starts_with("nudge indeterminate: the interrupt")
        );
    }
}
