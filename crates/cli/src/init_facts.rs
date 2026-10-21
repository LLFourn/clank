//! Shared knowledge of what `clank init` manages: canonical bodies,
//! markers, paths, and read-side state classifiers. `init.rs`
//! (writer) and `cli/open.rs` (reader) both depend on this so the
//! "what state does init recognize?" predicates live in exactly
//! one place.

use std::path::{Path, PathBuf};

/// The canonical `.clank/.gitignore` — a single-source ALLOW-LIST: ignore
/// everything under `.clank/` except the tracked plan docs and this file
/// itself. `/*` must precede the `!` re-includes (gitignore order matters),
/// so this is order-SENSITIVE and validated by exact match — the opposite of
/// the old per-dir set model.
///
/// Why an allow-list: (1) one tracked source of truth (no root/nested
/// duplication that can drift); (2) future-proof — a NEW `.clank/<subdir>`
/// is ignored by `/*` with no edit, so there are NO incremental appenders to
/// mutate the file (the old `ensure_clank_gitignore_entry` is gone). The
/// tracked surface is exactly `plans/` + `finished/` + this file.
pub const CLANK_GITIGNORE_ENTRIES: &[&str] = &["/*", "!/plans/", "!/finished/", "!/.gitignore"];

/// The pre-allow-list per-dir deny list — recognized so an existing repo's
/// `.clank/.gitignore` classifies `Legacy` and `clank init` rewrites it to
/// the allow-list. NOT written anymore.
pub const LEGACY_GITIGNORE_ENTRIES: &[&str] = &[
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

/// Canonical WRITE form: the allow-list entries, one per line, in order.
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

pub fn clank_dir(repo: &Path) -> PathBuf {
    repo.join(".clank")
}

pub fn clank_gitignore_path(repo: &Path) -> PathBuf {
    clank_dir(repo).join(".gitignore")
}

/// Repo-relative path of an ACTIVE plan's markdown
/// (`.clank/plans/<stem>.md`). The single source for where active plan
/// files live, so the convention isn't re-spelled at each call site.
pub fn plan_md_rel(stem: &str) -> String {
    format!(".clank/plans/{stem}.md")
}

/// Repo-relative path of a FINISHED plan's markdown
/// (`.clank/finished/<stem>.md`) — the finalize move's destination.
pub fn finished_md_rel(stem: &str) -> String {
    format!(".clank/finished/{stem}.md")
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

/// Order-AWARE classification (pure): the allow-list is order-sensitive, so
/// `Canonical` requires an EXACT ordered match. A body made only of the old
/// per-dir deny entries is `Legacy` (init rewrites it to the allow-list).
/// Anything else — a foreign body, or a modified/reordered allow-list — is
/// `Drifted` (init warns, never clobbers).
pub fn classify_gitignore_body(body: &str) -> GitignoreState {
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.as_slice() == CLANK_GITIGNORE_ENTRIES {
        return GitignoreState::Canonical;
    }
    if !lines.is_empty() && lines.iter().all(|l| LEGACY_GITIGNORE_ENTRIES.contains(l)) {
        return GitignoreState::Legacy;
    }
    GitignoreState::Drifted
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
    fn classify_allow_list_is_order_sensitive() {
        // Unlike the old set model, the allow-list is ORDER-sensitive: `/*`
        // must precede the `!` re-includes, so a reordered body is NOT
        // Canonical (it's a modified body → Drifted).
        let reversed: String = CLANK_GITIGNORE_ENTRIES
            .iter()
            .rev()
            .map(|e| format!("{e}\n"))
            .collect();
        assert_ne!(
            classify_gitignore_body(&reversed),
            GitignoreState::Canonical,
            "reordered allow-list must not be Canonical"
        );
    }

    #[test]
    fn classify_recognizes_legacy_and_flags_a_modified_allow_list() {
        // The full pre-allow-list per-dir deny list → Legacy (init rewrites
        // it to the allow-list).
        let legacy: String = LEGACY_GITIGNORE_ENTRIES
            .iter()
            .map(|e| format!("{e}\n"))
            .collect();
        assert_eq!(classify_gitignore_body(&legacy), GitignoreState::Legacy);
        // A canonical allow-list with an APPENDED entry — the exact drift the
        // deleted appender would have caused — is Drifted, never Canonical.
        let appended = format!("{}/drafts/\n", clank_gitignore_body());
        assert_eq!(classify_gitignore_body(&appended), GitignoreState::Drifted);
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
}

// `ensure_clank_gitignore_entry` (the per-dir appender) is DELETED: under the
// allow-list every `.clank/` subdir is ignored by `/*`, so there is nothing
// to append — and an append would drift the order-sensitive body out of
// `Canonical`. Its former callers (fork/open-zellij/queue) no
// longer touch the gitignore.
