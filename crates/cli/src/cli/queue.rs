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
/// conflict) BEFORE touching the body source, so a missing/empty
/// draft body fails only after we know the queue item could land.
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

/// Where a queued draft's body came from. Used for error
/// messages so the user knows what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodySourceKind {
    Inline,
    DraftsDir(PathBuf),
}

impl BodySourceKind {
    fn describe(&self) -> String {
        match self {
            BodySourceKind::Inline => "-m".to_string(),
            BodySourceKind::DraftsDir(p) => format!("`{}`", p.display()),
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
    if let Some(msg) = &args.message {
        return Ok(BodySource {
            raw: msg.clone(),
            kind: BodySourceKind::Inline,
        });
    }
    // The body lives in the drafts dir: write `.clank/drafts/<name>.md`,
    // then `queue add <name>` consumes it. Legacy `.clank/stubs/` is read
    // as a one-release fallback so an un-migrated repo's in-flight drafts
    // aren't orphaned — REMOVE this fallback once repos are migrated.
    let drafts_path = repo.join(format!(".clank/drafts/{name}.md"));
    let legacy = repo.join(format!(".clank/stubs/{name}.md"));
    let path = [drafts_path, legacy].into_iter().find(|p| p.is_file());
    if let Some(path) = path {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", path.display()))?;
        return Ok(BodySource {
            raw,
            kind: BodySourceKind::DraftsDir(path),
        });
    }
    anyhow::bail!(
        "no body for `{name}`. Either:\n  \
         write `.clank/drafts/{name}.md` and re-run, or\n  \
         pass -m \"<body>\" for a trivial inline body."
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
/// (wait promote, queue remove/promote) should use this; callers
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
    // Consume the draft after a successful write (below): capture its
    // path before `normalize_and_validate` takes `source`. ONLY the
    // drafts-dir source is consumed — `-m` names no file we own
    // (queue-add-consumes-draft).
    let draft_to_consume = match &source.kind {
        BodySourceKind::DraftsDir(p) => Some(p.clone()),
        _ => None,
    };
    let body = normalize_and_validate(name, source)?;
    // `.clank/drafts/` is this command's staging area; self-heal its
    // gitignore the first time queue add runs in a repo that predates
    // the `/drafts/` entry, mirroring fork (`/worktrees/`) and open
    // zellij (`/zellij/`) (drafts-gitignored).
    crate::init_facts::ensure_clank_gitignore_entry(repo, "/drafts/")
        .map_err(|e| anyhow::anyhow!("ensuring /drafts/ gitignore entry: {e}"))?;
    let dir = queue_dir(repo);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{priority:03}-{name}.md"));
    std::fs::write(&dest, body)?;
    // The queue entry is the record now — consume the draft. Strictly
    // after the write so a failed write leaves the draft for a retry;
    // best-effort, since the add itself already succeeded.
    if let Some(draft) = draft_to_consume {
        if let Err(e) = std::fs::remove_file(&draft) {
            eprintln!(
                "note: could not remove consumed draft `{}`: {e}",
                draft.display()
            );
        }
    }
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
    let plan_path = crate::init_facts::plan_md_rel(name);
    let dest = repo.join(&plan_path);
    if dest.exists() {
        anyhow::bail!("plan `{name}` already exists in plans/");
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(&entry.path, &dest)?;
    crate::git_plumbing::stage(repo, &plan_path)?;
    let msg = format!("[{name}] intro");
    crate::git_plumbing::commit_pathspec(repo, &plan_path, &msg)?;
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
    fn add_writes_draft_with_header_prepended() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "foo", 400, inline("real body\n")).unwrap();
        let path = dir.path().join(".clank/queue/400-foo.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "# foo\nreal body\n");
    }

    #[test]
    fn add_ensures_its_staging_dir_is_gitignored() {
        // queue add self-heals `.clank/.gitignore` to ignore `/drafts/`
        // (its staging area) even on a repo that predates the entry —
        // and even when the body came inline (drafts-gitignored).
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), "foo", 400, inline("real body\n")).unwrap();
        let gi = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
        assert!(gi.contains("/drafts/"), "queue add ignores /drafts/: {gi}");
    }

    #[test]
    fn add_consumes_a_draft_dir_source() {
        // queue-add-consumes-draft: a draft-sourced add deletes the draft
        // once the queue entry is written, so the staging dir
        // self-empties (no more leftover drafts piling up).
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join(".clank/drafts/foo.md");
        std::fs::create_dir_all(draft.parent().unwrap()).unwrap();
        std::fs::write(&draft, "# foo\nreal body\n").unwrap();

        let source = BodySource {
            raw: std::fs::read_to_string(&draft).unwrap(),
            kind: BodySourceKind::DraftsDir(draft.clone()),
        };
        add(dir.path(), "foo", 400, source).unwrap();

        assert!(
            dir.path().join(".clank/queue/400-foo.md").is_file(),
            "queue entry written"
        );
        assert!(!draft.exists(), "draft consumed (deleted) after queue add");
    }

    #[test]
    fn body_source_prefers_drafts_then_falls_back_to_legacy_stubs() {
        let dir = tempfile::tempdir().unwrap();
        let args = crate::cli::QueueAddArgs {
            name: "foo".into(),
            priority: 500,
            message: None,
        };

        // Legacy `.clank/stubs/` is read when `.clank/drafts/` is absent
        // (the one-release migration shim).
        let legacy = dir.path().join(".clank/stubs/foo.md");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "# foo\nfrom stubs\n").unwrap();
        let src = pick_body_source(dir.path(), "foo", &args).unwrap();
        assert!(src.raw.contains("from stubs"), "reads legacy stubs dir");

        // `.clank/drafts/` wins when both exist.
        let draft = dir.path().join(".clank/drafts/foo.md");
        std::fs::create_dir_all(draft.parent().unwrap()).unwrap();
        std::fs::write(&draft, "# foo\nfrom drafts\n").unwrap();
        let src = pick_body_source(dir.path(), "foo", &args).unwrap();
        assert!(
            src.raw.contains("from drafts"),
            "drafts dir takes precedence"
        );
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
