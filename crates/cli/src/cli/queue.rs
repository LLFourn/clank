use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::{QueueArgs, QueueCmd, resolve_repo};
use crate::priority_dir::{self, Lookup};

pub async fn run(args: QueueArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    match args.command {
        None => list(&repo),
        Some(QueueCmd::Add(a)) => add_cmd(&repo, a),
        Some(QueueCmd::Remove(r)) => remove(&repo, &r.name),
        Some(QueueCmd::Promote(p)) => {
            promote(&repo, &p.name)?;
            println!("promoted `{}` to active plan", p.name);
            Ok(())
        }
        Some(QueueCmd::Reprioritise(r)) => reprioritise(&repo, &r.name, r.priority),
        Some(QueueCmd::Unqueue(u)) => {
            let draft = unqueue(&repo, &u.name)?;
            println!(
                "unqueued `{}` → {}",
                u.name,
                draft.strip_prefix(&repo).unwrap_or(&draft).display()
            );
            Ok(())
        }
    }
}

/// Top-level orchestrator for `queue add`. Runs all cheap
/// validation (name shape, priority range, duplicate-name
/// conflict) BEFORE touching the body source, so a missing/empty
/// draft body fails only after we know the queue item could land.
fn add_cmd(repo: &Path, args: super::QueueAddArgs) -> anyhow::Result<()> {
    validate_name(&args.name)?;
    priority_dir::check_priority(args.priority)?;
    let scanned = scan_queue(repo);
    let conflicts: Vec<&QueueEntry> = scanned.iter().filter(|e| e.name == args.name).collect();
    if !conflicts.is_empty() {
        let listed = conflicts
            .iter()
            .map(|e| priority_dir::file_name(e.priority, &e.name))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "queue item `{}` already exists ({listed}). \
             Delete it from .clank/queue/ first or pick a different name.",
            args.name
        );
    }
    let source = pick_body_source(repo, &args.name, &args)?;
    let dest = add(repo, &args.name, args.priority, source)?;
    println!(
        "queued `{}` at priority {:03} ({})",
        args.name,
        args.priority,
        dest.display()
    );
    Ok(())
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

pub type QueueEntry = priority_dir::Entry;

