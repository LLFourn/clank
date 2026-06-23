//! Shared knowledge of what `clank init` manages: canonical bodies,
//! markers, paths, and read-side state classifiers. `init.rs`
//! (writer) and `cli/open.rs` (reader) both depend on this so the
//! "what state does init recognize?" predicates live in exactly
//! one place.

use std::path::{Path, PathBuf};

/// The managed entries of `.clank/.gitignore` — everything
/// local-only: agent state, caches, the queue, queue-add drafts
/// (the `.clank/drafts/<name>.md` staging area for `queue add`),
/// rendered html, shelved-plan state (plan-lifecycle-verbs), fork
/// worktrees (clank-fork-worktree-sessions), generated zellij
/// layouts.
///
/// The file is validated and repaired by SET MEMBERSHIP, not
/// exact string match (ruthless 02da305): three commands mutate
/// it (init writes/repairs; fork ensures `/worktrees/`;
/// open zellij ensures `/zellij/`), so order-insensitive
/// "contains every managed entry, nothing foreign" is the only
/// model under which incremental appends can't drift the file
/// out of recognition. This also killed the combinatorial
/// legacy-bodies list (every new entry demanded enumerating its
/// subset x ordering permutations — which is exactly how
/// /shelved/ slipped).
pub const CLANK_GITIGNORE_ENTRIES: &[&str] = &[
    "/agents/",
    "/cache/",
    "/feedback/",
    "/queue/",
    "/drafts/",
    "/html/",
    "/pr-reviews/",
    "/shelved/",
    "/worktrees/",
    "/zellij/",
];

/// Canonical WRITE form for fresh files: the managed entries,
/// one per line, in `CLANK_GITIGNORE_ENTRIES` order.
pub fn clank_gitignore_body() -> String {
    let mut out = String::new();
    for e in CLANK_GITIGNORE_ENTRIES {
        out.push_str(e);
        out.push('\n');
    }
    out
}

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

/// Resolve the real `post-rewrite` hook path in the SHARED
/// (common) git dir. `None` if it can't be resolved (not a git
/// repo). Honors `.git`-as-file linked worktrees: hooks live in
/// the main repo's shared `hooks/`, so this uses the common dir.
pub fn post_rewrite_hook_path(repo: &Path) -> Option<PathBuf> {
    // `hooks/` is SHARED across linked worktrees → the common dir.
    Some(
        crate::git_io::common_dir(repo)
            .ok()?
            .join("hooks/post-rewrite"),
    )
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
        Ok(body) => classify_gitignore_body(&body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GitignoreState::Missing,
        // Permission errors / IO errors: treat as drifted so we
        // surface a warning rather than silently claim Ready.
        Err(_) => GitignoreState::Drifted,
    }
}

/// Set-membership classification (pure): every non-empty line
/// must be a managed entry (else Drifted — foreign content we
/// won't touch); all managed entries present → Canonical (order
/// irrelevant); some missing → Legacy (repairable by appending).
pub fn classify_gitignore_body(body: &str) -> GitignoreState {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.iter().any(|l| !CLANK_GITIGNORE_ENTRIES.contains(l)) {
        return GitignoreState::Drifted;
    }
    if CLANK_GITIGNORE_ENTRIES.iter().all(|e| lines.contains(e)) {
        GitignoreState::Canonical
    } else {
        GitignoreState::Legacy
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
        std::fs::write(clank_gitignore_path(dir.path()), clank_gitignore_body()).unwrap();
        assert_eq!(
            classify_clank_gitignore(dir.path()),
            GitignoreState::Canonical
        );
    }

    #[test]
    fn classify_clank_gitignore_legacy() {
        // SET model: a subset of managed entries (any order) is
        // Legacy/repairable.
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(
            clank_gitignore_path(dir.path()),
            "/cache/\n/agents/\n/worktrees/\n",
        )
        .unwrap();
        assert_eq!(classify_clank_gitignore(dir.path()), GitignoreState::Legacy);
    }

    #[test]
    fn drafts_is_a_managed_gitignore_entry_with_legacy_repair() {
        // `.clank/drafts/` is the queue-add staging area and MUST be
        // gitignored everywhere (drafts-gitignored).
        assert!(
            clank_gitignore_body().contains("/drafts/\n"),
            "drafts in the canonical body"
        );
        // A repo whose .clank/.gitignore predates /drafts/ (the prior
        // canonical set) classifies Legacy, and the shared ensure
        // repairs it back to canonical without disturbing the rest.
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        let prior = "/agents/\n/cache/\n/feedback/\n/queue/\n/html/\n\
                     /pr-reviews/\n/shelved/\n/worktrees/\n/zellij/\n";
        std::fs::write(clank_gitignore_path(dir.path()), prior).unwrap();
        assert_eq!(classify_clank_gitignore(dir.path()), GitignoreState::Legacy);

        ensure_clank_gitignore_entry(dir.path(), "/drafts/").unwrap();
        assert_eq!(
            classify_clank_gitignore(dir.path()),
            GitignoreState::Canonical,
            "repaired to canonical after adding /drafts/"
        );
    }

    #[test]
    fn ensure_entry_appends_and_is_idempotent() {
        // The shared single-entry ensure (fork: /worktrees/,
        // open zellij: /zellij/) — append once, never duplicate,
        // create the file when missing.
        let dir = tempdir();
        ensure_clank_gitignore_entry(dir.path(), "/zellij/").unwrap();
        ensure_clank_gitignore_entry(dir.path(), "/zellij/").unwrap();
        let body = std::fs::read_to_string(clank_gitignore_path(dir.path())).unwrap();
        assert_eq!(body.matches("/zellij/").count(), 1, "{body}");

        std::fs::write(clank_gitignore_path(dir.path()), "/agents/\n").unwrap();
        ensure_clank_gitignore_entry(dir.path(), "/worktrees/").unwrap();
        let body = std::fs::read_to_string(clank_gitignore_path(dir.path())).unwrap();
        assert!(body.starts_with("/agents/\n"), "existing preserved: {body}");
        assert!(body.contains("/worktrees/\n"), "appended: {body}");
    }

    #[test]
    fn classify_gitignore_body_is_order_insensitive_and_set_based() {
        // Canonical = all managed entries, ANY order.
        let reversed: String = CLANK_GITIGNORE_ENTRIES
            .iter()
            .rev()
            .map(|e| format!("{e}\n"))
            .collect();
        assert_eq!(
            classify_gitignore_body(&reversed),
            GitignoreState::Canonical
        );
        // A managed subset PLUS a foreign line = Drifted (foreign
        // wins — we never touch user content).
        assert_eq!(
            classify_gitignore_body("/agents/\nmy-custom-thing\n"),
            GitignoreState::Drifted
        );
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
/// entry (fork: /worktrees/, open zellij: /zellij/, queue add:
/// /drafts/).
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
