#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub mode: FileDiffMode,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDiffMode {
    Added,
    Removed,
    Renamed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Insert,
    Delete,
    Context,
    Meta,
}

pub fn parse_diff(raw: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut old_lineno = 0_usize;
    let mut new_lineno = 0_usize;

    for line in raw.lines() {
        if let Some((old_path, new_path)) = parse_diff_git(line) {
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(FileDiff {
                path: new_path,
                old_path: Some(old_path),
                additions: 0,
                deletions: 0,
                mode: FileDiffMode::Modified,
                hunks: Vec::new(),
                binary: false,
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
            file.old_path = Some(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            file.mode = FileDiffMode::Renamed;
            file.path = rest.to_string();
            continue;
        }
        if line.starts_with("Binary files ") {
            file.binary = true;
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("index ") {
            continue;
        }
        if line.starts_with("@@ ") {
            let (old_start, new_start) = parse_hunk_starts(line);
            old_lineno = old_start;
            new_lineno = new_start;
            file.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };

        if let Some(content) = line.strip_prefix('+') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Insert,
                old_lineno: None,
                new_lineno: Some(new_lineno),
                content: content.to_string(),
            });
            file.additions += 1;
            new_lineno += 1;
        } else if let Some(content) = line.strip_prefix('-') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Delete,
                old_lineno: Some(old_lineno),
                new_lineno: None,
                content: content.to_string(),
            });
            file.deletions += 1;
            old_lineno += 1;
        } else if let Some(content) = line.strip_prefix(' ') {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_lineno: Some(old_lineno),
                new_lineno: Some(new_lineno),
                content: content.to_string(),
            });
            old_lineno += 1;
            new_lineno += 1;
        } else {
            hunk.lines.push(DiffLine {
                kind: DiffLineKind::Meta,
                old_lineno: None,
                new_lineno: None,
                content: line.to_string(),
            });
        }
    }

    if let Some(file) = current {
        files.push(file);
    }
    files
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

impl FileDiff {
    pub fn changed_lines(&self) -> usize {
        self.additions + self.deletions
    }

    pub fn anchor(&self, index: usize) -> String {
        format!("file-{index}")
    }

    pub fn is_collapsed_by_default(&self, total_files: usize) -> bool {
        if total_files <= 1 {
            return false;
        }
        self.changed_lines() > 60 || is_always_folded(&self.path)
    }
}

pub fn is_always_folded(path: &str) -> bool {
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
+new
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
@@ -1 +1 @@
-fn a() {}
+fn b() {}
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].additions, 1);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[1].mode, FileDiffMode::Renamed);
        assert_eq!(files[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(files[1].path, "new.rs");
    }

    #[test]
    fn detects_binary_file() {
        let raw = "\
diff --git a/img.png b/img.png
index 111..222 100644
Binary files a/img.png and b/img.png differ
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 1);
        assert!(files[0].binary);
        assert!(files[0].hunks.is_empty());
    }
}
