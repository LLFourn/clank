//! Validated newtypes for the IDs used across the filesystem-truth
//! model. Each type wraps a `String`, serde-transparent so the wire
//! form is just the inner string, with parse-validation enforced on
//! deserialize as well as on explicit `parse(&str)`.
//!
//! Daemon and frontend share these — a daemon-side `AgentLabel`
//! constructed via `parse` is the same value the frontend
//! reconstructs on the wire, and an invalid wire value fails to
//! deserialize at the fetch boundary.

use std::fmt;
use std::path::{Component, Path};

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[cfg_attr(
            feature = "cache-encoding",
            derive(wincode::SchemaWrite, wincode::SchemaRead)
        )]
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
string_newtype!(CommitRef);
string_newtype!(ContentHash);
string_newtype!(AgentLabel);
string_newtype!(PlanKey);
string_newtype!(RepoBasename);
string_newtype!(SessionId);

/// Validation error for the typed ID parsers.
///
/// All identifier newtypes (`CommitSha`, `ContentHash`, `AgentLabel`,
/// `PlanKey`, `RepoBasename`) reject malformed input through this
/// shared error type. `kind` is the type name so a single
/// `match`/`Display` is enough for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    Empty {
        kind: &'static str,
    },
    ForbiddenChar {
        kind: &'static str,
        ch: char,
        value: String,
    },
    DotSegment {
        kind: &'static str,
    },
    LeadingDot {
        kind: &'static str,
        value: String,
    },
    NotHex {
        kind: &'static str,
        value: String,
    },
    BadLength {
        kind: &'static str,
        min: usize,
        max: usize,
        len: usize,
    },
    Reserved {
        kind: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Empty { kind } => write!(f, "{kind} cannot be empty"),
            IdError::ForbiddenChar { kind, ch, value } => {
                write!(f, "{kind} must not contain `{ch}`: {value:?}")
            }
            IdError::DotSegment { kind } => write!(f, "{kind} cannot be `.` or `..`"),
            IdError::LeadingDot { kind, value } => {
                write!(f, "{kind} must not start with `.`: {value:?}")
            }
            IdError::NotHex { kind, value } => {
                write!(f, "{kind} must be lowercase hex; got {value:?}")
            }
            IdError::BadLength {
                kind,
                min,
                max,
                len,
            } => write!(f, "{kind} length must be {min}-{max} chars; got {len}"),
            IdError::Reserved {
                kind,
                value,
                reason,
            } => write!(f, "{kind} `{value}` is reserved: {reason}"),
        }
    }
}

impl std::error::Error for IdError {}

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

impl CommitRef {
    /// Parse an on-disk commit reference — the filename stem
    /// for a feedback file. 7–40 lowercase hex chars. NOT a
    /// commit identity by itself; use `resolve_against` to
    /// disambiguate against a known scope.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "CommitRef";
        const MIN: usize = 7;
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
        Ok(CommitRef(s.to_string()))
    }

    /// Resolve this ref against a known commit scope. Returns
    /// `Some(full)` iff exactly one commit in `scope` has a
    /// SHA that starts with `self.as_str()` (or equals it
    /// when `self` is 40 chars). `None` for no match (orphan)
    /// or multiple matches (ambiguous — shouldn't happen
    /// under the per-target filename-mode rule, but the type
    /// doesn't pretend it can't).
    pub fn resolve_against(&self, scope: &[CommitSha]) -> Option<CommitSha> {
        let prefix = self.as_str();
        let mut hit: Option<&CommitSha> = None;
        for sha in scope {
            if sha.as_str().starts_with(prefix) {
                if hit.is_some() {
                    return None; // ambiguous
                }
                hit = Some(sha);
            }
        }
        hit.cloned()
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

    /// Wrap an already-hex content hash without re-validating. Used by
    /// the daemon's `content_hash(&str)` factory which constructs the
    /// 64-char hex directly from blake3.
    pub fn from_hex_unchecked(hex: String) -> Self {
        ContentHash(hex)
    }
}

