//! A directory of priority-named markdown files, `NNN-<name>.md`, read
//! in `(priority, name)` order. The plan queue is one; so is each
//! agent's inbox.

use std::path::{Path, PathBuf};

/// The priority of a file WITHOUT a `NNN-` prefix: the end of the line,
/// so explicit priorities always sort sooner (lenient-queue-filenames).
pub const DEFAULT_PRIORITY: u16 = 999;

/// The highest priority a writer may give.
pub const MAX_PRIORITY: u16 = 999;

#[derive(Clone, Debug)]
pub struct Entry {
    pub priority: u16,
    pub name: String,
    pub path: PathBuf,
}

impl Entry {
    /// The file's stem — `500-foo`, `foo`, `001-foo` — the one handle
    /// that is unique per entry. The parse is lenient, so two files
    /// can share a name and even a priority; anything keyed by those
    /// collides. Pages, links and targets that must mean ONE entry
    /// use this (a-queued-plan-has-a-page).
    pub fn file_stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.name.clone())
    }
}

/// The file name a writer gives `name` at `priority`.
pub fn file_name(priority: u16, name: &str) -> String {
    format!("{priority:03}-{name}.md")
}

/// Refuse a priority no file name can carry.
pub fn check_priority(priority: u16) -> anyhow::Result<()> {
    if priority > MAX_PRIORITY {
        anyhow::bail!("priority must be 0-{MAX_PRIORITY}");
    }
    Ok(())
}

/// Parse a file stem into `(priority, name)`. Lenient by design
/// (lenient-queue-filenames): the strict `NNN-name` shape is a WRITER
/// convention, not a reader contract — a hand-dropped `foo.md` must
/// not be silently invisible. Split at the FIRST `-`: 1–3 ASCII
/// digits on the left = an explicit priority (zero-padding optional,
/// so `12-foo` works); anything else — no dash, non-digits (`2fa-x`),
/// out-of-range digits (`1234-x`) — is a NAME at [`DEFAULT_PRIORITY`].
/// Total: every stem parses.
pub fn parse_stem(stem: &str) -> (u16, String) {
    if let Some((left, rest)) = stem.split_once('-')
        && (1..=3).contains(&left.len())
        && left.bytes().all(|b| b.is_ascii_digit())
        && !rest.is_empty()
        && let Ok(priority) = left.parse::<u16>()
    {
        return (priority, rest.to_string());
    }
    (DEFAULT_PRIORITY, stem.to_string())
}

/// Every `*.md` in `dir`, in the order they are read. A missing
/// directory is an empty one.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(s) = fname.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".md") else {
            continue;
        };
        let (priority, name) = parse_stem(stem);
        out.push(Entry {
            priority,
            name,
            path: entry.path(),
        });
    }
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    out
}

/// What a lookup found. The caller words each case: the queue and an
/// inbox say "not found" differently.
pub enum Lookup<'a> {
    One(&'a Entry),
    None,
    Many(Vec<&'a Entry>),
}

/// The entries whose NAME is `name`.
pub fn by_name<'a>(entries: &'a [Entry], name: &str) -> Lookup<'a> {
    let matches: Vec<&Entry> = entries.iter().filter(|e| e.name == name).collect();
    match matches.as_slice() {
        [] => Lookup::None,
        [one] => Lookup::One(one),
        _ => Lookup::Many(matches),
    }
}

/// The entry `handle` means: its file stem exactly, else its NAME.
pub fn by_handle<'a>(entries: &'a [Entry], handle: &str) -> Lookup<'a> {
    match entries.iter().find(|e| e.file_stem() == handle) {
        Some(e) => Lookup::One(e),
        None => by_name(entries, handle),
    }
}

