//! Newtype wrappers for the IDs used across the filesystem-truth model.

use std::fmt;
use std::path::{Component, Path, PathBuf};

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
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

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let s = String::deserialize(de)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_newtype!(CommitSha);
string_newtype!(ContentHash);
string_newtype!(AgentLabel);
string_newtype!(PlanKey);
string_newtype!(RepoBasename);

/// Validation error for the typed ID parsers.
///
/// All identifier newtypes (`CommitSha`, `ContentHash`, `AgentLabel`,
/// `PlanKey`, `RepoBasename`) reject malformed input through this
/// shared error type. `kind` is the type name so a single
/// `match`/`Display` is enough for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} cannot be empty")]
    Empty { kind: &'static str },
    #[error("{kind} must not contain `{ch}`: {value:?}")]
    ForbiddenChar {
        kind: &'static str,
        ch: char,
        value: String,
    },
    #[error("{kind} cannot be `.` or `..`")]
    DotSegment { kind: &'static str },
    #[error("{kind} must not start with `.`: {value:?}")]
    LeadingDot { kind: &'static str, value: String },
    #[error("{kind} must be lowercase hex; got {value:?}")]
    NotHex { kind: &'static str, value: String },
    #[error("{kind} length must be {min}-{max} chars; got {len}")]
    BadLength {
        kind: &'static str,
        min: usize,
        max: usize,
        len: usize,
    },
}

fn check_no_slash(kind: &'static str, s: &str) -> Result<(), IdError> {
    if let Some(ch) = s.chars().find(|c| *c == '/' || *c == '\\') {
        return Err(IdError::ForbiddenChar {
            kind,
            ch,
            value: s.to_string(),
        });
    }
    Ok(())
}

fn check_non_empty(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty { kind });
    }
    Ok(())
}

fn check_not_dot_segment(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s == "." || s == ".." {
        return Err(IdError::DotSegment { kind });
    }
    Ok(())
}

fn check_no_leading_dot(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.starts_with('.') {
        return Err(IdError::LeadingDot {
            kind,
            value: s.to_string(),
        });
    }
    Ok(())
}

impl CommitSha {
    /// Parse a git short-or-full SHA: 4–40 lowercase hex chars.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "CommitSha";
        const MIN: usize = 4;
        const MAX: usize = 40;
        if s.len() < MIN || s.len() > MAX {
            return Err(IdError::BadLength {
                kind: KIND,
                min: MIN,
                max: MAX,
                len: s.len(),
            });
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(IdError::NotHex {
                kind: KIND,
                value: s.to_string(),
            });
        }
        Ok(CommitSha(s.to_string()))
    }
}

impl ContentHash {
    /// Parse a blake3 content hash: exactly 64 lowercase hex chars.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "ContentHash";
        const LEN: usize = 64;
        if s.len() != LEN {
            return Err(IdError::BadLength {
                kind: KIND,
                min: LEN,
                max: LEN,
                len: s.len(),
            });
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(IdError::NotHex {
                kind: KIND,
                value: s.to_string(),
            });
        }
        Ok(ContentHash(s.to_string()))
    }
}

impl AgentLabel {
    /// Parse an agent label: non-empty, no `/`, not `.`/`..`, no
    /// leading `.` (avoids dotfile collisions in
    /// `.trinity/feedback/.../<author>.md`).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "AgentLabel";
        check_non_empty(KIND, s)?;
        check_not_dot_segment(KIND, s)?;
        check_no_leading_dot(KIND, s)?;
        check_no_slash(KIND, s)?;
        Ok(AgentLabel(s.to_string()))
    }
}

impl PlanKey {
    /// Parse a plan stem (the `<stem>` in
    /// `.trinity/plans/<stem>.md`). Non-empty, no `/`, not `.`/`..`,
    /// no leading `.`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "PlanKey";
        check_non_empty(KIND, s)?;
        check_not_dot_segment(KIND, s)?;
        check_no_leading_dot(KIND, s)?;
        check_no_slash(KIND, s)?;
        Ok(PlanKey(s.to_string()))
    }
}

