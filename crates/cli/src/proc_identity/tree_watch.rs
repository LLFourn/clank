//! Changes to a few directories and files, from the kernel: kqueue on
//! macOS, inotify on Linux.
//!
//! For the small fixed sets clank watches — its own state, a git
//! directory's refs, a log, a transcript — where a platform event stream
//! (FSEvents) costs seconds to set up and loads a daemon every process on
//! the machine shares. Each watched directory and file holds one kernel
//! registration, made as it is found and dropped as it goes.
//!
//! A directory is watched for its entries, and a file for its writes as
//! well when the owner says its content matters: git replaces HEAD, the
//! index and refs by renaming over them, which is an entry change, while
//! a log or a transcript is appended to. Neither registration reports a
//! read or an open. A directory's change is turned into the entries that
//! changed by comparing its listing, so the owner hears paths, as it
//! would from any backend.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::exit_watch::platform::{Event, Sys};

/// Which file this is: a file replaced by renaming over it is another.
type Identity = (u64, u64, bool);

fn identity(path: &Path) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path)?;
    Ok((md.dev(), md.ino(), md.is_dir()))
}

enum Kind {
    Dir {
        recursive: bool,
        entries: BTreeMap<OsString, (Identity, Option<u64>)>,
    },
    File,
    /// The nearest existing ancestor of a root that does not exist yet.
    Waiting {
        root: PathBuf,
        recursive: bool,
    },
}

struct Node {
    path: PathBuf,
    /// What its registration is on: a path renamed over, or removed and
    /// made again, is another file or directory, which the registration
    /// does not see.
    id: Option<Identity>,
    root: bool,
    kind: Kind,
}

type Content = Box<dyn Fn(&Path) -> bool + Send>;

struct Tree {
    sys: Arc<Sys>,
    next: u64,
    nodes: HashMap<u64, Node>,
    content: Content,
    /// What a test does between looking for a root's nearest directory
    /// and registering it: what the world could do in that gap.
    #[cfg(test)]
    gap: Option<Box<dyn FnOnce() + Send>>,
}

