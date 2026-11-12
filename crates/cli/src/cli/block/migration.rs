//! Journaled move of legacy per-agent custody into the repository namespace.
//! The repo lock is held by the caller; legacy locks fence any in-flight writer.
//! Questions publish last. A journal survives any interruption, including after
//! source removal, and retains original bytes for repair and name mappings.
use super::*;
use std::collections::BTreeMap;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Journal {
    entries: BTreeMap<String, Entry>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    name: String,
    creator: String,
    files: BTreeMap<String, Vec<u8>>,
    applied: bool,
}

pub(super) fn run(repo: &Path, dry: bool) -> anyhow::Result<()> {
    let journal_path = root(repo).join("block-migration.json");
    let mut journal: Journal = match std::fs::read(&journal_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal::default(),
        Err(e) => return Err(e.into()),
    };
    let mut agents: Vec<_> = match std::fs::read_dir(agents_root(repo)) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.into()),
    };
    agents.sort_by_key(|e| e.file_name());
    let mut guards = vec![];
    for agent in &agents {
        if !agent.file_type()?.is_dir() {
            continue;
        }
        guards.push(crate::agent_store::exclusive_lock(
            &agent.path().join("blocks.lock"),
        )?);
        let creator = agent
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid legacy agent directory"))?;
        // A torn question must not make its delivery evidence disappear.
        // Journaled leftovers are handled below, including interrupted removals.
        for folder in ["blocks", "unblocks"] {
            check_orphans(&agent.path(), folder, Path::new(""), &creator, &journal)?;
        }
        let mut questions = vec![];
        collect(&agent.path().join("blocks"), Path::new(""), &mut questions)?;
        questions.sort();
        for relative in questions {
            let source = format!("{creator}/{}", relative.display());
            if journal.entries.contains_key(&source) {
                continue;
            }
            let base = relative
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("invalid legacy block name"))?;
            validate_name(base)?;
            let taken = |name: &str| {
                let f = files(&root(repo), name);
                [&f.question, &f.answer, &f.marker, &f.attempt, &f.creator]
                    .iter()
                    .any(|p| p.exists())
                    || journal.entries.values().any(|e| e.name == name)
            };
            let mut name = base.to_owned();
            if taken(&name) {
                name = format!("{base}-{}", &digest(&source)[..12]);
                anyhow::ensure!(
                    !taken(&name),
                    "migration name collision for {source}; repair required"
                );
            }
            let mut saved = BTreeMap::new();
            for suffix in ["question", "answer", "marker", "attempt"] {
                let path = legacy_file(repo, &source, suffix)?;
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        saved.insert(suffix.to_owned(), bytes);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound
                            && suffix != "question"
                            && absent(&path) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            eprintln!("block migration: {source} -> {name}");
            journal.entries.insert(
                source,
                Entry {
                    name,
                    creator: creator.clone(),
                    files: saved,
                    applied: false,
                },
            );
        }
    }
    if dry {
        return Ok(());
    }
    if journal.entries.is_empty() {
        return Ok(());
    }
    // Save before touching custody; don't rewrite an unchanged journal on reads.
    persist_changed(&journal_path, &journal)?;
    let sources: Vec<_> = journal.entries.keys().cloned().collect();
    for source in sources {
        let entry = &journal.entries[&source];
        validate_name(&entry.name)?;
        if !entry.applied {
            let f = files(&root(repo), &entry.name);
            for (suffix, dst) in [
                ("answer", &f.answer),
                ("marker", &f.marker),
                ("attempt", &f.attempt),
            ] {
                if let Some(bytes) = entry.files.get(suffix) {
                    copy_once(dst, bytes)?;
                }
            }
            copy_once(&f.creator, entry.creator.as_bytes())?;
            copy_once(
                &f.question,
                entry
                    .files
                    .get("question")
                    .ok_or_else(|| anyhow::anyhow!("migration journal has no question"))?,
            )?;
            journal.entries.get_mut(&source).unwrap().applied = true;
            persist_changed(&journal_path, &journal)?;
        }
        // A new-format writer may now change the destination. Never recopy it.
        // Any legacy writer arriving after migration is detected, not erased.
        for suffix in ["question", "answer", "marker", "attempt"] {
            let path = legacy_file(repo, &source, suffix)?;
            match std::fs::read(&path) {
                Ok(now) => {
                    anyhow::ensure!(
                        journal.entries[&source].files.get(suffix) == Some(&now),
                        "legacy block {source} changed after migration; stop old processes and reconcile custody"
                    );
                    remove_if_present(&path)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn check_orphans(
    agent: &Path,
    folder: &str,
    relative: &Path,
    creator: &str,
    journal: &Journal,
) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(agent.join(folder).join(relative)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let relative = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            check_orphans(agent, folder, &relative, creator, journal)?;
        } else if relative.extension().is_some_and(|e| {
            e == "attempt" || (folder == "unblocks" && (e == "md" || e == "received"))
        }) {
            let question = relative.with_extension("md");
            let source = format!("{creator}/{}", question.display());
            anyhow::ensure!(
                agent.join("blocks").join(&question).exists()
                    || journal.entries.contains_key(&source),
                "legacy block evidence has no question: {}; repair required",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn collect(dir: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            collect(&entry.path(), &path, out)?;
        } else if entry.file_type()?.is_file() && path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        } else if entry.file_type()?.is_symlink() {
            anyhow::bail!(
                "legacy block symlink needs repair: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}
fn legacy_file(repo: &Path, source: &str, suffix: &str) -> anyhow::Result<PathBuf> {
    let (creator, relative) = source
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid block migration source"))?;
    clank_core::AgentLabel::parse(creator)?;
    anyhow::ensure!(
        Path::new(relative)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "invalid migration path"
    );
    let (folder, extension) = match suffix {
        "question" => ("blocks", "md"),
        "attempt" => ("blocks", "attempt"),
        "answer" => ("unblocks", "md"),
        "marker" => ("unblocks", "received"),
        _ => anyhow::bail!("unknown migration file kind"),
    };
    Ok(agents_root(repo)
        .join(creator)
        .join(folder)
        .join(relative)
        .with_extension(extension))
}
fn copy_once(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    match std::fs::read(path) {
        Ok(now) => anyhow::ensure!(
            now == bytes,
            "migration destination changed: {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => publish(path, bytes)?,
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn persist_changed(path: &Path, journal: &Journal) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(journal)?;
    if std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
        publish(path, &bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_copy_resumes_and_completed_journal_never_replays_a_changed_answer() {
        let repo = tempfile::tempdir().unwrap();
        let repo = repo.path();
        let source = "removed/q.md".to_string();
        let entry = Entry {
            name: "q".into(),
            creator: "removed".into(),
            applied: false,
            files: [
                ("question".into(), b"Which?".to_vec()),
                ("answer".into(), b"B".to_vec()),
            ]
            .into(),
        };
        let journal = Journal {
            entries: [(source.clone(), entry)].into(),
        };
        let _lock = raw_lock(repo).unwrap();
        for (suffix, bytes) in &journal.entries[&source].files {
            publish(&legacy_file(repo, &source, suffix).unwrap(), bytes).unwrap();
        }
        persist_changed(&root(repo).join("block-migration.json"), &journal).unwrap();
        // Interruption after the first sidecar, before the question was published.
        publish(&files(&root(repo), "q").answer, b"B").unwrap();
        run(repo, false).unwrap();
        assert_eq!(
            Pair::read(&root(repo), "q").unwrap().answer.as_deref(),
            Some("B")
        );
        assert!(!legacy_file(repo, &source, "question").unwrap().exists());
        publish(&files(&root(repo), "q").answer, b"corrected decision").unwrap();
        run(repo, false).unwrap();
        assert_eq!(
            Pair::read(&root(repo), "q").unwrap().answer.as_deref(),
            Some("corrected decision")
        );
        // A retired binary writing again is detected, never erased or imported twice.
        publish(
            &legacy_file(repo, &source, "question").unwrap(),
            b"new stale ask",
        )
        .unwrap();
        assert!(run(repo, false).is_err());
        assert_eq!(
            std::fs::read(legacy_file(repo, &source, "question").unwrap()).unwrap(),
            b"new stale ask"
        );
    }
}
