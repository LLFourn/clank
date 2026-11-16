//! The web host's watch on its browser sessions store, from the kernel.

use std::path::Path;

use clank_web_cloudflare::process::StoreEvent;

/// [`clank_web_cloudflare::process::StoreWatch`] through
/// [`crate::proc_identity::tree_watch`]: the store's directory for a
/// write that renames over the file, and the file for one in place.
pub struct KernelStoreWatch;

/// What one notice says to the host about `store`: nothing for another
/// file's write or a read, and [`StoreEvent::Lost`] for a failure, after
/// which the watcher reports nothing more.
fn heard(notice: &crate::fs_notice::Notice<'_>, store: &Path) -> Option<StoreEvent> {
    use crate::fs_notice::Notice;
    match notice {
        Notice::Changed(paths) if !paths.iter().any(|p| p == store) => None,
        Notice::Read => None,
        Notice::Failed(why) => Some(StoreEvent::Lost(why.clone())),
        Notice::Changed(_) | Notice::Rescan => Some(StoreEvent::Changed),
    }
}

impl clank_web_cloudflare::process::StoreWatch for KernelStoreWatch {
    fn watch(
        &self,
        file: &Path,
        heard_by: Box<dyn Fn(StoreEvent) + Send + Sync>,
    ) -> anyhow::Result<Box<dyn Send>> {
        use notify::Watcher as _;
        let dir = file
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no directory", file.display()))?
            .to_path_buf();
        let (target, content) = (file.to_path_buf(), file.to_path_buf());
        let mut watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
            move |f| f == content,
            move |res: notify::Result<notify::Event>| {
                if let Some(event) = heard(&crate::fs_notice::notice(&res), &target) {
                    heard_by(event)
                }
            },
        )?;
        watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;
        Ok(Box::new(watcher))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_web_cloudflare::process::StoreWatch as _;
    use std::sync::mpsc;
    use std::time::Duration;

    fn watched(file: &Path) -> (Box<dyn Send>, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let guard = KernelStoreWatch
            .watch(
                file,
                Box::new(move |_| {
                    let _ = tx.lock().unwrap().send(());
                }),
            )
            .unwrap();
        (guard, rx)
    }

    /// The store is written by renaming a temporary file over it, as
    /// `door::write_typed_config` does.
    fn replace(file: &Path, body: &str) {
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, body).unwrap();
        std::fs::rename(&tmp, file).unwrap();
    }

    #[test]
    fn a_write_to_the_store_is_heard() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("remote-sessions.json");
        std::fs::write(&file, "{}").unwrap();
        let (_guard, rx) = watched(&file);
        replace(&file, r#"{"sessions":[]}"#);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a replaced store is heard");
    }

    #[test]
    fn another_file_beside_the_store_is_not_heard() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("remote-sessions.json");
        std::fs::write(&file, "{}").unwrap();
        let (_guard, rx) = watched(&file);
        std::fs::write(home.path().join("config.json"), "{}").unwrap();
        replace(&file, "{}");
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "only the store's own write rang"
        );
    }

    #[test]
    fn a_failed_watch_is_lost_and_only_the_stores_own_write_is_a_change() {
        use crate::fs_notice::Notice;
        let store = Path::new("/h/.clank/remote-sessions.json");
        let other = [std::path::PathBuf::from("/h/.clank/config.json")];
        let ours = [store.to_path_buf()];
        assert_eq!(
            heard(&Notice::Failed("gone".into()), store),
            Some(StoreEvent::Lost("gone".into()))
        );
        assert_eq!(
            heard(&Notice::Changed(&ours), store),
            Some(StoreEvent::Changed)
        );
        assert_eq!(heard(&Notice::Rescan, store), Some(StoreEvent::Changed));
        assert_eq!(heard(&Notice::Changed(&other), store), None);
        assert_eq!(heard(&Notice::Read, store), None);
    }
}