/// Missing is not a failure: what vanished between a listing and its
/// registration is reported by its directory.
fn vanished(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

impl Tree {
    fn register(&mut self, path: &Path, content: bool) -> io::Result<u64> {
        self.next += 1;
        let reg = self.next;
        self.sys.register_path(reg, path, content)?;
        Ok(reg)
    }

    /// Whether the root exists, and is watched itself rather than waited
    /// for.
    fn add_root(&mut self, path: &Path, recursive: bool) -> io::Result<bool> {
        loop {
            let added = match identity(path) {
                Ok((_, _, true)) => self.add_dir(path, recursive, true),
                Ok(_) => self.add_file(path, true),
                Err(e) => Err(e),
            };
            match added {
                Ok(_) => return Ok(true),
                Err(e) if vanished(&e) => {}
                Err(e) => return Err(e),
            }
            let ancestor = nearest_dir(path);
            #[cfg(test)]
            if let Some(gap) = self.gap.take() {
                gap();
            }
            let reg = match self.register(&ancestor, false) {
                Ok(reg) => reg,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e),
            };
            // Looked at again once registered: a level made between the
            // look and the registration would otherwise never be heard.
            if path.symlink_metadata().is_ok() || nearest_dir(path) != ancestor {
                self.sys.deregister_path(reg);
                continue;
            }
            self.nodes.insert(
                reg,
                Node {
                    path: ancestor,
                    id: None,
                    root: true,
                    kind: Kind::Waiting {
                        root: path.to_path_buf(),
                        recursive,
                    },
                },
            );
            return Ok(false);
        }
    }

    fn add_dir(&mut self, path: &Path, recursive: bool, root: bool) -> io::Result<u64> {
        // Read before registering: a directory put in its place between
        // the two is then a mismatch on its first change, not a loss.
        let id = identity(path)?;
        let reg = self.register(path, false)?;
        let listed = list(path).inspect_err(|_| self.sys.deregister_path(reg))?;
        let mut entries = BTreeMap::new();
        for (name, id) in listed {
            let child = self.add_child(&path.join(&name), id, recursive)?;
            entries.insert(name, (id, child));
        }
        self.nodes.insert(
            reg,
            Node {
                path: path.to_path_buf(),
                id: Some(id),
                root,
                kind: Kind::Dir { recursive, entries },
            },
        );
        Ok(reg)
    }

    fn add_file(&mut self, path: &Path, root: bool) -> io::Result<u64> {
        let id = identity(path)?;
        let reg = self.register(path, true)?;
        self.nodes.insert(
            reg,
            Node {
                path: path.to_path_buf(),
                id: Some(id),
                root,
                kind: Kind::File,
            },
        );
        Ok(reg)
    }

    /// A directory's entry, registered when it is a directory under a
    /// recursive watch or a file whose content matters.
    fn add_child(&mut self, path: &Path, id: Identity, recursive: bool) -> io::Result<Option<u64>> {
        let added = match (id.2, recursive) {
            (true, true) => self.add_dir(path, true, false),
            (false, _) if (self.content)(path) => self.add_file(path, false),
            _ => return Ok(None),
        };
        match added {
            Ok(reg) => Ok(Some(reg)),
            Err(e) if vanished(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn remove(&mut self, reg: u64) {
        self.sys.deregister_path(reg);
        if let Some(Node {
            kind: Kind::Dir { entries, .. },
            ..
        }) = self.nodes.remove(&reg)
        {
            for (_, child) in entries.into_values() {
                if let Some(child) = child {
                    self.remove(child);
                }
            }
        }
    }

    fn changed(&mut self, reg: u64) -> io::Result<Vec<PathBuf>> {
        let Some(node) = self.nodes.get(&reg) else {
            return Ok(Vec::new());
        };
        let (path, root) = (node.path.clone(), node.root);
        if let Some(id) = node.id
            && identity(&path).ok() != Some(id)
        {
            // Replaced or removed: a parent's listing registers what is
            // there now; a root has no parent watching it, so it is
            // registered again here, or watched for.
            let recursive = matches!(
                node.kind,
                Kind::Dir {
                    recursive: true,
                    ..
                }
            );
            self.remove(reg);
            if root {
                self.add_root(&path, recursive)?;
            }
            return Ok(vec![path]);
        }
        match &node.kind {
            Kind::Waiting {
                root: target,
                recursive,
            } => {
                let (target, recursive) = (target.clone(), *recursive);
                if target.symlink_metadata().is_err() && nearest_dir(&target) == path {
                    return Ok(Vec::new());
                }
                self.remove(reg);
                Ok(if self.add_root(&target, recursive)? {
                    vec![target]
                } else {
                    Vec::new()
                })
            }
            Kind::File => Ok(vec![path]),
            Kind::Dir { recursive, .. } => {
                let recursive = *recursive;
                let listed = match list(&path) {
                    Ok(listed) => listed,
                    Err(e) if vanished(&e) => {
                        self.remove(reg);
                        if root {
                            self.add_root(&path, recursive)?;
                        }
                        return Ok(vec![path]);
                    }
                    Err(e) => return Err(e),
                };
                let Some(Node {
                    kind: Kind::Dir { entries, .. },
                    ..
                }) = self.nodes.get_mut(&reg)
                else {
                    return Ok(Vec::new());
                };
                let old = std::mem::take(entries);
                let mut kept = BTreeMap::new();
                let mut changed = Vec::new();
                let mut stale = Vec::new();
                for (name, (id, child)) in &old {
                    match listed.get(name) {
                        Some(now) if now == id => {
                            kept.insert(name.clone(), (*id, *child));
                        }
                        _ => {
                            changed.push(path.join(name));
                            stale.extend(*child);
                        }
                    }
                }
                for child in stale {
                    self.remove(child);
                }
                for (name, id) in listed {
                    if kept.contains_key(&name) {
                        continue;
                    }
                    let at = path.join(&name);
                    if !old.contains_key(&name) {
                        changed.push(at.clone());
                    }
                    let child = self.add_child(&at, id, recursive)?;
                    kept.insert(name, (id, child));
                }
                if let Some(Node {
                    kind: Kind::Dir { entries, .. },
                    ..
                }) = self.nodes.get_mut(&reg)
                {
                    *entries = kept;
                }
                Ok(changed)
            }
        }
    }

    /// What `events` say to the owner: the paths that changed, or, when
    /// events were dropped, that it must look again everywhere. A loss
    /// may hide a directory made or replaced, so every root is
    /// registered afresh first.
    fn heard(&mut self, events: Vec<Event>) -> io::Result<Option<notify::Event>> {
        if events.iter().any(|event| matches!(event, Event::Lost)) {
            self.rebuild()?;
            return Ok(Some(
                notify::Event::new(notify::EventKind::Any).set_flag(notify::event::Flag::Rescan),
            ));
        }
        let mut paths = Vec::new();
        for event in events {
            if let Event::Path(reg) = event {
                paths.extend(self.changed(reg)?);
            }
        }
        paths.sort();
        paths.dedup();
        Ok((!paths.is_empty()).then(|| {
            paths.into_iter().fold(
                notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any)),
                |event, path| event.add_path(path),
            )
        }))
    }

    fn rebuild(&mut self) -> io::Result<()> {
        let roots: Vec<(u64, PathBuf, bool)> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.root)
            .map(|(reg, node)| match &node.kind {
                Kind::Dir { recursive, .. } => (*reg, node.path.clone(), *recursive),
                Kind::File => (*reg, node.path.clone(), false),
                Kind::Waiting { root, recursive } => (*reg, root.clone(), *recursive),
            })
            .collect();
        for (reg, _, _) in &roots {
            self.remove(*reg);
        }
        for (_, path, recursive) in roots {
            self.add_root(&path, recursive)?;
        }
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) {
        let roots: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.root
                    && (node.path == path
                        || matches!(&node.kind, Kind::Waiting { root, .. } if root == path))
            })
            .map(|(reg, _)| *reg)
            .collect();
        for reg in roots {
            self.remove(reg);
        }
    }
}