impl AgentLabel {
    /// Parse an agent label: non-empty, no `/`, not `.`/`..`, no
    /// leading `.` (avoids dotfile collisions in
    /// `.clank/agents/<author>/...`).
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
    /// `.clank/plans/<stem>.md`). Non-empty, no `/`, not `.`/`..`,
    /// no leading `.`, and not the literal reserved segment `_`
    /// (Phase 4 of `commit-first-review-model` reserves `_` as the
    /// ad-hoc-feedback path segment; allowing a real plan named `_`
    /// would shadow that path).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "PlanKey";
        check_non_empty(KIND, s)?;
        check_not_dot_segment(KIND, s)?;
        check_no_leading_dot(KIND, s)?;
        check_no_slash(KIND, s)?;
        if s == "_" {
            return Err(IdError::Reserved {
                kind: KIND,
                value: s.to_string(),
                reason: "`_` is reserved as the ad-hoc feedback path segment",
            });
        }
        Ok(PlanKey(s.to_string()))
    }

    /// Parse a repo-relative plan-file path into a `PlanKey`.
    ///
    /// Accepts exactly `.clank/plans/<stem>.md`. `<stem>` must be
    /// non-empty and must not contain path separators (no nesting).
    /// Dots inside the stem are allowed — `foo.v2.md` parses to
    /// `PlanKey("foo.v2")`. Only the trailing `.md` is treated as an
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
            [".clank", "plans", name] => *name,
            _ => return None,
        };

        let stem = stem_seg.strip_suffix(".md")?;
        // Route through PlanKey::parse so dot-segment / leading-dot /
        // forbidden-char rules are enforced for disk discovery the
        // same way they are for wire parsing.
        PlanKey::parse(stem).ok()
    }
}

impl SessionId {
    /// Parse the opaque session id produced by the running agent.
    /// Claude sets `CLAUDE_CODE_SESSION_ID` to a UUID v4 (36 chars);
    /// codex sets `CODEX_THREAD_ID` to a ULID rendered in the same
    /// 36-char dashed shape. Both use only `[0-9a-zA-Z-]`. We
    /// validate that charset over 8–128 chars: tight enough to
    /// catch typos, loose enough to survive if either agent
    /// changes its render (e.g. removes dashes).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        const KIND: &str = "SessionId";
        const MIN: usize = 8;
        const MAX: usize = 128;
        if s.len() < MIN || s.len() > MAX {
            return Err(IdError::BadLength {
                kind: KIND,
                min: MIN,
                max: MAX,
                len: s.len(),
            });
        }
        for ch in s.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '-') {
                return Err(IdError::ForbiddenChar {
                    kind: KIND,
                    ch,
                    value: s.to_string(),
                });
            }
        }
        Ok(SessionId(s.to_string()))
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
/// Parsing is filesystem-free — it only validates the wire shape.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsePlanIdError {
    Malformed,
    MissingMdSuffix,
    InvalidRepo(IdError),
    InvalidStem(IdError),
}

impl fmt::Display for ParsePlanIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParsePlanIdError::Malformed => {
                f.write_str("plan_id must be `<repo_basename>/<stem>.md`")
            }
            ParsePlanIdError::MissingMdSuffix => f.write_str("plan_id is missing the `.md` suffix"),
            ParsePlanIdError::InvalidRepo(e) => write!(f, "plan_id repo basename invalid: {e}"),
            ParsePlanIdError::InvalidStem(e) => write!(f, "plan_id stem invalid: {e}"),
        }
    }
}

