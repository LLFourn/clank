use std::io::Read;
use std::path::{Path, PathBuf};

use super::{QueueArgs, QueueCmd, resolve_repo};

pub async fn run(args: QueueArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    match args.command {
        None => list(&repo),
        Some(QueueCmd::Add(a)) => add_cmd(&repo, a),
        Some(QueueCmd::Remove(r)) => remove(&repo, &r.name),
        Some(QueueCmd::Promote(p)) => promote(&repo, &p.name),
    }
}

/// Top-level orchestrator for `queue add`. Runs all cheap
/// validation (name shape, priority range, duplicate-name
/// conflict) BEFORE touching the body source — `--from -`
/// would otherwise block on stdin and `--from <missing>`
/// would fail on IO before we knew the queue item couldn't
/// land.
fn add_cmd(repo: &Path, args: super::QueueAddArgs) -> anyhow::Result<()> {
    validate_name(&args.name)?;
    if args.priority > 999 {
        anyhow::bail!("priority must be 0-999");
    }
    let scanned = scan_queue(repo);
    let conflicts: Vec<&QueueEntry> = scanned.iter().filter(|e| e.name == args.name).collect();
    if !conflicts.is_empty() {
        let listed = conflicts
            .iter()
            .map(|e| format!("{:03}-{}.md", e.priority, e.name))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "queue item `{}` already exists ({listed}). \
             Delete it from .clank/queue/ first or pick a different name.",
            args.name
        );
    }
    let source = pick_body_source(repo, &args.name, &args)?;
    add(repo, &args.name, args.priority, source)
}

/// Where a queued stub's body came from. Used for error
/// messages so the user knows what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodySourceKind {
    Inline,
    FromPath(PathBuf),
    Stdin,
    StubsDir(PathBuf),
}

impl BodySourceKind {
    fn describe(&self) -> String {
        match self {
            BodySourceKind::Inline => "-m".to_string(),
            BodySourceKind::Stdin => "--from -".to_string(),
            BodySourceKind::FromPath(p) => format!("--from {}", p.display()),
            BodySourceKind::StubsDir(p) => format!("`{}`", p.display()),
        }
    }
}

pub(crate) struct BodySource {
    pub raw: String,
    pub kind: BodySourceKind,
}

fn pick_body_source(
    repo: &Path,
    name: &str,
    args: &super::QueueAddArgs,
) -> anyhow::Result<BodySource> {
    if let Some(path) = &args.from {
        if path == &PathBuf::from("-") {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;
            return Ok(BodySource {
                raw: buf,
                kind: BodySourceKind::Stdin,
            });
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", path.display()))?;
        return Ok(BodySource {
            raw,
            kind: BodySourceKind::FromPath(path.clone()),
        });
    }
    if let Some(msg) = &args.message {
        return Ok(BodySource {
            raw: msg.clone(),
            kind: BodySourceKind::Inline,
        });
    }
    let stubs_path = repo.join(format!(".clank/stubs/{name}.md"));
    if stubs_path.is_file() {
        let raw = std::fs::read_to_string(&stubs_path)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", stubs_path.display()))?;
        return Ok(BodySource {
            raw,
            kind: BodySourceKind::StubsDir(stubs_path),
        });
    }
    anyhow::bail!(
        "no body for `{name}`. Pass one of:\n  \
         --from <path>   read body from a file (or `--from -` for stdin)\n  \
         -m \"<body>\"   inline body\n  \
         or write `.clank/stubs/{name}.md` and re-run."
    );
}

/// Ensure the body has non-empty content beyond the
/// `# <name>` header. Returns the normalized body (with a
/// leading header prepended when missing) or an error
/// citing the source.
fn normalize_and_validate(name: &str, source: BodySource) -> anyhow::Result<String> {
    let raw = source.raw;
    let has_header = raw
        .lines()
        .next()
        .map(|l| l.trim_start_matches('#').trim() == name)
        .unwrap_or(false);
    let post_header = if has_header {
        raw.lines().skip(1).collect::<Vec<_>>().join("\n")
    } else {
        raw.clone()
    };
    if post_header.trim().is_empty() {
        anyhow::bail!(
            "queued body for `{name}` from {} is empty after the header; pass non-empty content.",
            source.kind.describe()
        );
    }
    if has_header {
        Ok(raw)
    } else {
        Ok(format!("# {name}\n{raw}"))
    }
}

fn queue_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/queue")
}