/// The nearest directory above `path` that exists.
fn nearest_dir(path: &Path) -> PathBuf {
    path.ancestors()
        .skip(1)
        .find(|dir| dir.is_dir())
        .unwrap_or(Path::new("/"))
        .to_path_buf()
}

/// A directory's entries and what each is.
fn list(dir: &Path) -> io::Result<BTreeMap<OsString, Identity>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Ok(id) = identity(&entry.path()) {
            out.insert(entry.file_name(), id);
        }
    }
    Ok(out)
}

/// A watcher on clank's kernel registrations, for owners written against
/// [`notify::Watcher`]: changed paths arrive as one event per batch, and a
/// failure once, as an error, after which nothing more is reported.
pub struct KernelWatcher {
    tree: Arc<Mutex<Tree>>,
    sys: Arc<Sys>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KernelWatcher {
    /// Starts the watch thread. `content` says which files' writes matter
    /// beyond their directories' entries.
    pub fn new(
        content: impl Fn(&Path) -> bool + Send + 'static,
        mut handler: impl FnMut(notify::Result<notify::Event>) + Send + 'static,
    ) -> io::Result<Self> {
        let sys = Arc::new(Sys::new(None)?);
        let tree = Arc::new(Mutex::new(Tree {
            sys: sys.clone(),
            next: 0,
            nodes: HashMap::new(),
            content: Box::new(content),
            #[cfg(test)]
            gap: None,
        }));
        let thread = std::thread::Builder::new()
            .name("tree-watch".into())
            .spawn({
                let (sys, tree) = (sys.clone(), tree.clone());
                move || {
                    loop {
                        let events = match sys.wait() {
                            Ok(Some(events)) => events,
                            Ok(None) => return,
                            Err(e) => return handler(Err(notify::Error::io(e))),
                        };
                        let heard = tree
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .heard(events);
                        match heard {
                            Err(e) => return handler(Err(notify::Error::io(e))),
                            Ok(event) => {
                                if let Some(event) = event {
                                    handler(Ok(event));
                                }
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            tree,
            sys,
            thread: Some(thread),
        })
    }

    fn tree(&self) -> std::sync::MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stand in for the kernel wait failing, as a test drives an owner.
    #[cfg(test)]
    pub fn fail_for_test(&self) {
        self.sys.fail();
    }
}

impl notify::Watcher for KernelWatcher {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        Err(notify::Error::generic(
            "a kernel watcher is made by KernelWatcher::new",
        ))
    }

    /// A root that does not exist yet is watched for, through its nearest
    /// existing ancestor, and reported once it appears.
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> notify::Result<()> {
        self.tree()
            .add_root(path, mode == notify::RecursiveMode::Recursive)
            .map(drop)
            .map_err(notify::Error::io)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.tree().unwatch(path);
        Ok(())
    }

    fn kind() -> notify::WatcherKind {
        if cfg!(target_os = "linux") {
            notify::WatcherKind::Inotify
        } else {
            notify::WatcherKind::Kqueue
        }
    }
}

impl Drop for KernelWatcher {
    fn drop(&mut self) {
        self.sys.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::{RecursiveMode, Watcher as _};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    type Heard = Receiver<notify::Result<notify::Event>>;

    fn watcher(content: impl Fn(&Path) -> bool + Send + 'static) -> (KernelWatcher, Heard) {
        let (tx, rx) = channel();
        let watch = KernelWatcher::new(content, move |res| {
            let _ = tx.send(res);
        })
        .unwrap();
        (watch, rx)
    }

    fn canonical(dir: &tempfile::TempDir) -> PathBuf {
        dunce::canonicalize(dir.path()).unwrap()
    }

    /// Whether `path` is reported within a few seconds.
    fn hears(rx: &Heard, path: &Path) -> bool {
        let by = std::time::Instant::now() + Duration::from_secs(5);
        while let Some(left) = by.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Ok(event)) if event.paths.iter().any(|p| p == path) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    fn quiet(rx: &Heard) -> bool {
        rx.recv_timeout(Duration::from_millis(300)).is_err()
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn rename_over(path: &Path, text: &str) {
        let tmp = path.with_extension("tmp-new");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn an_append_to_a_file_whose_content_matters_is_heard_and_a_read_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let log = root.join("events.jsonl");
        let other = root.join("other.txt");
        std::fs::write(&log, "one\n").unwrap();
        std::fs::write(&other, "x").unwrap();
        let (mut watch, rx) = watcher(|p| p.extension().is_some_and(|e| e == "jsonl"));
        watch.watch(&root, RecursiveMode::NonRecursive).unwrap();

        let _ = std::fs::read(&log).unwrap();
        assert!(quiet(&rx), "a read was reported");
        append(&log, "two\n");
        assert!(hears(&rx, &log), "an append was not heard");
        append(&other, "y");
        assert!(quiet(&rx), "a file whose content does not matter was");
    }

    #[test]
    fn a_rename_over_a_file_is_heard_and_the_new_file_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let log = root.join("t.jsonl");
        std::fs::write(&log, "one\n").unwrap();
        let (mut watch, rx) = watcher(|_| true);
        watch.watch(&root, RecursiveMode::NonRecursive).unwrap();

        rename_over(&log, "two\n");
        assert!(hears(&rx, &log), "the rename over it was not heard");
        while !quiet(&rx) {}
        append(&log, "three\n");
        assert!(hears(&rx, &log), "the file renamed in is not followed");
    }

    #[test]
    fn a_file_root_is_followed_across_a_rename_over_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let file = root.join("transcript.jsonl");
        std::fs::write(&file, "one\n").unwrap();
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&file, RecursiveMode::NonRecursive).unwrap();

        append(&file, "two\n");
        assert!(hears(&rx, &file), "an append to a file root was not heard");
        rename_over(&file, "three\n");
        assert!(hears(&rx, &file), "the rename over it was not heard");
        while !quiet(&rx) {}
        append(&file, "four\n");
        assert!(hears(&rx, &file), "the replacement is not followed");
    }

    #[test]
    fn a_directory_made_later_under_a_recursive_root_is_watched() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&root, RecursiveMode::Recursive).unwrap();

        let deeper = root.join("heads/new/deeper");
        std::fs::create_dir_all(&deeper).unwrap();
        assert!(hears(&rx, &root.join("heads")));
        while !quiet(&rx) {}
        let reference = deeper.join("branch");
        rename_over(&reference, "abc\n");
        assert!(hears(&rx, &reference), "an entry in the new directory");
    }

    #[test]
    fn a_root_made_later_is_watched_through_its_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let missing = root.join("a/b/refs");
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&missing, RecursiveMode::Recursive).unwrap();

