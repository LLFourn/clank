//! Shared knowledge of what `clank init` manages: canonical bodies,
//! markers, paths, and read-side state classifiers. `init.rs`
//! (writer) and `cli/open.rs` (reader) both depend on this so the
//! "what state does init recognize?" predicates live in exactly
//! one place.

use std::path::{Path, PathBuf};

/// Canonical body of `.clank/.gitignore`. Everything local-only
/// lives here: agent state, caches, the queue, rendered html,
/// shelved-plan state (plan-lifecycle-verbs), fork worktrees
/// (clank-fork-worktree-sessions — codex 335c0fc caught the
/// default location not being covered), and generated zellij
/// layouts.
pub const CLANK_GITIGNORE_BODY: &str =
    "/agents/\n/cache/\n/feedback/\n/queue/\n/html/\n/shelved/\n/worktrees/\n/zellij/\n";

/// Older bodies init silently upgrades to `CLANK_GITIGNORE_BODY`.
/// Anything else makes init bail.
pub const CLANK_GITIGNORE_LEGACY_BODIES: &[&str] = &[
    "/agents/\n/cache/\n/feedback/\n/queue/\n/html/\n",
    "/agents/\n/cache/\n/feedback/\n/queue/\n/html/\n/zellij/\n",
    "/agents/\n/cache/\n/feedback/\n/queue/\n",
    "/agents/\n/cache/\n/feedback/\n",
    "feedback/\ncache/\n",
    "feedback/\ncache/\nagents/*/config.json\n",
    "/feedback/\n/cache/\nagents/*/config.json\n",
];

/// Marker line embedded in the canonical post-rewrite hook so we
/// can recognize our own across re-runs.
pub const POST_REWRITE_MARKER: &str = "# clank rewire hook";

/// Canonical body of `.git/hooks/post-rewrite`.
pub const POST_REWRITE_BODY: &str = "#!/usr/bin/env sh\n\
# clank rewire hook\n\
exec clank rewire --from-stdin\n";

/// Permission rules tag-merged into `.claude/settings.local.json`'s
/// `permissions.allow` array.
pub const CLAUDE_ALLOW_RULES: &[&str] = &[
    "Write(.clank/agents/**)",
    "Edit(.clank/agents/**)",
    "Read(.clank/agents/**)",
];

pub fn clank_dir(repo: &Path) -> PathBuf {
    repo.join(".clank")
}

pub fn clank_gitignore_path(repo: &Path) -> PathBuf {
    clank_dir(repo).join(".gitignore")
}

pub fn claude_perms_path(repo: &Path) -> PathBuf {
    repo.join(".claude").join("settings.local.json")
}

/// Resolve the real `post-rewrite` hook path via
/// `git rev-parse --git-path`. Returns `None` if git can't
/// resolve it (not a git repo). Honors `.git`-as-file linked
/// worktrees: the path lands in the main repo's shared
/// `hooks/` dir.
pub fn post_rewrite_hook_path(repo: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-path", "hooks/post-rewrite"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(&raw);
    Some(if p.is_absolute() { p } else { repo.join(p) })
}

/// Classification of `.clank/.gitignore`'s current state.
///
/// `Missing` and `Legacy` are repaired by `clank init`.
/// `Drifted` makes init bail; surface as a warning, not an
/// InitGap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitignoreState {
    Missing,
    Canonical,
    Legacy,
    Drifted,
}

pub fn classify_clank_gitignore(repo: &Path) -> GitignoreState {
    match std::fs::read_to_string(clank_gitignore_path(repo)) {
        Ok(body) if body == CLANK_GITIGNORE_BODY => GitignoreState::Canonical,
        Ok(body)
            if CLANK_GITIGNORE_LEGACY_BODIES
                .iter()
                .any(|legacy| *legacy == body) =>
        {
            GitignoreState::Legacy
        }
        Ok(_) => GitignoreState::Drifted,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GitignoreState::Missing,
        // Permission errors / IO errors: treat as drifted so we
        // surface a warning rather than silently claim Ready.
        Err(_) => GitignoreState::Drifted,
    }
}

/// Classification of the resolved `post-rewrite` hook.
///
/// `Missing` (absent) and `Refreshable` (marker-present but
/// body has drifted) are repaired by `clank init`. `Foreign`
/// (file exists, no marker) makes init only warn — needs
/// `--force-hooks` — so surface as a warning, not an InitGap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    /// Git can't resolve a hook path (not a real git repo).
    Indeterminate,
    Missing,
    Canonical,
    Refreshable,
    Foreign,
}

pub fn classify_post_rewrite_hook(repo: &Path) -> HookState {
    let Some(path) = post_rewrite_hook_path(repo) else {
        return HookState::Indeterminate;
    };
    match std::fs::read_to_string(&path) {
        Ok(body) if body == POST_REWRITE_BODY => HookState::Canonical,
        Ok(body) if body.contains(POST_REWRITE_MARKER) => HookState::Refreshable,
        Ok(_) => HookState::Foreign,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HookState::Missing,
        Err(_) => HookState::Foreign,
    }
}

/// Classification of `.claude/settings.local.json`'s
/// `permissions.allow` array vs `CLAUDE_ALLOW_RULES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudePermsState {
    Missing,
    /// File exists but `permissions.allow` lacks one or more
    /// of our rules. Init tag-merges to repair.
    NeedsPatch {
        missing_rules: Vec<String>,
    },
    Complete,
    /// File exists but isn't valid JSON, or has the wrong
    /// shape. Init bails; surface as a warning.
    Drifted,
}

