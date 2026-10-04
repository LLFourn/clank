//! Workspace trust is a launch decision, separate from tool permissions.
//!
//! Claude documents projects[repo-root].hasTrustDialogAccepted in .claude.json:
//! https://code.claude.com/docs/en/permissions#project-allow-rules-and-workspace-trust
//! With CLAUDE_CONFIG_DIR, that file lives inside the selected profile; otherwise
//! it lives directly under HOME. Linked worktrees share the main checkout's key.

use anyhow::Context;
use std::path::Path;

pub(super) async fn prepare(config_home: &Path, cwd: &Path) -> anyhow::Result<()> {
    let config_home = config_home.to_owned();
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        let root = dunce::canonicalize(super::fork::main_repo_root(&cwd)?)?;
        ensure(&config_home, &root).with_context(|| {
            format!(
                "cannot prepare Claude workspace trust for {} in {}; check this profile's permissions and retry `clank agent start`",
                cwd.display(), config_home.join(".claude.json").display()
            )
        })
    }).await?
}

fn ensure(config_home: &Path, root: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let path = config_home.join(".claude.json");
    // Clank launches serialize even before the native file exists. Resolve a
    // config symlink too, so aliases do not acquire different Clank locks.
    let target = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => dunce::canonicalize(&path)?,
        Ok(_) => path.clone(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.clone(),
        Err(error) => return Err(error.into()),
    };
    let read = || -> anyhow::Result<serde_json::Value> {
        match std::fs::read(&target) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "parsing {}; existing configuration was not changed",
                    path.display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
            Err(error) => Err(error.into()),
        }
    };
    let key = root
        .to_str()
        .context("Claude workspace path is not UTF-8")?;
    if read()?["projects"][key]["hasTrustDialogAccepted"] == true {
        return Ok(());
    }
    // A healthy launch is read-only: no lock file or directory needs creating.
    // For a change, serialize and re-read under the native transaction below.
    std::fs::create_dir_all(config_home)?;
    let parent = dunce::canonicalize(target.parent().context("Claude config has no parent")?)?;
    let _clank_lock = super::setup::installation_lock(&parent)?;
    // Claude 2.1.280 uses proper-lockfile's mkdir <config>.lock lease for its
    // read/modify/atomic-write transaction. A Clank-only flock would not protect
    // against Claude writing at the same time. Never fall back to an unlocked
    // write. Contention retries happen only here, during a real launch.
    let native = NativeLock::acquire(&path.with_extension("json.lock"))?;
    let mut config = read()?; // re-read AFTER acquiring Claude's lock
    let object = config
        .as_object_mut()
        .context("Claude config must be a JSON object")?;
    let projects = object
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("Claude config projects must be a JSON object")?;
    let project = projects
        .entry(key)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("Claude project state must be a JSON object")?;
    if project.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true)) {
        return Ok(());
    }
    project.insert("hasTrustDialogAccepted".into(), true.into());
    let mut temp = tempfile::NamedTempFile::new_in(&parent)?;
    match std::fs::metadata(&target) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.permissions().readonly(),
                "Claude config {} is read-only",
                path.display()
            );
            temp.as_file().set_permissions(metadata.permissions())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    serde_json::to_writer_pretty(&mut temp, &config)?;
    temp.write_all(b"\n")?;
    temp.as_file_mut().sync_all()?;
    // A stalled process must not publish after Claude reclaimed its lease.
    native.refresh()?;
    temp.persist(&target).map_err(|error| error.error)?;
    Ok(())
}

/// Match Claude's native lock directory, default 10-second stale lease and
/// bounded contention backoff. No watcher, background task or idle polling.
struct NativeLock {
    path: std::path::PathBuf,
    directory: std::fs::File,
}

impl NativeLock {
    const STALE: std::time::Duration = std::time::Duration::from_secs(10);

