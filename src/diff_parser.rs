//! Parse `git diff` output into typed `trinity_core::api` structures.
//!
//! The parser writes directly into the wire types so there's no
//! duplicate diff-DTO family in the daemon (`FileDiff` etc. live in
//! `trinity_core::api`) and no boundary mapper. The path-based
//! always-folded rule (lockfiles, generated fixtures) is applied
//! here as the parser sets `FileDiff.always_folded`.

use trinity_core::api::{DiffHunk, DiffLine, FileDiff, FileDiffMode};

pub fn parse_diff(raw: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut old_lineno = 0_usize;
    let mut new_lineno = 0_usize;

    for line in raw.lines() {
        if let Some((old_path, new_path)) = parse_diff_git(line) {
            if let Some(file) = current.take() {
                files.push(finalize(file));
            }
            current = Some(FileDiff {
                path: new_path,
                old_path: Some(old_path),
                additions: 0,
                deletions: 0,
                mode: FileDiffMode::Modified,
                binary: false,
                always_folded: false,
                hunks: Vec::new(),
            });
            old_lineno = 0;
            new_lineno = 0;
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.mode = FileDiffMode::Added;
            file.old_path = None;
            continue;
        }
        if line.starts_with("deleted file mode ") {
            file.mode = FileDiffMode::Removed;
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            file.mode = FileDiffMode::Renamed;
            file.old_path = Some(strip_git_prefix(rest).to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            file.mode = FileDiffMode::Renamed;
            file.path = strip_git_prefix(rest).to_string();
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
            continue;
        }
        if line.starts_with("@@ ") {
            let (old_start, new_start) = parse_hunk_starts(line);
            old_lineno = old_start;
            new_lineno = new_start;
            file.hunks.push(DiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };

        if let Some(content) = line.strip_prefix('+') {
            hunk.lines.push(DiffLine::Insert {
                new_lineno,
                content: content.to_string(),
            });
            file.additions += 1;
            new_lineno += 1;
        } else if let Some(content) = line.strip_prefix('-') {
            hunk.lines.push(DiffLine::Delete {
                old_lineno,
                content: content.to_string(),
            });
            file.deletions += 1;
            old_lineno += 1;
        } else if let Some(content) = line.strip_prefix(' ') {
            hunk.lines.push(DiffLine::Context {
                old_lineno,
                new_lineno,
                content: content.to_string(),
            });
            old_lineno += 1;
            new_lineno += 1;
        } else {
            hunk.lines.push(DiffLine::Meta {
                content: line.to_string(),
            });
        }
    }

    if let Some(file) = current {
        files.push(finalize(file));
    }
    files
}

/// Set the path-based `always_folded` flag once per file, so the
/// parser is the single point that decides which files default to
/// the SPA's collapsed view.
fn finalize(mut file: FileDiff) -> FileDiff {
    file.always_folded = is_always_folded(&file.path);
    file
}

fn parse_diff_git(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let mut parts = rest.split_whitespace();
    let old = strip_git_prefix(parts.next()?);
    let new = strip_git_prefix(parts.next()?);
    Some((old.to_string(), new.to_string()))
}

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn parse_hunk_starts(line: &str) -> (usize, usize) {
    let mut old_start = 0;
    let mut new_start = 0;
    for token in line.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            old_start = parse_start(rest);
        } else if let Some(rest) = token.strip_prefix('+') {
            new_start = parse_start(rest);
        }
    }
    (old_start, new_start)
}

fn parse_start(token: &str) -> usize {
    token
        .split(',')
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

fn is_always_folded(path: &str) -> bool {
    path == "Cargo.lock"
        || path.ends_with("-lock.json")
        || path.ends_with(".min.js")
        || path.ends_with(".min.css")
        || path == "pnpm-lock.yaml"
        || path == "yarn.lock"
        || path.ends_with(".snap")
        || path.starts_with("tests/fixtures/")
        || path.contains("/fixtures/")
        || path.contains("/golden/")
        || (path.starts_with("tests/") && path.ends_with(".json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multifile_diff_with_counts_and_modes() {
        let raw = "\
diff --git a/a.txt b/a.txt
index 111..222 100644
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 old
-gone
+kept
+new
diff --git a/b.txt b/c.txt
similarity index 100%
rename from b.txt
rename to c.txt
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[0].mode, FileDiffMode::Modified);
        assert_eq!(files[1].mode, FileDiffMode::Renamed);
        assert_eq!(files[1].old_path.as_deref(), Some("b.txt"));
        assert_eq!(files[1].path, "c.txt");
    }

    #[test]
    fn always_folded_marks_lockfiles() {
        let raw = "\
diff --git a/Cargo.lock b/Cargo.lock
index 111..222 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,0 +1,1 @@
+x
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 1);
        assert!(files[0].always_folded);
    }

    #[test]
    fn always_folded_clears_for_normal_files() {
        let raw = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +1,1 @@
+x
";
        let files = parse_diff(raw);
        assert!(!files[0].always_folded);
    }
}