pub fn classify_claude_perms(repo: &Path) -> ClaudePermsState {
    let path = claude_perms_path(repo);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ClaudePermsState::Missing;
        }
        Err(_) => return ClaudePermsState::Drifted,
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return ClaudePermsState::Drifted,
    };
    let Some(obj) = v.as_object() else {
        return ClaudePermsState::Drifted;
    };
    // Init's repair only works when the path is well-shaped at
    // every level it touches: top-level object, `permissions`
    // either absent or an object, `permissions.allow` either
    // absent or an array. Anything else makes
    // `write_claude_perms` bail — surface as Drifted, not
    // NeedsPatch.
    let allow = match obj.get("permissions") {
        None => None, // init will fill in
        Some(p) if p.is_object() => {
            let pobj = p.as_object().unwrap();
            match pobj.get("allow") {
                None => None, // init will fill in
                Some(a) if a.is_array() => Some(a.as_array().unwrap()),
                Some(_) => return ClaudePermsState::Drifted,
            }
        }
        Some(_) => return ClaudePermsState::Drifted,
    };
    let Some(allow) = allow else {
        return ClaudePermsState::NeedsPatch {
            missing_rules: CLAUDE_ALLOW_RULES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
    };
    let present: Vec<&str> = allow.iter().filter_map(|v| v.as_str()).collect();
    let missing: Vec<String> = CLAUDE_ALLOW_RULES
        .iter()
        .filter(|rule| !present.iter().any(|p| p == *rule))
        .map(|s| (*s).to_string())
        .collect();
    if missing.is_empty() {
        ClaudePermsState::Complete
    } else {
        ClaudePermsState::NeedsPatch {
            missing_rules: missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn classify_clank_gitignore_missing_when_absent() {
        let dir = tempdir();
        assert_eq!(
            classify_clank_gitignore(dir.path()),
            GitignoreState::Missing
        );
    }

    #[test]
    fn classify_clank_gitignore_canonical() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(clank_gitignore_path(dir.path()), CLANK_GITIGNORE_BODY).unwrap();
        assert_eq!(
            classify_clank_gitignore(dir.path()),
            GitignoreState::Canonical
        );
    }

    #[test]
    fn classify_clank_gitignore_legacy() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(
            clank_gitignore_path(dir.path()),
            CLANK_GITIGNORE_LEGACY_BODIES[0],
        )
        .unwrap();
        assert_eq!(classify_clank_gitignore(dir.path()), GitignoreState::Legacy);
    }

    #[test]
    fn classify_clank_gitignore_drifted_on_foreign_content() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(clank_gitignore_path(dir.path()), "totally-foreign\n").unwrap();
        assert_eq!(
            classify_clank_gitignore(dir.path()),
            GitignoreState::Drifted
        );
    }

    #[test]
    fn classify_claude_perms_missing_when_absent() {
        let dir = tempdir();
        assert_eq!(classify_claude_perms(dir.path()), ClaudePermsState::Missing);
    }

    #[test]
    fn classify_claude_perms_complete() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            claude_perms_path(dir.path()),
            r#"{"permissions":{"allow":[
                "Write(.clank/agents/**)",
                "Edit(.clank/agents/**)",
                "Read(.clank/agents/**)"
            ]}}"#,
        )
        .unwrap();
        assert_eq!(
            classify_claude_perms(dir.path()),
            ClaudePermsState::Complete
        );
    }

    #[test]
    fn classify_claude_perms_needs_patch_when_partial() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            claude_perms_path(dir.path()),
            r#"{"permissions":{"allow":["Write(.clank/agents/**)"]}}"#,
        )
        .unwrap();
        match classify_claude_perms(dir.path()) {
            ClaudePermsState::NeedsPatch { missing_rules } => {
                assert!(missing_rules.iter().any(|r| r.starts_with("Edit")));
                assert!(missing_rules.iter().any(|r| r.starts_with("Read")));
            }
            other => panic!("expected NeedsPatch, got {other:?}"),
        }
    }

    #[test]
    fn classify_claude_perms_drifted_when_garbage_json() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(claude_perms_path(dir.path()), "not json").unwrap();
        assert_eq!(classify_claude_perms(dir.path()), ClaudePermsState::Drifted);
    }

    #[test]
    fn classify_claude_perms_drifted_when_permissions_not_object() {
        // init.rs bails when permissions isn't a JSON object;
        // open.rs must surface that as a drift warning, not as
        // a fixable NeedsPatch.
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(claude_perms_path(dir.path()), r#"{"permissions":"oh no"}"#).unwrap();
        assert_eq!(classify_claude_perms(dir.path()), ClaudePermsState::Drifted);
    }

    #[test]
    fn classify_claude_perms_drifted_when_allow_not_array() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            claude_perms_path(dir.path()),
            r#"{"permissions":{"allow":"single-string-not-array"}}"#,
        )
        .unwrap();
        assert_eq!(classify_claude_perms(dir.path()), ClaudePermsState::Drifted);
    }
}

/// Idempotently ensure `<repo>/.clank/.gitignore` contains
/// `entry` (one line). Shared by the commands that create
/// local-only state in repos whose gitignore may predate the
/// entry (fork: /worktrees/, open zellij: /zellij/).
pub fn ensure_clank_gitignore_entry(repo: &Path, entry: &str) -> std::io::Result<()> {
    let path = clank_gitignore_path(repo);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if body.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    let mut new_body = body;
    if !new_body.is_empty() && !new_body.ends_with('\n') {
        new_body.push('\n');
    }
    new_body.push_str(entry);
    new_body.push('\n');
    std::fs::write(&path, new_body)
}