    fn acquire(path: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut retries = [200, 400, 800, 1600, 3200, 4000].into_iter();
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => {
                    let directory = match std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                        .open(path)
                    {
                        Ok(file) => file,
                        Err(error) => {
                            let _ = std::fs::remove_dir(path);
                            return Err(error.into());
                        }
                    };
                    return Ok(Self {
                        path: path.to_owned(),
                        directory,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = match std::fs::symlink_metadata(path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error.into()),
                    };
                    anyhow::ensure!(
                        metadata.is_dir(),
                        "Claude config lock {} is not a directory",
                        path.display()
                    );
                    if metadata.modified()?.elapsed().unwrap_or_default() > Self::STALE {
                        // Same stale-lock recovery as proper-lockfile. Only an
                        // empty directory is removed, never its contents.
                        match std::fs::remove_dir(path) {
                            Ok(()) => continue,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error.into()),
                        }
                    }
                    let Some(delay) = retries.next() else {
                        anyhow::bail!(
                            "Claude config lock {} is still held; retry after the other Claude configuration write completes",
                            path.display()
                        );
                    };
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn owns(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.directory
            .metadata()
            .ok()
            .zip(std::fs::symlink_metadata(&self.path).ok())
            .is_some_and(|(held, current)| {
                current.is_dir() && held.dev() == current.dev() && held.ino() == current.ino()
            })
    }

    fn refresh(&self) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;
        anyhow::ensure!(
            self.owns(),
            "Claude config lock was replaced; configuration was not changed"
        );
        anyhow::ensure!(
            self.directory
                .metadata()?
                .modified()?
                .elapsed()
                .unwrap_or_default()
                < Self::STALE,
            "Claude config lock lease expired; configuration was not changed; retry launch"
        );
        if unsafe { libc::futimens(self.directory.as_raw_fd(), std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for NativeLock {
    fn drop(&mut self) {
        if self.owns() {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_preserves_config_projects_permissions_and_repeated_bytes() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json");
        let before = serde_json::json!({"theme":"light", "oauthAccount":{"keep":"account"},
            "projects":{"/other":{"hasTrustDialogAccepted":false,"allowedTools":["Read"]},
            "/repo":{"hasTrustDialogAccepted":false,"mcpServers":{"keep":{}}}}});
        std::fs::write(&path, serde_json::to_vec(&before).unwrap()).unwrap();
        ensure(home.path(), Path::new("/repo")).unwrap();
        let first = std::fs::read(&path).unwrap();
        let mut expected = before;
        expected["projects"]["/repo"]["hasTrustDialogAccepted"] = true.into();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first).unwrap(),
            expected
        );
        ensure(home.path(), Path::new("/repo")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert!(!path.with_extension("json.lock").exists());
    }

    #[test]
    fn already_trusted_read_only_profile_needs_no_lock_or_write() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json");
        std::fs::write(
            &path,
            "{\"projects\":{\"/repo\":{\"hasTrustDialogAccepted\":true}}}",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = ensure(home.path(), Path::new("/repo"));
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(!home.path().join(".clank").exists());
        assert!(!path.with_extension("json.lock").exists());
    }

    #[test]
    fn concurrent_launches_keep_all_project_decisions() {
        let home = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for n in 0..8 {
                let home = home.path();
                scope.spawn(move || {
                    ensure(home, Path::new(&format!("/repo-{n}"))).unwrap();
                });
            }
        });
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(config["projects"].as_object().unwrap().len(), 8);
        assert_eq!(
            std::fs::metadata(home.path().join(".claude.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn malformed_and_read_only_configuration_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json");
        for value in [
            "invalid JSON",
            "[]",
            "{\"projects\":null}",
            "{\"projects\":{\"/repo\":false}}",
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(ensure(home.path(), Path::new("/repo")).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), value);
            assert!(!path.with_extension("json.lock").exists());
        }
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(ensure(home.path(), Path::new("/repo")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn native_writer_is_respected_and_its_update_is_re_read() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json");
        let lock = NativeLock::acquire(&path.with_extension("json.lock")).unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| ensure(home.path(), Path::new("/repo")));
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::write(&path, "{\"nativeWrite\":true}").unwrap();
            drop(lock);
            worker.join().unwrap().unwrap();
        });
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["nativeWrite"], true);
        assert_eq!(value["projects"]["/repo"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn replaced_native_lock_is_neither_published_under_nor_removed() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".claude.json.lock");
        let held = NativeLock::acquire(&path).unwrap();
        std::fs::remove_dir(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(held.refresh().is_err());
        drop(held);
        assert!(path.exists());
    }

    #[test]
    fn abandoned_native_lease_is_recovered_and_config_symlink_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        std::fs::create_dir(&profile).unwrap();
        let target = temp.path().join("config.json");
        std::fs::write(&target, "{\"keep\":true}").unwrap();
        std::os::unix::fs::symlink(&target, profile.join(".claude.json")).unwrap();
        let path = profile.join(".claude.json.lock");
        let abandoned = NativeLock::acquire(&path).unwrap();
        abandoned
            .directory
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        ensure(&profile, Path::new("/repo")).unwrap();
        assert!(
            std::fs::symlink_metadata(profile.join(".claude.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(config["keep"], true);
        assert_eq!(config["projects"]["/repo"]["hasTrustDialogAccepted"], true);
        assert!(!abandoned.owns());
    }

    #[tokio::test]
    async fn linked_worktree_uses_main_checkout_key_and_not_a_parent_directory() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let child = temp.path().join("child");
        let profile = temp.path().join("profile");
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        };
        git(&["init", "-q", main.to_str().unwrap()]);
        git(&[
            "-C",
            main.to_str().unwrap(),
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ]);
        git(&[
            "-C",
            main.to_str().unwrap(),
            "worktree",
            "add",
            "--detach",
            child.to_str().unwrap(),
        ]);
        prepare(&profile, &child).await.unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(profile.join(".claude.json")).unwrap()).unwrap();
        let projects = config["projects"].as_object().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects[dunce::canonicalize(&main).unwrap().to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        assert!(!projects.contains_key(child.to_str().unwrap()));
        assert!(!projects.contains_key(temp.path().to_str().unwrap()));
    }
}
