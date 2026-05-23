//! CLI-side IO: walk
//! `.clank/agents/<author>/feedback/<plan>/<commit-ref>.md`
//! into a typed [`FeedbackView`]. Both `status` / `wfw` and the
//! `preview` gate-computation consume the result.
//!
//! Pure interpretation of the parsed bodies (verdict → state
//! machine, participant set, etc.) lives in `clank_core::plan_view`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::disk_format::{FeedbackTarget, parse_feedback_path, parse_verdict};
use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, content_hash};
use clank_core::feedback_view::{CommitFeedback, FeedbackEntry, FeedbackView};

#[derive(Debug, thiserror::Error)]
pub enum FeedbackScanError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Build a `FeedbackView` for `plan` by walking
/// `.clank/agents/*/feedback/<plan>/*.md`.
///
/// `reviewable_shas` is the cumulative list of reviewable commits
/// (chronological); pass `state.fold.plans[plan].commits` filtered
/// to `touched_plan || touched_code`. Result `per_commit` is in
/// the same order.
///
/// Files whose stem doesn't resolve against `reviewable_shas` —
/// orphans from rewritten history, or just a typo — are silently
/// dropped. The reader doesn't error on missing directories
/// either: a brand-new repo with no `.clank/agents/` yields an
/// empty `FeedbackView` for every sha.
pub fn scan_feedback(
    repo: &Path,
    plan: &PlanKey,
    reviewable_shas: &[CommitSha],
) -> Result<FeedbackView, FeedbackScanError> {
    // Result accumulator: one bucket per reviewable sha.
    let mut buckets: BTreeMap<CommitSha, BTreeMap<AgentLabel, FeedbackEntry>> = reviewable_shas
        .iter()
        .cloned()
        .map(|s| (s, BTreeMap::new()))
        .collect();

    let agents_root = repo.join(".clank/agents");
    let agent_dirs = match std::fs::read_dir(&agents_root) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(emit_in_order(reviewable_shas, buckets));
        }
        Err(e) => return Err(e.into()),
    };

    for agent_entry in agent_dirs.flatten() {
        let plan_dir = agent_entry.path().join("feedback").join(plan.as_str());
        let files = match std::fs::read_dir(&plan_dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for file_entry in files.flatten() {
            let abs = file_entry.path();
            let rel = match abs.strip_prefix(repo.join(".clank")) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            let Some(parsed) = parse_feedback_path(&rel) else {
                continue;
            };
            // Defensive — caller asked for `plan` so AdHoc and
            // other-plan files shouldn't be here, but typed-skip
            // anyway.
            if parsed.target != FeedbackTarget::Plan(plan.clone()) {
                continue;
            }
            // Resolve the on-disk ref into a full sha via the
            // reviewable-commit scope. Orphans drop here.
            let Some(full_sha) = parsed.target_ref.resolve_against(reviewable_shas) else {
                continue;
            };
            let body = match std::fs::read_to_string(&abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let source_path = abs
                .strip_prefix(repo)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| abs.to_string_lossy().to_string());
            let entry = FeedbackEntry {
                verdict: parse_verdict(&body),
                body_hash: content_hash(&body),
                source_path,
            };
            // `buckets[full_sha]` always exists because
            // `resolve_against` only returns shas from
            // `reviewable_shas`.
            buckets
                .entry(full_sha)
                .or_default()
                .insert(parsed.author, entry);
        }
    }

    Ok(emit_in_order(reviewable_shas, buckets))
}

fn emit_in_order(
    shas: &[CommitSha],
    mut buckets: BTreeMap<CommitSha, BTreeMap<AgentLabel, FeedbackEntry>>,
) -> FeedbackView {
    let per_commit = shas
        .iter()
        .map(|sha| CommitFeedback {
            sha: sha.clone(),
            entries: buckets.remove(sha).unwrap_or_default(),
        })
        .collect();
    FeedbackView { per_commit }
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

    fn full_sha(suffix: &str) -> CommitSha {
        CommitSha::parse(&format!("{suffix:0<40}")).unwrap()
    }

    #[test]
    fn scans_two_commits_with_interleaved_authors() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("1111");
        let s2 = full_sha("2222");
        // Short stem for both — first 7 chars unique.
        write(
            repo,
            &format!(".clank/agents/alice/feedback/foo/{}.md", &s1.as_str()[..7]),
            "APPROVE\n\nlgtm\n",
        );
        write(
            repo,
            &format!(".clank/agents/bob/feedback/foo/{}.md", &s1.as_str()[..7]),
            "REQUEST_CHANGES\n\nnope\n",
        );
        write(
            repo,
            &format!(".clank/agents/alice/feedback/foo/{}.md", &s2.as_str()[..7]),
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
    fn reader_finds_short_path() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("abcdef0");
        write(
            repo,
            ".clank/agents/alice/feedback/foo/abcdef0.md",
            "APPROVE\n",
        );
        let view = scan_feedback(repo, &key, &[s1.clone()]).unwrap();
        assert_eq!(view.per_commit[0].entries.len(), 1);
    }

    #[test]
    fn reader_finds_full_sha_path() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("abcdef0");
        write(
            repo,
            &format!(".clank/agents/alice/feedback/foo/{}.md", s1.as_str()),
            "APPROVE\n",
        );
        let view = scan_feedback(repo, &key, &[s1.clone()]).unwrap();
        assert_eq!(view.per_commit[0].entries.len(), 1);
    }

    #[test]
    fn orphan_ref_is_dropped() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("1111111");
        // Write feedback for a sha that's NOT in the reviewable scope.
        write(
            repo,
            ".clank/agents/alice/feedback/foo/deadbee.md",
            "APPROVE\n",
        );
        let view = scan_feedback(repo, &key, &[s1.clone()]).unwrap();
        assert_eq!(view.per_commit[0].entries.len(), 0);
    }

    #[test]
    fn missing_agents_root_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let key = PlanKey::parse("foo").unwrap();
        let view = scan_feedback(dir.path(), &key, &[full_sha("1111")]).unwrap();
        assert_eq!(view.per_commit.len(), 1);
        assert!(view.per_commit[0].entries.is_empty());
    }

    #[test]
    fn non_md_files_ignored() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("1111");
        write(
            repo,
            &format!(".clank/agents/alice/feedback/foo/{}.md", &s1.as_str()[..7]),
            "APPROVE\n",
        );
        write(
            repo,
            ".clank/agents/alice/feedback/foo/notes.txt",
            "ignore me",
        );
        let view = scan_feedback(repo, &key, &[s1]).unwrap();
        assert_eq!(view.per_commit[0].entries.len(), 1);
    }

    #[test]
    fn other_plan_files_ignored() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        let key = PlanKey::parse("foo").unwrap();
        let s1 = full_sha("1111");
        write(
            repo,
            &format!(".clank/agents/alice/feedback/foo/{}.md", &s1.as_str()[..7]),
            "APPROVE\n",
        );
        write(
            repo,
            &format!(".clank/agents/alice/feedback/bar/{}.md", &s1.as_str()[..7]),
            "APPROVE\n",
        );
        let view = scan_feedback(repo, &key, &[s1]).unwrap();
        // `bar` plan file not visible — `scan_feedback(plan=foo)`
        // descends only into `agents/*/feedback/foo/`.
        assert_eq!(view.per_commit[0].entries.len(), 1);
    }
}
