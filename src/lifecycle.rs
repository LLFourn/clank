//! Newtype wrappers for the IDs used across the filesystem-truth model.

use std::fmt;
use std::path::{Component, Path, PathBuf};

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

string_newtype!(CommitSha);
string_newtype!(ContentHash);
string_newtype!(AgentLabel);
string_newtype!(PlanKey);
string_newtype!(RepoBasename);

impl RepoBasename {
    /// Extract the basename (final `file_name` component) of a canonical
    /// repo root. Returns `None` if the path has no usable basename.
    pub fn from_repo_root(repo: &Path) -> Option<Self> {
        let name = repo.file_name()?.to_str()?;
        if name.is_empty() {
            return None;
        }
        Some(RepoBasename(name.to_string()))
    }
}

/// Public-facing plan identity: `<repo_basename>/<stem>.md`. Stable
/// across the active↔done move (the file moves, the `PlanId` doesn't).
///
/// Parsing is filesystem-free — it only validates the wire shape. The
/// daemon resolves the basename against `Trinity.repo_basenames` to
/// find the canonical `RepoRoot`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanId {
    repo: RepoBasename,
    key: PlanKey,
}

impl PlanId {
    pub fn new(repo: RepoBasename, key: PlanKey) -> Self {
        Self { repo, key }
    }

    pub fn repo(&self) -> &RepoBasename {
        &self.repo
    }

    pub fn key(&self) -> &PlanKey {
        &self.key
    }

    /// Parse the wire form `<repo_basename>/<stem>.md`. Validates shape
    /// only; does not touch the filesystem.
    pub fn parse(s: &str) -> Result<Self, ParsePlanIdError> {
        let (repo, stem_md) = s.split_once('/').ok_or(ParsePlanIdError::Malformed)?;
        if repo.is_empty() {
            return Err(ParsePlanIdError::EmptyRepo);
        }
        let stem = stem_md
            .strip_suffix(".md")
            .ok_or(ParsePlanIdError::MissingMdSuffix)?;
        if stem.is_empty() {
            return Err(ParsePlanIdError::EmptyStem);
        }
        if stem.contains('/') {
            return Err(ParsePlanIdError::SlashInStem);
        }
        Ok(PlanId {
            repo: RepoBasename(repo.to_string()),
            key: PlanKey(stem.to_string()),
        })
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}.md", self.repo.as_str(), self.key.as_str())
    }
}

