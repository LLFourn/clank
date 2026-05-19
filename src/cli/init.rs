//! `trinity init` — scaffold `.trinity/` in a new repo.

use std::path::{Path, PathBuf};

use super::InitArgs;

/// The single canonical content of `.trinity/.gitignore`.
const GITIGNORE_BODY: &str = "feedback/\ncache/\n";

pub async fn run(args: InitArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    write_scaffold(&repo)?;
    warn_if_globally_excluded(&repo);
    Ok(())
}

fn resolve_repo(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "no --repo given and `git rev-parse --show-toplevel` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let root = String::from_utf8(output.stdout)?.trim().to_string();
        PathBuf::from(root)
    };
    Ok(dunce::canonicalize(&raw)?)
}

fn write_scaffold(repo: &Path) -> anyhow::Result<()> {
    let trinity_dir = repo.join(".trinity");
    let plans_dir = trinity_dir.join("plans");
    std::fs::create_dir_all(&plans_dir)?;

    let gitignore = trinity_dir.join(".gitignore");
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
/// `core.excludesFile` excludes `.trinity/` wholesale — that would
/// hide tracked plan files too.
///
/// Probes `.trinity/plans/` (not the directory we just created — git
/// matches patterns against the path, not its existence) and parses
/// `git check-ignore -v`'s structured output:
///   `<source_file>:<line>:<pattern>\t<probed_path>`
/// We suppress only when `<source_file>` is the `.trinity/.gitignore`
/// we just wrote. Substring matching on the whole record would be
/// fooled by paths or patterns that happen to contain that literal.
fn warn_if_globally_excluded(repo: &Path) {
    let probe = repo.join(".trinity/plans");
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
    if matched_by_trinity_gitignore(line) {
        return;
    }
    eprintln!(
        "warning: an ancestor .gitignore (or core.excludesFile) excludes .trinity/ — \
         tracked plan files would be hidden. Source:\n  {line}"
    );
}

/// True if the matched rule lives in `<repo>/.trinity/.gitignore`
/// (the file we just wrote). `git check-ignore -v` emits records as
/// `<source_file>:<line>:<pattern>\t<probed>`; we parse the
/// `source_file` column and check it's our managed file.
fn matched_by_trinity_gitignore(record: &str) -> bool {
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
        && p.parent()
            .is_some_and(|parent| parent.ends_with(".trinity"))
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
        assert!(dir.path().join(".trinity/plans").is_dir());
        let body = std::fs::read_to_string(dir.path().join(".trinity/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn writes_scaffold_is_idempotent_when_content_matches() {
        let dir = init_repo();
        write_scaffold(dir.path()).unwrap();
        write_scaffold(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".trinity/.gitignore")).unwrap();
        assert_eq!(body, GITIGNORE_BODY);
    }

    #[test]
    fn matched_by_trinity_gitignore_recognises_our_managed_file() {
        // Standard git check-ignore -v output: <source>:<line>:<pattern>\t<probed>.
        let record = ".trinity/.gitignore:1:plans/extra\t.trinity/plans/extra/foo";
        assert!(matched_by_trinity_gitignore(record));
    }

    #[test]
    fn matched_by_trinity_gitignore_rejects_ancestor_gitignore() {
        // Pattern column mentions the literal `.trinity/.gitignore`
        // but the matching rule is in the repo-root .gitignore.
        let record = ".gitignore:5:!.trinity/.gitignore\t.trinity/.gitignore";
        assert!(!matched_by_trinity_gitignore(record));
    }

    #[test]
    fn matched_by_trinity_gitignore_rejects_unrelated_source() {
        let record = "../.gitignore:2:.trinity/\t.trinity/plans";
        assert!(!matched_by_trinity_gitignore(record));
    }

    #[test]
    fn matched_by_trinity_gitignore_rejects_missing_tab() {
        assert!(!matched_by_trinity_gitignore(""));
        assert!(!matched_by_trinity_gitignore(
            ".trinity/.gitignore:1:plans/"
        ));
    }

    #[test]
    fn writes_scaffold_refuses_on_drifted_content() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".trinity")).unwrap();
        std::fs::write(dir.path().join(".trinity/.gitignore"), "something/else\n").unwrap();
        let err = write_scaffold(dir.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("different content"), "unexpected error: {msg}");
        let body = std::fs::read_to_string(dir.path().join(".trinity/.gitignore")).unwrap();
        assert_eq!(
            body, "something/else\n",
            "drifted file must not be overwritten"
        );
    }
}