        std::fs::create_dir_all(missing.join("heads")).unwrap();
        assert!(hears(&rx, &missing), "the root appearing");
        while !quiet(&rx) {}
        let reference = missing.join("heads/main");
        rename_over(&reference, "abc\n");
        assert!(hears(&rx, &reference), "an entry under the new root");
    }

    /// A root made between the look for its nearest directory and that
    /// directory's registration is watched, not waited for forever.
    #[test]
    fn a_root_made_while_its_ancestor_is_registered_is_watched() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let missing = root.join("a/refs");
        let (mut watch, rx) = watcher(|_| false);
        let made = missing.clone();
        watch.tree().gap = Some(Box::new(move || std::fs::create_dir_all(&made).unwrap()));
        watch.watch(&missing, RecursiveMode::Recursive).unwrap();

        let reference = missing.join("main");
        rename_over(&reference, "abc\n");
        assert!(
            hears(&rx, &reference),
            "an entry under the root made in the gap"
        );
    }

    /// The watched directory renamed aside and made again at its path,
    /// empty or not: what is written in the new one is heard.
    #[test]
    fn a_root_directory_replaced_at_its_path_is_followed() {
        for populated in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let base = canonical(&dir);
            let root = base.join("refs");
            std::fs::create_dir(&root).unwrap();
            let (mut watch, rx) = watcher(|_| false);
            watch.watch(&root, RecursiveMode::Recursive).unwrap();

            let fresh = base.join("fresh");
            std::fs::create_dir(&fresh).unwrap();
            if populated {
                std::fs::create_dir(fresh.join("heads")).unwrap();
                std::fs::write(fresh.join("heads/main"), "abc\n").unwrap();
            }
            {
                // Both renames land before the watch looks at either.
                let _held = watch.tree();
                std::fs::rename(&root, base.join("aside")).unwrap();
                std::fs::rename(&fresh, &root).unwrap();
            }
            assert!(
                hears(&rx, &root),
                "the replacement (populated: {populated})"
            );
            while !quiet(&rx) {}

            let later = root.join("later.jsonl");
            std::fs::write(&later, "").unwrap();
            assert!(
                hears(&rx, &later),
                "an entry made later (populated: {populated})"
            );
            if populated {
                let deeper = root.join("heads/topic");
                std::fs::write(&deeper, "").unwrap();
                assert!(hears(&rx, &deeper), "an entry in its subdirectory");
            }
        }
    }

    /// The same for a directory under a recursive root.
    #[test]
    fn a_subdirectory_replaced_at_its_path_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let sub = root.join("heads");
        std::fs::create_dir(&sub).unwrap();
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&root, RecursiveMode::Recursive).unwrap();

        let fresh = root.join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        {
            let _held = watch.tree();
            std::fs::rename(&sub, root.join("aside")).unwrap();
            std::fs::rename(&fresh, &sub).unwrap();
        }
        assert!(hears(&rx, &sub));
        while !quiet(&rx) {}
        let later = sub.join("main");
        std::fs::write(&later, "").unwrap();
        assert!(hears(&rx, &later), "an entry in the replacement");
    }

    /// Dropped events tell the owner to look again everywhere, through
    /// the same notice every owner already treats conservatively, and
    /// the watch goes on hearing changes afterwards.
    #[test]
    fn dropped_events_are_a_rescan_and_the_watch_survives_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        std::fs::create_dir(root.join("heads")).unwrap();
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&root, RecursiveMode::Recursive).unwrap();

        let told = watch.tree().heard(vec![Event::Lost]).unwrap();
        assert_eq!(
            crate::fs_notice::notice(&Ok(told.expect("the owner is told"))),
            crate::fs_notice::Notice::Rescan
        );
        let later = root.join("heads/main");
        std::fs::write(&later, "").unwrap();
        assert!(hears(&rx, &later), "a change after the loss");
    }

    #[test]
    fn a_removed_entry_is_heard_and_unwatch_stops_reports() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let gone = root.join("gone");
        std::fs::write(&gone, "").unwrap();
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&root, RecursiveMode::NonRecursive).unwrap();

        std::fs::remove_file(&gone).unwrap();
        assert!(hears(&rx, &gone));
        watch.unwatch(&root).unwrap();
        std::fs::write(root.join("later"), "").unwrap();
        assert!(quiet(&rx), "an unwatched root reported");
    }

    #[test]
    fn a_failed_wait_reaches_the_owner_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(&dir);
        let (mut watch, rx) = watcher(|_| false);
        watch.watch(&root, RecursiveMode::NonRecursive).unwrap();
        watch.fail_for_test();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Err(_))
        ));
        std::fs::write(root.join("after"), "").unwrap();
        assert!(quiet(&rx), "a failed watch reported again");
    }
}