impl serde::Serialize for PlanId {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for PlanId {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        PlanId::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParsePlanIdError {
    #[error("plan_id must be `<repo_basename>/<stem>.md`")]
    Malformed,
    #[error("plan_id is missing repo basename before `/`")]
    EmptyRepo,
    #[error("plan_id is missing the `.md` suffix")]
    MissingMdSuffix,
    #[error("plan_id stem is empty")]
    EmptyStem,
    #[error("plan_id stem must not contain `/`")]
    SlashInStem,
}

impl PlanKey {
    /// Parse a repo-relative plan-file path into a `PlanKey`.
    ///
    /// Accepts exactly:
    /// - `.trinity/plans/<stem>.md`
    /// - `.trinity/plans/done/<stem>.md`
    ///
    /// `<stem>` must be non-empty and must not contain path separators
    /// (no nesting). Dots inside the stem are allowed — `foo.v2.md` parses
    /// to `PlanKey("foo.v2")`. Only the trailing `.md` is treated as an
    /// extension.
    pub fn from_path(p: &Path) -> Option<Self> {
        let segments: Vec<&std::ffi::OsStr> = p
            .components()
            .map(|c| match c {
                Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;

        let stem_seg = match segments
            .iter()
            .map(|s| s.to_str())
            .collect::<Option<Vec<_>>>()?
            .as_slice()
        {
            [".trinity", "plans", name] => *name,
            [".trinity", "plans", "done", name] => *name,
            _ => return None,
        };

        let stem = stem_seg.strip_suffix(".md")?;
        if stem.is_empty() || stem.contains('/') {
            return None;
        }
        Some(PlanKey(stem.to_string()))
    }
}

/// True when `p` lies under `.trinity/plans/done/`. Pure check on the
/// path string; doesn't touch disk.
pub fn is_done_plan_path(p: &Path) -> bool {
    p.components()
        .any(|c| matches!(c, Component::Normal(s) if s == "done"))
}

/// Active↔done counterpart of a canonical plan path:
/// `.trinity/plans/foo.md` ↔ `.trinity/plans/done/foo.md`. Returns `None`
/// if `p` doesn't parse as a canonical plan path.
pub fn plan_path_counterpart(p: &Path) -> Option<PathBuf> {
    let key = PlanKey::from_path(p)?;
    let stem = key.as_str();
    Some(if is_done_plan_path(p) {
        PathBuf::from(format!(".trinity/plans/{stem}.md"))
    } else {
        PathBuf::from(format!(".trinity/plans/done/{stem}.md"))
    })
}

/// Stable content hash for a plan-file body. Identifies "is this the same
/// plan or a new one?" in the rebuild + worktree-status logic.
pub fn content_hash(body: &str) -> ContentHash {
    ContentHash(blake3::hash(body.as_bytes()).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn plan_key_active_path() {
        let k = PlanKey::from_path(&p(".trinity/plans/foo.md")).unwrap();
        assert_eq!(k.as_str(), "foo");
    }

    #[test]
    fn plan_key_done_path() {
        let k = PlanKey::from_path(&p(".trinity/plans/done/foo.md")).unwrap();
        assert_eq!(k.as_str(), "foo");
    }

    #[test]
    fn plan_key_rejects_nested() {
        assert!(PlanKey::from_path(&p(".trinity/plans/team/foo.md")).is_none());
        assert!(PlanKey::from_path(&p(".trinity/plans/done/team/foo.md")).is_none());
    }

    #[test]
    fn plan_key_rejects_non_md() {
        assert!(PlanKey::from_path(&p(".trinity/plans/foo.txt")).is_none());
        assert!(PlanKey::from_path(&p(".trinity/plans/foo")).is_none());
    }

    #[test]
    fn plan_key_rejects_empty_stem() {
        assert!(PlanKey::from_path(&p(".trinity/plans/.md")).is_none());
    }

    #[test]
    fn plan_key_accepts_dots_inside_stem() {
        // `.md` is the trailing extension; dots inside the stem are part of
        // the slug. Useful for versioning conventions like `foo.v2.md`.
        let k = PlanKey::from_path(&p(".trinity/plans/foo.v2.md")).unwrap();
        assert_eq!(k.as_str(), "foo.v2");
    }

    #[test]
    fn plan_key_rejects_off_tree_paths() {
        assert!(PlanKey::from_path(&p("plans/foo.md")).is_none());
        assert!(PlanKey::from_path(&p(".trinity/notes/foo.md")).is_none());
    }

    #[test]
    fn is_done_plan_path_helper() {
        assert!(!is_done_plan_path(&p(".trinity/plans/foo.md")));
        assert!(is_done_plan_path(&p(".trinity/plans/done/foo.md")));
    }

    #[test]
    fn plan_path_counterpart_flips_active_done() {
        let active = p(".trinity/plans/foo.md");
        let done = p(".trinity/plans/done/foo.md");
        assert_eq!(plan_path_counterpart(&active), Some(done.clone()));
        assert_eq!(plan_path_counterpart(&done), Some(active));
    }

    #[test]
    fn plan_path_counterpart_rejects_off_tree() {
        assert!(plan_path_counterpart(&p("plans/foo.md")).is_none());
    }

    // ---- PlanId::parse ----

    #[test]
    fn plan_id_parses_canonical_form() {
        let pid = PlanId::parse("trinity/foo.md").unwrap();
        assert_eq!(pid.repo().as_str(), "trinity");
        assert_eq!(pid.key().as_str(), "foo");
        assert_eq!(pid.to_string(), "trinity/foo.md");
    }

    #[test]
    fn plan_id_accepts_dots_in_stem() {
        let pid = PlanId::parse("trinity/foo.v2.md").unwrap();
        assert_eq!(pid.key().as_str(), "foo.v2");
    }

    #[test]
    fn plan_id_malformed_without_slash() {
        assert_eq!(
            PlanId::parse("foo.md").unwrap_err(),
            ParsePlanIdError::Malformed
        );
    }

    #[test]
    fn plan_id_empty_repo_rejected() {
        assert_eq!(
            PlanId::parse("/foo.md").unwrap_err(),
            ParsePlanIdError::EmptyRepo
        );
    }

    #[test]
    fn plan_id_missing_md_suffix_rejected() {
        assert_eq!(
            PlanId::parse("trinity/foo").unwrap_err(),
            ParsePlanIdError::MissingMdSuffix
        );
    }

    #[test]
    fn plan_id_empty_stem_rejected() {
        assert_eq!(
            PlanId::parse("trinity/.md").unwrap_err(),
            ParsePlanIdError::EmptyStem
        );
    }

    #[test]
    fn plan_id_slash_in_stem_rejected() {
        assert_eq!(
            PlanId::parse("trinity/sub/foo.md").unwrap_err(),
            ParsePlanIdError::SlashInStem
        );
    }

    #[test]
    fn plan_id_roundtrips_through_display() {
        let original = "trinity/plan-path-identity.md";
        let pid = PlanId::parse(original).unwrap();
        assert_eq!(pid.to_string(), original);
    }
}