/// The entry `handle` means: its file stem exactly, else its NAME when
/// that names exactly one entry. Ambiguity is an error naming the
/// stems, so a caller can say which; a parsed name that no file
/// carries is "not in the queue".
pub fn resolve_entry(repo: &Path, handle: &str) -> anyhow::Result<QueueEntry> {
    let entries = scan_queue(repo);
    match priority_dir::by_handle(&entries, handle) {
        Lookup::One(e) => Ok(e.clone()),
        Lookup::None => anyhow::bail!("`{handle}` is not in the queue (see `clank queue`)"),
        Lookup::Many(many) => anyhow::bail!(
            "`{handle}` names {} queue files ({}); pass the file stem",
            many.len(),
            many.iter()
                .map(|e| e.file_stem())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
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
                .map(|e| {
                    e.path
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| priority_dir::file_name(e.priority, &e.name))
                })
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
    priority_dir::scan(&queue_dir(repo))
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

/// THE definition of a valid queue/plan name (PlanKey). Also called by
/// fork's `--draft` preflight so an invalid name fails before any
/// mutation, not inside `add` after the worktree exists (codex 82f3e9c).
pub(crate) fn validate_name(name: &str) -> anyhow::Result<()> {
    clank_core::ids::PlanKey::parse(name)
        .map_err(|e| anyhow::anyhow!("invalid plan name `{name}`: {e}"))?;
    Ok(())
}

/// The queue-add core: validate, refuse stem collisions, write the
/// canonical `{priority:03}-{name}.md` entry, and consume a drafts-dir
/// source AFTER the write (queue-add-consumes-draft). Quiet — callers
/// own the user-facing print (`clank fork create --draft` seeds a fork's queue
/// through here and must keep its own stdout contract). Returns the
/// written entry's path.
pub(crate) fn add(
    repo: &Path,
    name: &str,
    priority: u16,
    source: BodySource,
) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    priority_dir::check_priority(priority)?;
    let scanned = scan_queue(repo);
    let conflicts: Vec<&QueueEntry> = scanned.iter().filter(|e| e.name == name).collect();
    if !conflicts.is_empty() {
        let listed = conflicts
            .iter()
            .map(|e| priority_dir::file_name(e.priority, &e.name))
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
    // `.clank/drafts/` (this command's staging area) is gitignored by the
    // `.clank/.gitignore` allow-list (`/*`) — no per-dir entry to add.
    let dir = queue_dir(repo);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(priority_dir::file_name(priority, name));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)?
        .write_all(body.as_bytes())?;
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
    Ok(dest)
}

fn find_unique<'a>(entries: &'a [QueueEntry], name: &str) -> anyhow::Result<&'a QueueEntry> {
    match priority_dir::by_name(entries, name) {
        Lookup::None => anyhow::bail!("`{name}` not in queue"),
        Lookup::One(only) => Ok(only),
        Lookup::Many(many) => {
            let listed = many
                .iter()
                .map(|e| priority_dir::file_name(e.priority, &e.name))
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

/// Change a queued item's priority by renaming its `NNN-` prefix — the
/// ONE validated mutation both the CLI (`queue reprioritise`) and the TUI
/// nudge go through (no ad-hoc renames). Same priority is a no-op.
/// Returns the entry's (possibly new) path.
pub fn set_priority(repo: &Path, name: &str, priority: u16) -> anyhow::Result<PathBuf> {
    let entries = scan_queue(repo);
    let entry = find_unique(&entries, name)?.clone();
    set_priority_of(repo, &entry, priority)
}

/// Move a queued file onto a name NOBODY holds — the move and the
/// claim in one step, so two callers wanting one destination cannot
/// both believe they got it. A plan body is the thing being moved;
/// there is no second copy to recover it from.
fn take_the_name(from: &Path, to: &Path) -> std::io::Result<()> {
    priority_dir::rename_noreplace(from, to)
}

/// The entry a FILE STEM names, exactly. Unlike [`resolve_entry`]
/// there is no fallback to the parsed name: a handle whose file is
/// gone is GONE, not resolved to a sibling that happens to share its
/// name. What a surface addresses by stem, it addresses one of
/// (a-queued-plan-has-a-page), and a queue that changed under a phone
/// must refuse rather than act on the wrong plan.
pub fn entry_by_stem(repo: &Path, stem: &str) -> anyhow::Result<QueueEntry> {
    scan_queue(repo)
        .into_iter()
        .find(|e| e.file_stem() == stem)
        .ok_or_else(|| anyhow::anyhow!("`{stem}` is no longer in the queue"))
}

/// Read the Markdown source of exactly the queued file the page names.
pub fn read_entry(repo: &Path, stem: &str) -> anyhow::Result<(QueueEntry, String, String)> {
    let entry = entry_by_stem(repo, stem)?;
    let mut file = std::fs::File::open(&entry.path)
        .map_err(|e| anyhow::anyhow!("opening queued plan `{stem}`: {e}"))?;
    let mut body = String::new();
    file.read_to_string(&mut body)?;
    let stamp = revision(&file.metadata()?);
    if stamp != revision(&std::fs::metadata(&entry.path)?) {
        anyhow::bail!("queued plan `{stem}` moved while it was opened; reopen it");
    }
    Ok((entry, body, stamp))
}

/// Save an editor's document only while its handle and original source
/// still match. Opening without `create` cannot resurrect a moved entry.
pub fn save_entry(
    repo: &Path,
    stem: &str,
    expected: &str,
    expected_revision: &str,
    body: String,
) -> anyhow::Result<()> {
    let entry = entry_by_stem(repo, stem)?;
    let body = normalize_and_validate(
        &entry.name,
        BodySource {
            raw: body,
            kind: BodySourceKind::Inline,
        },
    )?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&entry.path)
        .map_err(|e| anyhow::anyhow!("opening queued plan `{stem}`: {e}"))?;
    let mut current = String::new();
    file.read_to_string(&mut current)?;
    if current != expected
        || revision(&file.metadata()?) != expected_revision
        || revision(&std::fs::metadata(&entry.path)?) != expected_revision
    {
        anyhow::bail!("queued plan `{stem}` changed since it was opened; reopen it before saving");
    }
    file.set_len(0)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
    file.write_all(body.as_bytes())?;
    Ok(())
}

fn revision(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}",
        meta.dev(),
        meta.ino(),
        meta.ctime(),
        meta.ctime_nsec()
    )
}

