//! `clank init` — scaffold `.clank/` in a new repo.

use std::path::Path;

use super::{InitArgs, resolve_repo};

/// The single canonical content of `.clank/.gitignore`.
const GITIGNORE_BODY: &str = "feedback/\ncache/\n";

pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    write_scaffold(&repo)?;
    warn_if_globally_excluded(&repo);
    Ok(())
}

fn write_scaffold(repo: &Path) -> anyhow::Result<()> {
    let clank_dir = repo.join(".clank");
    let plans_dir = clank_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)?;

    let gitignore = clank_dir.join(".gitignore");
    match std::fs::read_to_string(&gitignore) {
        Ok(existing) if existing == GITIGNORE_BODY => {
            println!("{} already up to date", gitignore.display());
        }
        Ok(_existing) => {
            anyhow::bail!(
                "{} exists with different content; refusing to overwrite. \
                 Inspect it, delete it, or edit it to match the documented content:\n{}",
                gitignore.display(),
                GITIGNORE_BODY
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&gitignore, GITIGNORE_BODY)?;
            println!("wrote {}", gitignore.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Warn (but don't fail) if some ancestor `.gitignore` or
/// `core.excludesFile` excludes `.clank/` wholesale — that would
/// hide tracked plan files too.
///
/// Probes `.clank/plans/` (not the directory we just created — git
/// matches patterns against the path, not its existence) and parses
/// `git check-ignore -v`'s structured output:
///   `<source_file>:<line>:<pattern>\t<probed_path>`
/// We suppress only when `<source_file>` is the `.clank/.gitignore`
/// we just wrote. Substring matching on the whole record would be
/// fooled by paths or patterns that happen to contain that literal.
fn warn_if_globally_excluded(repo: &Path) {
    let probe = repo.join(".clank/plans");
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-v"])
        .arg(probe.as_path())
        .output();
    let Ok(output) = output else { return };
    match output.status.code() {
        Some(0) => {}
        Some(1) => return,
        _ => {
            tracing::debug!(
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "check-ignore probe failed unexpectedly"
            );
            return;
        }
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end();
    if line.is_empty() {
        return;
    }
    if matched_by_clank_gitignore(line) {
        return;
    }
    eprintln!(
        "warning: an ancestor .gitignore (or core.excludesFile) excludes .clank/ — \
         tracked plan files would be hidden. Source:\n  {line}"
    );
}

/// True if the matched rule lives in `<repo>/.clank/.gitignore`
/// (the file we just wrote). `git check-ignore -v` emits records as
/// `<source_file>:<line>:<pattern>\t<probed>`; we parse the
/// `source_file` column and check it's our managed file.
fn matched_by_clank_gitignore(record: &str) -> bool {
    let (source_part, _probed) = match record.split_once('\t') {
        Some(parts) => parts,
        None => return false,
    };
    let source_file = match source_part.split(':').next() {
        Some(s) => s,
        None => return false,
    };
    let p = Path::new(source_file);
    p.file_name().is_some_and(|n| n == ".gitignore")
        && p.parent().is_some_and(|parent| parent.ends_with(".clank"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let s = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "--quiet", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(s.success());
        dir
    }

    #[test]
    fn writes_scaffold_creates_plans_dir_and_gitignore() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        assert!(dir.path().join(".clank/plans").is_dir());
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn writes_scaffold_is_idempotent_when_content_matches() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn matched_by_clank_gitignore_recognises_our_managed_file() {
        // Standard git check-ignore -v output: <source>:<line>:<pattern>\t<probed>.
        let record = ".clank/.gitignore:1:plans/extra\t.clank/plans/extra/foo";
        assert!(matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_ancestor_gitignore() {
        // Pattern column mentions the literal `.clank/.gitignore`
        // but the matching rule is in the repo-root .gitignore.
        let record = ".gitignore:5:!.clank/.gitignore\t.clank/.gitignore";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_unrelated_source() {
        let record = "../.gitignore:2:.clank/\t.clank/plans";
        assert!(!matched_by_clank_gitignore(record));
    }

    #[test]
    fn matched_by_clank_gitignore_rejects_missing_tab() {
        assert!(!matched_by_clank_gitignore(""));
        assert!(!matched_by_clank_gitignore(".clank/.gitignore:1:plans/"));
    }

    #[test]
    fn writes_scaffold_refuses_on_drifted_content() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        std::fs::write(dir.path().join(".clank/.gitignore"), "something/else\n").unwrap();
        let err = write_scaffold(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("different content"), "unexpected error: {msg}");
        let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert_eq!(
            body, "something/else\n",
            "drifted file must not be overwritten"
        );
    }
}
