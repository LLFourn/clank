//! What a native file notification says about the files it names, as
//! every watcher's owner acts on it.
//!
//! A backend reports reads as well as writes: Linux's inotify backend
//! subscribes to opens, so a watcher's owner reading the files it watches
//! would otherwise wake itself, and do the work that wake asks for, for
//! as long as it runs.

use std::path::PathBuf;

use notify::event::{AccessKind, AccessMode, EventKind};

/// One notification.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Notice<'a> {
    /// These paths were created, written, renamed or removed.
    Changed(&'a [PathBuf]),
    /// The backend says to look again: changes may have been missed
    /// anywhere it watches.
    Rescan,
    /// Something was read, or opened or closed without being written.
    Read,
    /// The watch failed. What it would have reported may be missing from
    /// now on, so its owner stops or recovers, saying so.
    Failed(String),
}

pub(crate) fn notice(res: &notify::Result<notify::Event>) -> Notice<'_> {
    let event = match res {
        Ok(event) => event,
        Err(e) => return Notice::Failed(e.to_string()),
    };
    if event.need_rescan() {
        return Notice::Rescan;
    }
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
        EventKind::Access(_) => return Notice::Read,
        _ => {}
    }
    if event.paths.is_empty() {
        return Notice::Rescan;
    }
    Notice::Changed(&event.paths)
}

/// Where a watcher says something changed: a channel, and what it says
/// there, so one owner can hear many watchers on one channel and tell
/// them apart.
pub(crate) struct Wake<M = ()> {
    tx: std::sync::mpsc::Sender<M>,
    says: M,
}

impl<M: Clone> Clone for Wake<M> {
    fn clone(&self) -> Self {
        Wake {
            tx: self.tx.clone(),
            says: self.says.clone(),
        }
    }
}

impl<M: Clone> Wake<M> {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<M>, says: M) -> Self {
        Wake { tx, says }
    }

    /// Say it. False once nobody listens.
    pub(crate) fn ring(&self) -> bool {
        self.tx.send(self.says.clone()).is_ok()
    }
}

impl From<std::sync::mpsc::Sender<()>> for Wake {
    fn from(tx: std::sync::mpsc::Sender<()>) -> Self {
        Wake::new(tx, ())
    }
}

/// Where a watcher records that it failed, for its owner to read.
pub(crate) type Fault = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// What `fault` holds, if the watcher failed.
pub(crate) fn read(fault: &Fault) -> Option<String> {
    fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Record that the watcher failed, keeping the first reason.
pub(crate) fn record(fault: &Fault, why: String) {
    fault
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert(why);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };

    /// `kind` reported for `path`, as a backend delivers it.
    pub(crate) fn event(kind: EventKind, path: &std::path::Path) -> notify::Result<notify::Event> {
        Ok(notify::Event::new(kind).add_path(path.to_path_buf()))
    }

    /// An open, as Linux's inotify backend reports one.
    pub(crate) fn opened(path: &std::path::Path) -> notify::Result<notify::Event> {
        event(EventKind::Access(AccessKind::Open(AccessMode::Any)), path)
    }

    #[test]
    fn reads_are_not_changes_and_writes_renames_and_removals_are() {
        let path = std::path::Path::new("/r/.git/HEAD");
        for read in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Access(AccessKind::Any),
        ] {
            assert_eq!(notice(&event(read, path)), Notice::Read, "{read:?}");
        }
        for change in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Any,
        ] {
            assert!(
                matches!(notice(&event(change, path)), Notice::Changed(_)),
                "{change:?}"
            );
        }
        let rescan = Ok(notify::Event::new(EventKind::Any)
            .add_path(path.to_path_buf())
            .set_flag(Flag::Rescan));
        assert_eq!(notice(&rescan), Notice::Rescan);
        assert_eq!(
            notice(&Ok(notify::Event::new(EventKind::Any))),
            Notice::Rescan
        );
        assert!(matches!(
            notice(&Err(notify::Error::generic("gone"))),
            Notice::Failed(why) if why.contains("gone")
        ));
    }
}