/// Change THIS entry's priority. The primitive both the name-addressed
/// verb and an entry-addressed caller go through.
pub fn set_priority_of(repo: &Path, entry: &QueueEntry, priority: u16) -> anyhow::Result<PathBuf> {
    priority_dir::check_priority(priority)?;
    priority_dir::set_priority(&queue_dir(repo), entry, priority)
}

fn reprioritise(repo: &Path, name: &str, priority: u16) -> anyhow::Result<()> {
    let dest = set_priority(repo, name, priority)?;
    println!(
        "`{name}` now at priority {priority:03} ({})",
        dest.display()
    );
    Ok(())
}

/// Promote a queued item to an active plan: move it into `plans/`
/// and commit the intro. Shared by the CLI verb and the TUI's page,
/// so it prints nothing — the caller says what happened on its own
/// surface.
pub fn promote(repo: &Path, name: &str) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    let entry = find_unique(&entries, name)?.clone();
    promote_entry(repo, &entry)
}

/// Promote THIS entry, whatever else shares its name.
pub fn promote_entry(repo: &Path, entry: &QueueEntry) -> anyhow::Result<()> {
    let name = entry.name.clone();
    let name = name.as_str();
    validate_name(name)?;
    let plan_path = crate::init_facts::plan_md_rel(name);
    let dest = repo.join(&plan_path);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    // Atomically, and refusing a taken name: two queue files may parse
    // to ONE name, so two promotes can want this exact destination —
    // and a check followed by a rename lets both pass the check and
    // the second overwrite the first's plan (codex on 22e5fbe).
    take_the_name(&entry.path, &dest).map_err(|e| {
        // Only a TAKEN NAME is that; a source that moved between the
        // lookup and here, or a refusal from the filesystem, says what
        // it was rather than an explanation that fits the other case
        // (codex on 66488eb).
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("plan `{name}` already exists in plans/")
        } else {
            anyhow::anyhow!("moving `{}` into plans: {e}", entry.path.display())
        }
    })?;
    crate::git_plumbing::stage(repo, &plan_path)?;
    let msg = format!("[{name}] intro");
    crate::git_plumbing::commit_pathspec(repo, &plan_path, &msg)?;
    Ok(())
}

/// Take a queued item OUT of the queue and back to the drafts dir —
/// the inverse of `queue add`, which consumed the draft. `remove`
/// deletes; this keeps the body. Refuses to overwrite a draft that
/// already exists under the name, and names it. Shared by the CLI
/// verb and the TUI's page, which never renames files itself
/// (a-queued-plan-has-a-page). Returns the draft's path.
pub fn unqueue(repo: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let entries = scan_queue(repo);
    let entry = find_unique(&entries, name)?.clone();
    unqueue_entry(repo, &entry)
}

