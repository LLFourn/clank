//! Filesystem implementation of `PlanStateLookup` for the CLI.
//!
//! Scans `.clank/agents/*/feedback/<sha>.md` for review files.

use std::cell::OnceCell;
use std::path::Path;

use clank_core::ids::{CommitSha, PlanKey};
use clank_core::plan_view::PlanBlock;
use clank_core::vocab::{PlanWorktreeStatus, Verdict};
use clank_core::wait::{PlanStateLookup, ReviewEntry};

pub struct FsPlanStateLookup<'a> {
    pub repo: &'a Path,
    head: Option<&'a CommitSha>,
    /// Opened on first `worktree_status` and reused across every
    /// plan in a single derive (was a `git diff` subprocess per
    /// plan). Lazy so review-only uses don't pay for it.
    git: OnceCell<Option<gix::Repository>>,
}

impl<'a> FsPlanStateLookup<'a> {
    pub fn new(repo: &'a Path, head: Option<&'a CommitSha>) -> Self {
        Self {
            repo,
            head,
            git: OnceCell::new(),
        }
    }

    fn git(&self) -> Option<&gix::Repository> {
        self.git.get_or_init(|| gix::open(self.repo).ok()).as_ref()
    }
}

impl PlanStateLookup for FsPlanStateLookup<'_> {
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
            let Some(label) = agent_name
                .to_str()
                .and_then(|s| clank_core::ids::AgentLabel::parse(s).ok())
            else {
                continue;
            };
            let feedback_dir = agent_entry.path().join("feedback");
            if !feedback_dir.is_dir() {
                continue;
            }
            if let Some(verdict) = try_read_verdict(&feedback_dir.join(format!("{sha_stem}.md"))) {
                entries.push(ReviewEntry {
                    author: label.clone(),
                    verdict,
                });
                continue;
            }
            if let Some(verdict) = try_read_verdict(&feedback_dir.join(format!("{sha_short}.md"))) {
                entries.push(ReviewEntry {
                    author: label,
                    verdict,
                });
            }
        }
        entries
    }

    fn blocks_for(&self, plan: &PlanKey) -> Vec<PlanBlock> {
        // Reuse the existing scan_blocks pass and project pending
        // (unanswered) plan-scoped blocks for this plan key.
        let plan_str = plan.as_str();
        crate::cli::block::scan_blocks(self.repo)
            .into_iter()
            .filter(|b| b.answer.is_none() && b.plan.as_deref() == Some(plan_str))
            .filter_map(|b| {
                clank_core::ids::AgentLabel::parse(&b.agent)
                    .ok()
                    .map(|creator| PlanBlock {
                        creator,
                        name: b.name,
                        message: b.question,
                    })
            })
            .collect()
    }

    fn pr_reviews(&self) -> Vec<clank_core::wait::PrReviewInput> {
        crate::cli::pr_review::pr_review_inputs(self.repo)
    }

    fn worktree_status(&self, plan: &PlanKey) -> PlanWorktreeStatus {
        let rel = format!(".clank/plans/{}.md", plan.as_str());
        if !self.repo.join(&rel).exists() {
            return PlanWorktreeStatus::PlanFileMissing;
        }
        let Some(head) = self.head else {
            return PlanWorktreeStatus::Clean;
        };
        let Some(git) = self.git() else {
            return PlanWorktreeStatus::Clean;
        };
        crate::worktree_facts::body_status_vs_commit(git, head, &rel)
    }
}

fn try_read_verdict(path: &Path) -> Option<Verdict> {
    let body = std::fs::read_to_string(path).ok()?;
    Some(clank_core::feedback_body::parse_verdict(&body))
}