impl RepoBasename {
    /// Parse a repo basename: non-empty, no `/`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "RepoBasename";
        check_non_empty(KIND, s)?;
        check_no_slash(KIND, s)?;
        Ok(RepoBasename(s.to_string()))
    }
}

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
    /// via `RepoBasename::parse` and `PlanKey::parse`; does not touch
    /// the filesystem.
    pub fn parse(s: &str) -> Result<Self, ParsePlanIdError> {
        let (repo, stem_md) = s.split_once('/').ok_or(ParsePlanIdError::Malformed)?;
        let stem = stem_md
            .strip_suffix(".md")
            .ok_or(ParsePlanIdError::MissingMdSuffix)?;
        let repo = RepoBasename::parse(repo).map_err(ParsePlanIdError::InvalidRepo)?;
        let key = PlanKey::parse(stem).map_err(ParsePlanIdError::InvalidStem)?;
        Ok(PlanId { repo, key })
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
    #[error("plan_id is missing the `.md` suffix")]
    MissingMdSuffix,
    #[error("plan_id repo basename invalid: {0}")]
    InvalidRepo(IdError),
    #[error("plan_id stem invalid: {0}")]
    InvalidStem(IdError),
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
        // Route through PlanKey::parse so dot-segment / leading-dot /
        // forbidden-char rules are enforced for disk discovery the
        // same way they are for wire parsing.
        PlanKey::parse(stem).ok()
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
        assert!(matches!(
            PlanId::parse("/foo.md").unwrap_err(),
            ParsePlanIdError::InvalidRepo(IdError::Empty { .. })
        ));
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
        assert!(matches!(
            PlanId::parse("trinity/.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::Empty { .. })
        ));
    }

    #[test]
    fn plan_id_slash_in_stem_rejected() {
        assert!(matches!(
            PlanId::parse("trinity/sub/foo.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::ForbiddenChar { .. })
        ));
    }

    #[test]
    fn plan_id_dot_stem_rejected() {
        // Regression for the validator-bypass codex caught: `.hidden`
        // stems must be rejected at PlanId::parse, not silently
        // accepted into PlanKey.
        assert!(matches!(
            PlanId::parse("trinity/.hidden.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::LeadingDot { .. })
        ));
    }

    #[test]
    fn plan_key_from_path_rejects_leading_dot() {
        // Regression: disk discovery routes through PlanKey::parse.
        // `.trinity/plans/.hidden.md` must NOT become a PlanKey.
        assert!(PlanKey::from_path(&p(".trinity/plans/.hidden.md")).is_none());
    }

    #[test]
    fn plan_id_roundtrips_through_display() {
        let original = "trinity/plan-path-identity.md";
        let pid = PlanId::parse(original).unwrap();
        assert_eq!(pid.to_string(), original);
    }

    // ---- newtype validated parsers ----

    #[test]
    fn commit_sha_accepts_valid_hex() {
        assert!(CommitSha::parse("abc1").is_ok());
        assert!(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").is_ok());
    }

    #[test]
    fn commit_sha_rejects_uppercase() {
        assert!(matches!(
            CommitSha::parse("ABCD"),
            Err(IdError::NotHex { .. })
        ));
    }

    #[test]
    fn commit_sha_rejects_non_hex() {
        assert!(matches!(
            CommitSha::parse("xyzw"),
            Err(IdError::NotHex { .. })
        ));
    }

    #[test]
    fn commit_sha_rejects_too_short() {
        assert!(matches!(
            CommitSha::parse("abc"),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn commit_sha_rejects_too_long() {
        let s = "a".repeat(41);
        assert!(matches!(
            CommitSha::parse(&s),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn content_hash_accepts_64_hex() {
        let s = "a".repeat(64);
        assert!(ContentHash::parse(&s).is_ok());
    }

    #[test]
    fn content_hash_rejects_non_64() {
        assert!(matches!(
            ContentHash::parse(&"a".repeat(63)),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn agent_label_accepts_simple() {
        assert!(AgentLabel::parse("codex").is_ok());
        assert!(AgentLabel::parse("claude-main").is_ok());
    }

    #[test]
    fn agent_label_rejects_empty() {
        assert!(matches!(AgentLabel::parse(""), Err(IdError::Empty { .. })));
    }

    #[test]
    fn agent_label_rejects_slash() {
        assert!(matches!(
            AgentLabel::parse("a/b"),
            Err(IdError::ForbiddenChar { .. })
        ));
    }

    #[test]
    fn agent_label_rejects_leading_dot() {
        assert!(matches!(
            AgentLabel::parse(".hidden"),
            Err(IdError::LeadingDot { .. })
        ));
    }

    #[test]
    fn agent_label_rejects_dot_segments() {
        assert!(matches!(
            AgentLabel::parse("."),
            Err(IdError::DotSegment { .. })
        ));
        assert!(matches!(
            AgentLabel::parse(".."),
            Err(IdError::DotSegment { .. })
        ));
    }

    #[test]
    fn plan_key_accepts_simple_and_dotted_stem() {
        assert!(PlanKey::parse("foo").is_ok());
        assert!(PlanKey::parse("foo.v2").is_ok());
        assert!(PlanKey::parse("architecture-tech-debt-sweep").is_ok());
    }

    #[test]
    fn plan_key_rejects_path_traversal() {
        assert!(matches!(
            PlanKey::parse(".."),
            Err(IdError::DotSegment { .. })
        ));
        assert!(PlanKey::parse("../etc/passwd").is_err());
    }

    #[test]
    fn plan_key_rejects_slash() {
        assert!(matches!(
            PlanKey::parse("team/foo"),
            Err(IdError::ForbiddenChar { .. })
        ));
    }

    #[test]
    fn plan_key_rejects_empty() {
        assert!(matches!(PlanKey::parse(""), Err(IdError::Empty { .. })));
    }

    #[test]
    fn plan_key_rejects_leading_dot() {
        assert!(matches!(
            PlanKey::parse(".hidden"),
            Err(IdError::LeadingDot { .. })
        ));
    }

    #[test]
    fn repo_basename_accepts_simple() {
        assert!(RepoBasename::parse("trinity").is_ok());
    }

    #[test]
    fn repo_basename_rejects_empty() {
        assert!(matches!(
            RepoBasename::parse(""),
            Err(IdError::Empty { .. })
        ));
    }

    #[test]
    fn repo_basename_rejects_slash() {
        assert!(matches!(
            RepoBasename::parse("a/b"),
            Err(IdError::ForbiddenChar { .. })
        ));
    }
}
