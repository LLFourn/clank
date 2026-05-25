//! Filesystem implementation of `ReviewLookup` for the CLI.
//!
//! Scans `.clank/agents/*/feedback/` for review files matching a
//! given commit SHA. Supports both the old plan-scoped layout
//! (`<plan>/<sha>.md`) and the new flat layout (`<sha>.md`).

use std::path::Path;

use clank_core::ids::{AgentLabel, CommitSha, PlanKey};
use clank_core::vocab::{PlanWorktreeStatus, Verdict};
use clank_core::wait::{ReviewEntry, ReviewLookup};

pub struct FsReviewLookup<'a> {
    pub repo: &'a Path,
    head: Option<&'a CommitSha>,
}

impl<'a> FsReviewLookup<'a> {
    pub fn new(repo: &'a Path, head: Option<&'a CommitSha>) -> Self {
        Self { repo, head }
    }
}

impl ReviewLookup for FsReviewLookup<'_> {
    fn reviews_for(&self, sha: &CommitSha) -> Vec<ReviewEntry> {
        let agents_dir = self.repo.join(".clank/agents");
        let Ok(agents) = std::fs::read_dir(&agents_dir) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        let sha_stem = sha.as_str();
        let sha_short = &sha_stem[..sha_stem.len().min(7)];

        for agent_entry in agents.flatten() {
            let agent_name = agent_entry.file_name();
            let Some(label) = agent_name.to_str().and_then(|s| AgentLabel::parse(s).ok()) else {
                continue;
            };
            let feedback_dir = agent_entry.path().join("feedback");
            if !feedback_dir.is_dir() {
                continue;
            }
            // Scan flat layout: feedback/<sha>.md
            if let Some(verdict) = try_read_verdict(&feedback_dir.join(format!("{sha_stem}.md"))) {
                entries.push(ReviewEntry {
                    author: label.clone(),
                    verdict,
                });
                continue;
            }
            // Scan short-sha flat: feedback/<short>.md
            if let Some(verdict) = try_read_verdict(&feedback_dir.join(format!("{sha_short}.md"))) {
                entries.push(ReviewEntry {
                    author: label.clone(),
                    verdict,
                });
                continue;
            }
            // Scan old plan-scoped layout: feedback/<plan>/<sha>.md
            if let Ok(plan_dirs) = std::fs::read_dir(&feedback_dir) {
                for pd in plan_dirs.flatten() {
                    if !pd.file_type().map_or(false, |t| t.is_dir()) {
                        continue;
                    }
                    let full = pd.path().join(format!("{sha_stem}.md"));
                    if let Some(verdict) = try_read_verdict(&full) {
                        entries.push(ReviewEntry {
                            author: label.clone(),
                            verdict,
                        });
                        break;
                    }
                    let short_path = pd.path().join(format!("{sha_short}.md"));
                    if let Some(verdict) = try_read_verdict(&short_path) {
                        entries.push(ReviewEntry {
                            author: label.clone(),
                            verdict,
                        });
                        break;
                    }
                }
            }
        }
        entries
    }

    fn worktree_status(&self, plan: &PlanKey) -> PlanWorktreeStatus {
        let plan_path = self.repo.join(format!(".clank/plans/{}.md", plan.as_str()));
        if !plan_path.exists() {
            return PlanWorktreeStatus::PlanFileMissing;
        }
        let Some(head) = self.head else {
            return PlanWorktreeStatus::Clean;
        };
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(self.repo)
            .args([
                "diff",
                "--quiet",
                head.as_str(),
                "--",
                &format!(".clank/plans/{}.md", plan.as_str()),
            ])
            .status();
        match output {
            Ok(s) if s.success() => PlanWorktreeStatus::Clean,
            _ => PlanWorktreeStatus::BodyDirty,
        }
    }
}

fn try_read_verdict(path: &Path) -> Option<Verdict> {
    let body = std::fs::read_to_string(path).ok()?;
    Some(clank_core::feedback_body::parse_verdict(&body))
}