/// Take THIS entry out of the queue, whatever else shares its name.
pub fn unqueue_entry(repo: &Path, entry: &QueueEntry) -> anyhow::Result<PathBuf> {
    let name = entry.name.as_str();
    let drafts = repo.join(".clank/drafts");
    let dest = drafts.join(format!("{name}.md"));
    std::fs::create_dir_all(&drafts)?;
    // The same hazard as a promote: two entries of one name both want
    // `drafts/<name>.md`, and an overwriting rename keeps one body and
    // loses the other (codex on 22e5fbe).
    take_the_name(&entry.path, &dest).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!(
                "`{}` already exists; move it aside before unqueuing `{name}`",
                dest.display()
            )
        } else {
            anyhow::anyhow!("moving `{}` to drafts: {e}", entry.path.display())
        }
    })?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A handle is a FILE, and two queued files may parse to one
    /// name. A surface that addresses an entry by stem acts on that
    /// entry, and one whose file is gone is gone — not resolved to the
    /// sibling that shares its name, which is the way a phone ends up
    /// promoting a plan nobody asked for
    /// (the-queue-is-workable-from-the-phone).
    #[test]
    fn a_handle_names_one_file_and_never_its_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let q = repo.join(".clank/queue");
        write(&q.join("100-foo.md"), "# foo\n\nthe first\n");
        write(&q.join("900-foo.md"), "# foo\n\nthe second\n");

        // The lenient parse gives both the same name, so the name is
        // not an address: `resolve_entry` refuses it, and by stem each
        // is itself.
        assert!(resolve_entry(repo, "foo").is_err(), "the name is ambiguous");
        assert!(
            entry_by_stem(repo, "foo").is_err(),
            "a NAME is not a handle, even when it names something"
        );
        assert_eq!(entry_by_stem(repo, "100-foo").unwrap().priority, 100);
        assert_eq!(entry_by_stem(repo, "900-foo").unwrap().priority, 900);

        // The entry-addressed mutation acts on the file it was given.
        let entry = entry_by_stem(repo, "900-foo").unwrap();
        let draft = unqueue_entry(repo, &entry).unwrap();
        assert!(
            std::fs::read_to_string(&draft)
                .unwrap()
                .contains("the second"),
            "the one that was named, not its sibling"
        );
        assert!(q.join("100-foo.md").exists(), "and the other is untouched");

        // A handle whose file is gone stays gone, though a file that
        // parses to the same name is right there.
        assert!(
            entry_by_stem(repo, "900-foo").is_err(),
            "a spent handle resolves to nothing"
        );
    }

    /// Two entries of ONE name want one destination, and a plan body
    /// is the thing being moved — there is no second copy. Whichever
    /// order they run in, one moves and the other is refused with its
    /// source intact; a check followed by a rename let both pass the
    /// check and the second overwrite the first (codex on 22e5fbe).
    #[test]
    fn two_entries_of_one_name_cannot_overwrite_each_other() {
        for (first, second) in [("100-foo", "900-foo"), ("900-foo", "100-foo")] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            let q = repo.join(".clank/queue");
            write(&q.join("100-foo.md"), "# foo\n\nthe first\n");
            write(&q.join("900-foo.md"), "# foo\n\nthe second\n");

            let one = entry_by_stem(repo, first).unwrap();
            let two = entry_by_stem(repo, second).unwrap();
            unqueue_entry(repo, &one).expect("the first move takes the name");
            let refused = unqueue_entry(repo, &two).expect_err("the name is taken");
            assert!(
                refused.to_string().contains("already exists"),
                "and says so: {refused}"
            );

            // Neither body is lost: one is in the drafts, the other is
            // still queued where it was.
            let draft = std::fs::read_to_string(repo.join(".clank/drafts/foo.md")).unwrap();
            let left = std::fs::read_to_string(&two.path).expect("the refused entry is untouched");
            let bodies = [draft.clone(), left.clone()];
            assert!(
                bodies.iter().any(|b| b.contains("the first"))
                    && bodies.iter().any(|b| b.contains("the second")),
                "both bodies survive: {bodies:?}"
            );
        }
    }

    /// A refusal says what actually happened. Only a taken name is a
    /// taken name: a source that moved between the lookup and the
    /// move said "already exists in plans/" while plans/ held nothing
    /// (codex on 66488eb).
    #[test]
    fn a_promote_that_fails_for_another_reason_says_that_reason() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(".clank/queue/100-foo.md"), "# foo\n");
        let entry = entry_by_stem(repo, "100-foo").unwrap();
        // Gone between the lookup and the move, as another surface
        // promoting it would leave it.
        std::fs::remove_file(&entry.path).unwrap();

        let said = promote_entry(repo, &entry)
            .expect_err("there is nothing to move")
            .to_string();
        assert!(
            !said.contains("already exists"),
            "plans/ holds nothing; that is not what happened: {said}"
        );
        assert!(said.contains("100-foo.md"), "it names the file: {said}");
        assert!(
            !repo.join(".clank/plans/foo.md").exists(),
            "and nothing was made"
        );
    }

    /// The two moves at ONCE, which is the shape the phone makes
    /// possible: each request runs on its own worker, and a check
    /// followed by a rename lets both pass the check and the second
    /// overwrite the first — one body gone, and a plan body is the
    /// thing being moved (codex on 22e5fbe).
    #[test]
    fn concurrent_unqueues_of_one_name_keep_both_bodies() {
        for round in 0..25 {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().to_path_buf();
            let q = repo.join(".clank/queue");
            write(&q.join("100-foo.md"), "# foo\n\nthe first\n");
            write(&q.join("900-foo.md"), "# foo\n\nthe second\n");
            let one = entry_by_stem(&repo, "100-foo").unwrap();
            let two = entry_by_stem(&repo, "900-foo").unwrap();

            let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
            let hands: Vec<_> = [one, two]
                .into_iter()
                .map(|entry| {
                    let (repo, gate) = (repo.clone(), gate.clone());
                    std::thread::spawn(move || {
                        gate.wait();
                        unqueue_entry(&repo, &entry)
                    })
                })
                .collect();
            let done: Vec<_> = hands.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(
                done.iter().filter(|r| r.is_ok()).count(),
                1,
                "round {round}: one takes the name, one is refused: {done:?}"
            );
            // Neither body is lost: one moved, the other is still
            // queued where it was.
            let mut bodies =
                vec![std::fs::read_to_string(repo.join(".clank/drafts/foo.md")).unwrap()];
            for left in std::fs::read_dir(&q).unwrap() {
                bodies.push(std::fs::read_to_string(left.unwrap().path()).unwrap());
            }
            assert!(
                bodies.iter().any(|b| b.contains("the first")),
                "round {round}: the first body survives: {bodies:?}"
            );
            assert!(
                bodies.iter().any(|b| b.contains("the second")),
                "round {round}: the second body survives: {bodies:?}"
            );
        }
    }

    /// A promote wants `plans/<name>.md`, which two entries of one name
    /// both want. The second is refused rather than replacing the plan
    /// the first just made.
    #[test]
    fn a_promote_does_not_replace_the_plan_another_entry_made() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(
            &repo.join(".clank/plans/foo.md"),
            "# foo\n\nalready active\n",
        );
        write(
            &repo.join(".clank/queue/100-foo.md"),
            "# foo\n\nthe queued one\n",
        );
        let entry = entry_by_stem(repo, "100-foo").unwrap();
        let refused = promote_entry(repo, &entry).expect_err("the plan name is taken");
        assert!(refused.to_string().contains("already exists"), "{refused}");
        assert!(
            std::fs::read_to_string(repo.join(".clank/plans/foo.md"))
                .unwrap()
                .contains("already active"),
            "the active plan is untouched"
        );
        assert!(entry.path.exists(), "and the queued one stays queued");
    }

    /// The name-addressed verbs are the same work, reached the way the
    /// CLI reaches it: one implementation of what promoting means.
    #[test]
    fn the_named_verbs_go_through_the_entry_ones() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(".clank/queue/500-solo.md"), "# solo\n");
        let moved = set_priority(repo, "solo", 120).unwrap();
        assert_eq!(moved.file_name().unwrap(), "120-solo.md");
        let entry = entry_by_stem(repo, "120-solo").unwrap();
        assert_eq!(entry.priority, 120);
        let draft = unqueue(repo, "solo").unwrap();
        assert!(draft.ends_with(".clank/drafts/solo.md"));
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

    /// The inverse of `add`: the body goes back to the drafts dir and
    /// the queue entry is gone; an existing draft is never overwritten.
    #[test]
    fn unqueue_moves_the_entry_back_to_drafts_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(".clank/queue/500-foo.md"), "# foo\nthe body\n");
        let draft = unqueue(repo, "foo").unwrap();
        assert_eq!(draft, repo.join(".clank/drafts/foo.md"));
        assert_eq!(
            std::fs::read_to_string(&draft).unwrap(),
            "# foo\nthe body\n"
        );
        assert!(scan_queue(repo).is_empty(), "out of the queue");

        // Queue it again; a draft of the name still exists → refused,
        // nothing moved.
        write(&repo.join(".clank/queue/500-foo.md"), "# foo\nv2\n");
        let err = unqueue(repo, "foo").unwrap_err().to_string();
        assert!(err.contains("drafts/foo.md"), "names the file: {err}");
        assert_eq!(scan_queue(repo).len(), 1, "still queued");
        assert_eq!(
            std::fs::read_to_string(&draft).unwrap(),
            "# foo\nthe body\n"
        );

        // Unknown and ambiguous names refuse as every queue op does.
        assert!(unqueue(repo, "nope").is_err());
    }

    #[test]
    fn set_priority_renames_the_prefix_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("500-foo.md"), "# foo\n\nbody\n");

        // Rename to a new priority; body travels with the file.
        let dest = set_priority(dir.path(), "foo", 100).unwrap();
        assert_eq!(dest, q.join("100-foo.md"));
        assert!(dest.is_file());
        assert!(!q.join("500-foo.md").exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "# foo\n\nbody\n");

        // Same priority: a safe no-op returning the existing path.
        let dest = set_priority(dir.path(), "foo", 100).unwrap();
        assert_eq!(dest, q.join("100-foo.md"));
        assert!(dest.is_file());

        // Out-of-range and unknown names error clearly.
        let err = set_priority(dir.path(), "foo", 1000).unwrap_err();
        assert!(err.to_string().contains("0-999"), "{err}");
        let err = set_priority(dir.path(), "nope", 100).unwrap_err();
        assert!(err.to_string().contains("not in queue"), "{err}");
    }

    #[test]
    fn set_priority_refuses_ambiguous_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("100-foo.md"), "# foo\n");
        write(&q.join("200-foo.md"), "# foo\n");
        let err = set_priority(dir.path(), "foo", 300).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn scan_queue_accepts_every_md_with_lenient_priorities() {
        // lenient-queue-filenames: the strict NNN- shape is a writer
        // convention; the READER accepts every .md. Un-prefixed and
        // out-of-range stems are NAMES at the default 999 (end of the
        // queue); short digit prefixes parse (zero-padding optional).
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("100-good.md"), "ok\n");
        write(&q.join("bad.md"), "no prefix\n");
        write(&q.join("1000-overflow.md"), "4 digits\n");
        write(&q.join("12-short.md"), "2-digit prefix\n");
        write(&q.join("notes.txt"), "not md\n");
        let entries = scan_queue(dir.path());
        let got: Vec<(u16, &str)> = entries
            .iter()
            .map(|e| (e.priority, e.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (12, "short"),
                (100, "good"),
                (999, "1000-overflow"),
                (999, "bad"),
            ],
            "explicit priorities sort sooner; bare files at 999 tie-break by name"
        );
    }

    #[test]
    fn bare_and_prefixed_same_stem_is_a_duplicate_error() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("foo.md"), "bare\n");
        write(&q.join("500-foo.md"), "prefixed\n");
        let err = scan_queue_no_dups(dir.path()).expect_err("duplicate stems");
        let text = err.to_string();
        assert!(text.contains("foo"), "{text}");
        assert!(
            text.contains("foo.md") && text.contains("500-foo.md"),
            "names the ACTUAL files on disk: {text}"
        );
    }

    #[test]
    fn reprioritise_canonicalizes_a_bare_file() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("foo.md"), "bare\n");
        let dest = set_priority(dir.path(), "foo", 42).unwrap();
        assert!(dest.ends_with("042-foo.md"), "{dest:?}");
        assert!(!q.join("foo.md").exists(), "bare file renamed away");
        assert!(q.join("042-foo.md").exists());
    }

    fn inline(body: &str) -> BodySource {
        BodySource {
            raw: body.to_string(),
            kind: BodySourceKind::Inline,
        }
    }

    #[test]
    fn queue_add_leaves_the_allow_list_gitignore_byte_identical() {
        // Regression guard for the single-source plan (ruthless): the deleted
        // per-dir appender means `queue add` — like fork/open/pr-review — must
        // NOT touch `.clank/.gitignore` (an append would drift the
        // order-sensitive allow-list out of Canonical).
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".clank/.gitignore");
        write(&gi, &crate::init_facts::clank_gitignore_body());
        add_cmd(
            dir.path(),
            crate::cli::QueueAddArgs {
                name: "foo".into(),
                priority: 500,
                message: Some("do the thing".into()),
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&gi).unwrap(),
            crate::init_facts::clank_gitignore_body(),
            "queue add must leave the allow-list gitignore untouched"
        );
        assert!(dir.path().join(".clank/queue/500-foo.md").exists());
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
