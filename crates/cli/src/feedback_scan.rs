//! CLI-side IO: scan `.clank/feedback/<plan>/<sha>/<author>.md`
//! into a typed [`FeedbackView`]. Both `status` / `wfw` and the
//! `preview` gate-computation consume the result.
//!
//! Pure interpretation of the parsed bodies (verdict → state
//! machine, participant set, etc.) lives in `clank_core::plan_view`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::disk_format::parse_verdict;
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use clank_core::feedback_view::{CommitFeedback, FeedbackEntry, FeedbackView};

#[derive(Debug, thiserror::Error)]
pub enum FeedbackScanError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Build a `FeedbackView` for `plan` by reading the per-commit
/// feedback directories under `.clank/feedback/<plan>/<sha>/`.
///
/// `reviewable_shas` is the cumulative list of reviewable commits
/// (chronological); pass `state.fold.plans[plan].commits` filtered
/// to `touched_plan || touched_code`. Result `per_commit` is in
/// the same order.
///
/// Missing directories are treated as "no feedback at that sha,"
/// not as errors. Unreadable files are silently skipped (parity
/// with the preview gate scanner this replaces).
pub fn scan_feedback(
    repo: &Path,
    plan: &PlanKey,
    reviewable_shas: &[CommitSha],
) -> Result<FeedbackView, FeedbackScanError> {
    let mut per_commit = Vec::with_capacity(reviewable_shas.len());
    for sha in reviewable_shas {
        let dir = repo
            .join(".clank/feedback")
            .join(plan.as_str())
            .join(sha.as_str());
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                per_commit.push(CommitFeedback {
                    sha: sha.clone(),
                    entries: BTreeMap::new(),
                });
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut commit_entries: BTreeMap<AgentLabel, FeedbackEntry> = BTreeMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(author) = AgentLabel::parse(stem) else {
                continue;
            };
            let body = match std::fs::read_to_string(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(repo)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            commit_entries.insert(
                author,
                FeedbackEntry {
                    verdict: parse_verdict(&body),
                    body_hash: content_hash(&body),
                    source_path: rel,
                },
            );
        }
        per_commit.push(CommitFeedback {
            sha: sha.clone(),
            entries: commit_entries,
        });
    }
    Ok(FeedbackView { per_commit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, body).unwrap();
    }

    fn sha(s: &str) -> CommitSha {
        CommitSha::parse(&format!("{s:0<40}")).unwrap()
    }

    #[test]
    fn scans_two_commits_with_interleaved_authors() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = sha("1111");
        let s2 = sha("2222");
        write(
            repo,
            &format!(".clank/feedback/foo/{}/alice.md", s1.as_str()),
            "APPROVE\n\nlgtm\n",
        );
        write(
            repo,
            &format!(".clank/feedback/foo/{}/bob.md", s1.as_str()),
            "REQUEST_CHANGES\n\nnope\n",
        );
        write(
            repo,
            &format!(".clank/feedback/foo/{}/alice.md", s2.as_str()),
            "APPROVE\n\nstill lgtm\n",
        );

        let view = scan_feedback(repo, &key, &[s1.clone(), s2.clone()]).unwrap();
        assert_eq!(view.per_commit.len(), 2);
        assert_eq!(view.per_commit[0].sha, s1);
        assert_eq!(view.per_commit[0].entries.len(), 2);
        assert_eq!(view.per_commit[1].sha, s2);
        assert_eq!(view.per_commit[1].entries.len(), 1);
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let key = PlanKey::parse("foo").unwrap();
        let view = scan_feedback(dir.path(), &key, &[sha("1111")]).unwrap();
        assert_eq!(view.per_commit.len(), 1);
        assert!(view.per_commit[0].entries.is_empty());
    }

    #[test]
    fn non_md_files_ignored() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = sha("1111");
        write(
            repo,
            &format!(".clank/feedback/foo/{}/alice.md", s1.as_str()),
            "APPROVE\n",
        );
        write(
            repo,
            &format!(".clank/feedback/foo/{}/notes.txt", s1.as_str()),
            "ignore me",
        );
        let view = scan_feedback(repo, &key, &[s1]).unwrap();
        assert_eq!(view.per_commit[0].entries.len(), 1);
    }
}