pub struct QueueEntry {
    pub priority: u16,
    pub name: String,
    pub path: PathBuf,
}

/// Validate that no two entries share the same `name`. Returns
/// the entry list on success, or a loud error naming every
/// conflicting file pair. Callers that act on a queue item
/// (wfw promote, queue remove/promote) should use this; callers
/// that merely render (list, status count) can stay on
/// `scan_queue`.
pub fn scan_queue_no_dups(repo: &Path) -> anyhow::Result<Vec<QueueEntry>> {
    let entries = scan_queue(repo);
    let mut names: std::collections::BTreeMap<&str, Vec<&QueueEntry>> =
        std::collections::BTreeMap::new();
    for e in &entries {
        names.entry(e.name.as_str()).or_default().push(e);
    }
    let dupes: Vec<(String, Vec<String>)> = names
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(name, v)| {
            let files: Vec<String> = v
                .iter()
                .map(|e| format!("{:03}-{}.md", e.priority, e.name))
                .collect();
            (name.to_string(), files)
        })
        .collect();
    if dupes.is_empty() {
        return Ok(entries);
    }
    let listed = dupes
        .iter()
        .map(|(name, files)| format!("`{name}` matched {}", files.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::bail!(
        "ambiguous queue names: {listed}. \
         Delete the duplicate(s) from .clank/queue/ and re-run."
    );
}

pub fn scan_queue(repo: &Path) -> Vec<QueueEntry> {
    let dir = queue_dir(repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<QueueEntry> = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(s) = fname.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".md") else {
            continue;
        };
        if stem.len() < 5 || stem.as_bytes()[3] != b'-' {
            continue;
        }
        let Ok(priority) = stem[..3].parse::<u16>() else {
            continue;
        };
        let name = stem[4..].to_string();
        out.push(QueueEntry {
            priority,
            name,
            path: entry.path(),
        });
    }
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    out
}

fn list(repo: &Path) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    if entries.is_empty() {
        println!("queue is empty");
        return Ok(());
    }
    for e in &entries {
        println!("{:03}-{}", e.priority, e.name);
    }
    Ok(())
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    clank_core::ids::PlanKey::parse(name)
        .map_err(|e| anyhow::anyhow!("invalid plan name `{name}`: {e}"))?;
    Ok(())
}

fn add(repo: &Path, name: &str, priority: u16, source: BodySource) -> anyhow::Result<()> {
    validate_name(name)?;
    if priority > 999 {
        anyhow::bail!("priority must be 0-999");
    }
    let scanned = scan_queue(repo);
    let conflicts: Vec<&QueueEntry> = scanned.iter().filter(|e| e.name == name).collect();
    if !conflicts.is_empty() {
        let listed = conflicts
            .iter()
            .map(|e| format!("{:03}-{}.md", e.priority, e.name))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "queue item `{name}` already exists ({listed}). \
             Delete it from .clank/queue/ first or pick a different name."
        );
    }
    let body = normalize_and_validate(name, source)?;
    let dir = queue_dir(repo);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{priority:03}-{name}.md"));
    std::fs::write(&dest, body)?;
    println!(
        "queued `{name}` at priority {priority:03} ({})",
        dest.display()
    );
    Ok(())
}

fn find_unique<'a>(entries: &'a [QueueEntry], name: &str) -> anyhow::Result<&'a QueueEntry> {
    let matches: Vec<&QueueEntry> = entries.iter().filter(|e| e.name == name).collect();
    match matches.as_slice() {
        [] => anyhow::bail!("`{name}` not in queue"),
        [only] => Ok(*only),
        many => {
            let listed = many
                .iter()
                .map(|e| format!("{:03}-{}.md", e.priority, e.name))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "ambiguous queue name `{name}`: matched {listed}. \
                 Delete the duplicate from .clank/queue/ and re-run."
            );
        }
    }
}

fn remove(repo: &Path, name: &str) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    let entry = find_unique(&entries, name)?;
    std::fs::remove_file(&entry.path)?;
    println!("removed `{name}` from queue");
    Ok(())
}