/// Move `entry` to `priority` by renaming its `NNN-` prefix within
/// `dir`. The same priority is a no-op. Returns the entry's (possibly
/// new) path.
///
/// Refuses a destination that is taken rather than replacing it: two
/// files can share a name at different priorities, and a plain rename
/// onto the other's name deletes it.
pub fn set_priority(dir: &Path, entry: &Entry, priority: u16) -> anyhow::Result<PathBuf> {
    check_priority(priority)?;
    if entry.priority == priority {
        return Ok(entry.path.clone());
    }
    let dest = dir.join(file_name(priority, &entry.name));
    match rename_noreplace(&entry.path, &dest) {
        Ok(()) => Ok(dest),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
            "`{}` already exists; not replacing it with `{}`",
            dest.display(),
            entry.path.display()
        ),
        Err(e) => Err(anyhow::anyhow!("renaming `{}`: {e}", entry.path.display())),
    }
}

/// Rename `from` to `to`, refusing atomically when `to` exists — also
/// when it appears between any check and the rename.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let cstr = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path holds a NUL"))
    };
    let (from_c, to_c) = (cstr(from)?, cstr(to)?);
    match rename_excl(&from_c, &to_c) {
        Some(0) => Ok(()),
        Some(_) => {
            let e = std::io::Error::last_os_error();
            // A filesystem without the flag says so; a link refuses a
            // taken name just as surely, and the unlink after it only
            // removes the old name.
            match e.raw_os_error() {
                Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => link_then_unlink(from, to),
                _ => Err(e),
            }
        }
        None => link_then_unlink(from, to),
    }
}

fn link_then_unlink(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

#[cfg(target_vendor = "apple")]
fn rename_excl(from: &std::ffi::CStr, to: &std::ffi::CStr) -> Option<libc::c_int> {
    // SAFETY: both are valid NUL-terminated paths for the call's duration.
    Some(unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) })
}

#[cfg(target_os = "linux")]
fn rename_excl(from: &std::ffi::CStr, to: &std::ffi::CStr) -> Option<libc::c_int> {
    // SAFETY: both are valid NUL-terminated paths for the call's duration.
    Some(unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    })
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn rename_excl(_: &std::ffi::CStr, _: &std::ffi::CStr) -> Option<libc::c_int> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stem_edges() {
        assert_eq!(parse_stem("500-foo"), (500, "foo".into()));
        assert_eq!(parse_stem("012-foo"), (12, "foo".into()));
        assert_eq!(parse_stem("0-foo"), (0, "foo".into()));
        assert_eq!(parse_stem("foo"), (999, "foo".into()));
        // Not all digits before the first dash → a name.
        assert_eq!(parse_stem("2fa-support"), (999, "2fa-support".into()));
        // Four digits exceed 0-999 → a name, not a bad priority.
        assert_eq!(parse_stem("1234-foo"), (999, "1234-foo".into()));
        // A dash with nothing after it is a name, not an empty stem.
        assert_eq!(parse_stem("123-"), (999, "123-".into()));
    }

    #[test]
    fn a_handle_is_a_stem_first_then_a_unique_name() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["500-foo.md", "400-bar.md", "999-bar.md"] {
            std::fs::write(dir.path().join(f), "x").unwrap();
        }
        let entries = scan(dir.path());
        assert!(matches!(by_handle(&entries, "500-foo"), Lookup::One(e) if e.name == "foo"));
        assert!(matches!(by_handle(&entries, "foo"), Lookup::One(e) if e.priority == 500));
        assert!(matches!(by_handle(&entries, "999-bar"), Lookup::One(e) if e.priority == 999));
        assert!(matches!(by_handle(&entries, "bar"), Lookup::Many(m) if m.len() == 2));
        assert!(matches!(by_handle(&entries, "baz"), Lookup::None));
    }

    #[test]
    fn reprioritising_onto_a_taken_name_refuses_and_keeps_both() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("100-note.md"), "first").unwrap();
        std::fs::write(dir.path().join("500-note.md"), "second").unwrap();
        let entries = scan(dir.path());
        let Lookup::One(second) = by_handle(&entries, "500-note") else {
            panic!("the stem names one file");
        };

        let err = set_priority(dir.path(), second, 100).unwrap_err();

        assert!(err.to_string().contains("already exists"), "{err}");
        let read = |f: &str| std::fs::read_to_string(dir.path().join(f)).unwrap();
        assert_eq!(
            (read("100-note.md"), read("500-note.md")),
            ("first".into(), "second".into())
        );
    }
}
