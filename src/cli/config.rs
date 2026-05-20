//! Two-layer JSON config loader for Trinity's review knobs.
//!
//! Phase 4 of `commit-first-review-model` introduces a config surface
//! so operators can decide whether master is blocked on plan reviews,
//! whether ad hoc commits surface as reviewer work, and (in strict
//! mode) whether commits without a valid `[plan]` / `[misc]` title
//! prefix raise `FixCommitTitle` instead of being classified.
//!
//! Layering (deep merge, repo overrides user):
//!
//! 1. `~/.trinity/config.json` — user-level defaults.
//! 2. `<repo>/.trinity/config.json` — repo-level overrides
//!    (committed; visible to all collaborators).
//!
//! Missing files at either layer fall through to built-in defaults.
//! Malformed JSON at either layer logs a warning and treats that
//! layer as missing — config loading must never fail the operation.
//!
//! The loader runs at projection time (matcher / `attach_live_feedback`),
//! never inside the cache-keyed fold. Config changes therefore do not
//! invalidate the on-disk `BaseRepoState` cache.

use serde::Deserialize;
use std::path::Path;

use crate::lifecycle::AgentLabel;

/// Top-level Trinity config. Always non-`Option` here even though
/// the on-disk schema permits omissions; the loader fills in
/// defaults so consumers don't need to thread Option chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub review: ReviewConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            review: ReviewConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewConfig {
    /// Block master on ad hoc (no-plan) commit review. Default `true`
    /// (review every commit by default).
    pub force_review_on_misc_commits: bool,
    /// Block master on plan-attributed commit review. Default `true`
    /// — today's implicit behavior.
    pub force_review_on_plan_commits: bool,
    /// Explicit reviewer list for ad hoc commits. `None` → derive
    /// from the repo's feedback authors at projection time.
    pub ad_hoc_reviewers: Option<Vec<AgentLabel>>,
    /// Strict mode: when `true`, commits without a valid
    /// `[plan]` / `[plan-one,plan-two]` / `[misc]` title prefix
    /// surface as `FixCommitTitle` master work instead of being
    /// classified. Phase 5 of `commit-first-review-model`.
    pub require_commit_prefix: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            force_review_on_misc_commits: true,
            force_review_on_plan_commits: true,
            ad_hoc_reviewers: None,
            require_commit_prefix: false,
        }
    }
}

/// On-disk schema (all fields optional so partial files are valid).
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    review: Option<ReviewFile>,
}

#[derive(Debug, Default, Deserialize)]
struct ReviewFile {
    #[serde(default)]
    force_review_on_misc_commits: Option<bool>,
    #[serde(default)]
    force_review_on_plan_commits: Option<bool>,
    #[serde(default)]
    ad_hoc_reviewers: Option<Vec<String>>,
    #[serde(default)]
    require_commit_prefix: Option<bool>,
}

/// Load and merge the user-level and repo-level config. Missing
/// files at either layer fall through to defaults; malformed
/// JSON is treated as absent (with a log warning) — config
/// loading must never fail the operation.
pub fn load(repo_root: &Path) -> Config {
    let mut cfg = Config::default();
    apply_layer(&mut cfg, user_path().as_deref());
    apply_layer(&mut cfg, Some(&repo_root.join(".trinity/config.json")));
    cfg
}

fn user_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".trinity/config.json"))
}

fn apply_layer(cfg: &mut Config, path: Option<&Path>) {
    let Some(path) = path else { return };
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "trinity config: read failed; ignoring layer");
            return;
        }
    };
    let parsed: ConfigFile = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "trinity config: malformed JSON; ignoring layer");
            return;
        }
    };
    if let Some(review) = parsed.review {
        if let Some(v) = review.force_review_on_misc_commits {
            cfg.review.force_review_on_misc_commits = v;
        }
        if let Some(v) = review.force_review_on_plan_commits {
            cfg.review.force_review_on_plan_commits = v;
        }
        if let Some(list) = review.ad_hoc_reviewers {
            let parsed: Vec<AgentLabel> = list
                .into_iter()
                .filter_map(|s| AgentLabel::parse(&s).ok())
                .collect();
            cfg.review.ad_hoc_reviewers = Some(parsed);
        }
        if let Some(v) = review.require_commit_prefix {
            cfg.review.require_commit_prefix = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn defaults_when_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load(tmp.path());
        assert!(cfg.review.force_review_on_misc_commits);
        assert!(cfg.review.force_review_on_plan_commits);
        assert!(cfg.review.ad_hoc_reviewers.is_none());
        assert!(!cfg.review.require_commit_prefix);
    }

    #[test]
    fn repo_layer_overrides_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".trinity/config.json");
        write(
            &repo_cfg,
            r#"{"review": {"force_review_on_misc_commits": false}}"#,
        );
        let cfg = load(tmp.path());
        assert!(!cfg.review.force_review_on_misc_commits);
        // Other fields untouched.
        assert!(cfg.review.force_review_on_plan_commits);
    }

    #[test]
    fn malformed_json_ignored_layer_falls_through_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".trinity/config.json");
        write(&repo_cfg, "not valid json");
        let cfg = load(tmp.path());
        assert!(cfg.review.force_review_on_misc_commits);
    }

    #[test]
    fn ad_hoc_reviewers_list_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_cfg = tmp.path().join(".trinity/config.json");
        write(
            &repo_cfg,
            r#"{"review": {"ad_hoc_reviewers": ["alice", "bob"]}}"#,
        );
        let cfg = load(tmp.path());
        let list = cfg.review.ad_hoc_reviewers.expect("list set");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].as_str(), "alice");
        assert_eq!(list[1].as_str(), "bob");
    }
}