fn promote(repo: &Path, name: &str) -> anyhow::Result<()> {
    validate_name(name)?;
    let entries = scan_queue(repo);
    let entry = find_unique(&entries, name)?;
    let dest = repo.join(format!(".clank/plans/{name}.md"));
    if dest.exists() {
        anyhow::bail!("plan `{name}` already exists in plans/");
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(&entry.path, &dest)?;
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["add", "--", &format!(".clank/plans/{name}.md")])
        .status()?;
    if !status.success() {
        anyhow::bail!("git add failed");
    }
    let msg = format!("[{name}] intro");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "commit",
            "--quiet",
            &format!(".clank/plans/{name}.md"),
            "-m",
            &msg,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("git commit failed");
    }
    println!("promoted `{name}` to active plan");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn scan_queue_returns_sorted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("200-beta.md"), "# beta\n");
        write(&q.join("100-alpha.md"), "# alpha\n");
        write(&q.join("100-gamma.md"), "# gamma\n");
        let entries = scan_queue(dir.path());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[0].priority, 100);
        assert_eq!(entries[1].name, "gamma");
        assert_eq!(entries[2].name, "beta");
    }

    #[test]
    fn scan_queue_ignores_malformed_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("100-good.md"), "ok\n");
        write(&q.join("bad.md"), "no prefix\n");
        write(&q.join("1000-overflow.md"), "4 digits\n");
        write(&q.join("notes.txt"), "not md\n");
        let entries = scan_queue(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
    }

    fn inline(body: &str) -> BodySource {
        BodySource {
            raw: body.to_string(),
            kind: BodySourceKind::Inline,
        }
    }

    #[test]
    fn invalid_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = add(dir.path(), ".hidden", 100, inline("body\n"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid plan name")
        );
    }

    #[test]
    fn priority_over_999_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = add(dir.path(), "foo", 1000, inline("body\n"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0-999"));
    }

    #[test]
    fn add_writes_stub_with_header_prepended() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "foo", 400, inline("real body\n")).unwrap();
        let path = dir.path().join(".clank/queue/400-foo.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "# foo\nreal body\n");
    }

    #[test]
    fn add_preserves_existing_header_when_present() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "foo", 400, inline("# foo\n\nreal body\n")).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".clank/queue/400-foo.md")).unwrap();
        assert_eq!(body, "# foo\n\nreal body\n");
    }

    #[test]
    fn add_rejects_inline_message_with_only_header() {
        let dir = tempfile::tempdir().unwrap();
        let result = add(dir.path(), "foo", 400, inline("# foo\n"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty after the header") && msg.contains("-m"),
            "unexpected error: {msg}"
        );
        assert!(
            !dir.path().join(".clank/queue/400-foo.md").exists(),
            "must not write a file on rejection"
        );
    }

    #[test]
    fn add_rejects_inline_message_whitespace_only() {
        let dir = tempfile::tempdir().unwrap();
        let result = add(dir.path(), "foo", 400, inline("   \n\n"));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("empty after"),
            "expected empty-after-header message"
        );
        assert!(!dir.path().join(".clank/queue/400-foo.md").exists());
    }

    #[test]
    fn add_refuses_when_same_name_any_priority() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("400-foo.md"), "# foo\n");
        let result = add(dir.path(), "foo", 410, inline("body\n"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("already exists") && msg.contains("400-foo.md"),
            "unexpected error: {msg}"
        );
        assert!(
            !dir.path().join(".clank/queue/410-foo.md").exists(),
            "destination must not be created on conflict"
        );
    }

    #[test]
    fn promote_fails_on_ambiguous_name() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("400-foo.md"), "# foo\n");
        write(&q.join("410-foo.md"), "# foo v2\n");
        let result = promote(dir.path(), "foo");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("ambiguous"), "unexpected error: {msg}");
        assert!(msg.contains("400-foo.md"));
        assert!(msg.contains("410-foo.md"));
        assert!(msg.contains("Delete the duplicate"));
    }

    #[test]
    fn remove_fails_on_ambiguous_name() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("400-foo.md"), "# foo\n");
        write(&q.join("410-foo.md"), "# foo v2\n");
        let result = remove(dir.path(), "foo");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ambiguous"));
        // Both files survive the failed remove.
        assert!(dir.path().join(".clank/queue/400-foo.md").is_file());
        assert!(dir.path().join(".clank/queue/410-foo.md").is_file());
    }
}