impl std::error::Error for ParsePlanIdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParsePlanIdError::InvalidRepo(e) | ParsePlanIdError::InvalidStem(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn plan_key_active_path() {
        let k = PlanKey::from_path(&p(".clank/plans/foo.md")).unwrap();
        assert_eq!(k.as_str(), "foo");
    }

    #[test]
    fn plan_key_rejects_reserved_ad_hoc_segment() {
        // `_` is reserved for the ad-hoc feedback path segment;
        // PlanKey::parse must reject it so no plan can shadow that path.
        let err = PlanKey::parse("_").unwrap_err();
        assert!(
            matches!(
                err,
                IdError::Reserved {
                    kind: "PlanKey",
                    ..
                }
            ),
            "expected IdError::Reserved for `_`; got {err:?}"
        );
        // from_path routes through parse, so the rejection is inherited.
        assert!(PlanKey::from_path(&p(".clank/plans/_.md")).is_none());
    }

    #[test]
    fn plan_key_rejects_nested() {
        assert!(PlanKey::from_path(&p(".clank/plans/team/foo.md")).is_none());
        assert!(PlanKey::from_path(&p(".clank/plans/done/team/foo.md")).is_none());
    }

    #[test]
    fn plan_key_rejects_non_md() {
        assert!(PlanKey::from_path(&p(".clank/plans/foo.txt")).is_none());
        assert!(PlanKey::from_path(&p(".clank/plans/foo")).is_none());
    }

    #[test]
    fn plan_key_rejects_empty_stem() {
        assert!(PlanKey::from_path(&p(".clank/plans/.md")).is_none());
    }

    #[test]
    fn plan_key_accepts_dots_inside_stem() {
        let k = PlanKey::from_path(&p(".clank/plans/foo.v2.md")).unwrap();
        assert_eq!(k.as_str(), "foo.v2");
    }

    #[test]
    fn plan_key_rejects_off_tree_paths() {
        assert!(PlanKey::from_path(&p("plans/foo.md")).is_none());
        assert!(PlanKey::from_path(&p(".clank/notes/foo.md")).is_none());
    }

    #[test]
    fn plan_key_rejects_done_subdir() {
        assert!(PlanKey::from_path(&p(".clank/plans/done/foo.md")).is_none());
    }

    #[test]
    fn plan_id_parses_canonical_form() {
        let pid = PlanId::parse("clank/foo.md").unwrap();
        assert_eq!(pid.repo().as_str(), "clank");
        assert_eq!(pid.key().as_str(), "foo");
        assert_eq!(pid.to_string(), "clank/foo.md");
    }

    #[test]
    fn plan_id_accepts_dots_in_stem() {
        let pid = PlanId::parse("clank/foo.v2.md").unwrap();
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
            PlanId::parse("clank/foo").unwrap_err(),
            ParsePlanIdError::MissingMdSuffix
        );
    }

    #[test]
    fn plan_id_empty_stem_rejected() {
        assert!(matches!(
            PlanId::parse("clank/.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::Empty { .. })
        ));
    }

    #[test]
    fn plan_id_slash_in_stem_rejected() {
        assert!(matches!(
            PlanId::parse("clank/sub/foo.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::ForbiddenChar { .. })
        ));
    }

    #[test]
    fn plan_id_dot_stem_rejected() {
        assert!(matches!(
            PlanId::parse("clank/.hidden.md").unwrap_err(),
            ParsePlanIdError::InvalidStem(IdError::LeadingDot { .. })
        ));
    }

    #[test]
    fn plan_key_from_path_rejects_leading_dot() {
        assert!(PlanKey::from_path(&p(".clank/plans/.hidden.md")).is_none());
    }

    #[test]
    fn plan_id_roundtrips_through_display() {
        let original = "clank/plan-path-identity.md";
        let pid = PlanId::parse(original).unwrap();
        assert_eq!(pid.to_string(), original);
    }

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

    // ============ CommitRef ============

    #[test]
    fn commit_ref_accepts_7_through_40_hex() {
        assert!(CommitRef::parse("abc1234").is_ok());
        assert!(CommitRef::parse(&"a".repeat(40)).is_ok());
        assert!(CommitRef::parse(&"f".repeat(20)).is_ok());
    }

    #[test]
    fn commit_ref_rejects_under_7() {
        assert!(matches!(
            CommitRef::parse("abc123"),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn commit_ref_rejects_over_40() {
        let s = "a".repeat(41);
        assert!(matches!(
            CommitRef::parse(&s),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn commit_ref_rejects_uppercase() {
        assert!(matches!(
            CommitRef::parse("ABC1234"),
            Err(IdError::NotHex { .. })
        ));
    }

    #[test]
    fn commit_ref_rejects_non_hex() {
        assert!(matches!(
            CommitRef::parse("xyzqrst"),
            Err(IdError::NotHex { .. })
        ));
    }

    #[test]
    fn commit_ref_rejects_empty() {
        assert!(matches!(
            CommitRef::parse(""),
            Err(IdError::BadLength { .. })
        ));
    }

    #[test]
    fn commit_ref_resolve_full_in_scope() {
        let full = CommitSha::parse(&"a".repeat(40)).unwrap();
        let r = CommitRef::parse(&"a".repeat(40)).unwrap();
        assert_eq!(r.resolve_against(&[full.clone()]), Some(full));
    }

    #[test]
    fn commit_ref_resolve_short_matches_unique_scope() {
        let full = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let other = CommitSha::parse("ffeeddc1111111111111111111111111111111ff").unwrap();
        let r = CommitRef::parse("abcdef0").unwrap();
        assert_eq!(r.resolve_against(&[full.clone(), other]), Some(full));
    }

    #[test]
    fn commit_ref_resolve_ambiguous_returns_none() {
        let a = CommitSha::parse("abcdef0111111111111111111111111111111111").unwrap();
        let b = CommitSha::parse("abcdef0222222222222222222222222222222222").unwrap();
        let r = CommitRef::parse("abcdef0").unwrap();
        assert_eq!(r.resolve_against(&[a, b]), None);
    }

    #[test]
    fn commit_ref_resolve_orphan_returns_none() {
        let s = CommitSha::parse(&"a".repeat(40)).unwrap();
        let r = CommitRef::parse("deadbee").unwrap();
        assert_eq!(r.resolve_against(&[s]), None);
    }

    #[test]
    fn commit_ref_resolve_empty_scope_returns_none() {
        let r = CommitRef::parse("abc1234").unwrap();
        assert_eq!(r.resolve_against(&[]), None);
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
        assert!(RepoBasename::parse("clank").is_ok());
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
