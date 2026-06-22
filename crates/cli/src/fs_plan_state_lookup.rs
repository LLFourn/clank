//! Filesystem implementation of `PlanStateLookup` for the CLI.
//!
//! Scans `.clank/agents/*/feedback/<sha>.md` for review files.

use std::cell::OnceCell;
use std::path::Path;

use crate::git_io;
use clank_core::ids::{CommitSha, PlanKey};
use clank_core::plan_view::PlanBlock;
use clank_core::vocab::{PlanWorktreeStatus, Verdict};
use clank_core::wait::{PlanStateLookup, ReviewEntry};

pub struct FsPlanStateLookup<'a> {
    pub repo: &'a Path,
    head: Option<&'a CommitSha>,
    /// A handle shared with the rest of the rebuild (status's
    /// `from_state` threads one through both the dirty walk and this
    /// lookup, so a snapshot opens the ODB once). `None` for callers
    /// that don't hold one — then `git_owned` opens lazily.
    git: Option<&'a git_io::Repo>,
    /// Lazily opened on first `worktree_status` when no shared handle
    /// was given, reused across every plan in a derive (was a `git
    /// diff` subprocess per plan). Lazy so review-only uses don't pay.
    git_owned: OnceCell<Option<git_io::Repo>>,
}

impl<'a> FsPlanStateLookup<'a> {
    pub fn new(repo: &'a Path, head: Option<&'a CommitSha>) -> Self {
        Self {
            repo,
            head,
            git: None,
            git_owned: OnceCell::new(),
        }
    }

    /// Like [`new`](Self::new) but reuses a handle the caller already
    /// opened, so the whole rebuild shares one ODB.
    pub fn with_handle(repo: &'a Path, head: Option<&'a CommitSha>, git: &'a git_io::Repo) -> Self {
        Self {
            repo,
            head,
            git: Some(git),
            git_owned: OnceCell::new(),
        }
    }

    fn git(&self) -> Option<&git_io::Repo> {
        match self.git {
            Some(git) => Some(git),
            None => self
                .git_owned
                .get_or_init(|| git_io::open(self.repo).ok())
                .as_ref(),
        }
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
        git_io::plan_body_status(git, head, &rel)
    }
}

fn try_read_verdict(path: &Path) -> Option<Verdict> {
    let body = std::fs::read_to_string(path).ok()?;
    Some(clank_core::feedback_body::parse_verdict(&body))
}
