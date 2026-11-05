//! Where the remote token is kept: never in the user config, which
//! agents and transcripts read as a matter of course.
//!
//! A store is reached only from inside [`crate::door::Door`]'s hold of
//! `~/.clank/remote.lock`; nothing here locks.

use std::path::{Path, PathBuf};

/// The token's backing store.
///
/// An error never carries the token: the door masks what a store
/// says before it reaches anyone, but a store must not count on it.
pub trait Secrets: Send + Sync {
    /// The stored token, or `None` when nothing is stored.
    fn read(&self) -> anyhow::Result<Option<String>>;
    /// Store `token`, replacing any held.
    fn write(&self, token: &str) -> anyhow::Result<()>;
    /// The store in words an operator can act on.
    fn describe(&self) -> String;
}

/// This user's store for `home`: the macOS login keychain when `home`
/// is the login account's own home, otherwise `~/.clank/remote-token`.
///
/// The keychain is per account, not per home, so a door run under a
/// substituted `HOME` (a test, a sandbox) sharing it would hand one
/// home's token to another.
pub fn platform(home: &Path) -> std::sync::Arc<dyn Secrets> {
    #[cfg(target_os = "macos")]
    if login_home().is_some_and(|login| same_dir(&login, home)) {
        return std::sync::Arc::new(Keychain::new(SecurityFramework, home));
    }
    std::sync::Arc::new(TokenFile::at(home))
}

#[cfg(target_os = "macos")]
fn same_dir(a: &Path, b: &Path) -> bool {
    identity(a) == identity(b)
}

/// One name for a home however it is spelled — `/Users/x`,
/// `/Users/x/.`, a symlink to it — so every spelling that shares its
/// `remote.lock` and sessions also shares its token.
fn identity(home: &Path) -> PathBuf {
    std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf())
}

/// The passwd entry's home for this uid, which `HOME` cannot move.
#[cfg(target_os = "macos")]
fn login_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local sized as passed;
    // getpwuid_r writes only within them.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` points into `buf`, NUL-terminated by getpwuid_r.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

pub const KEYCHAIN_SERVICE: &str = "clank-remote";
const TOKEN_FILE: &str = ".clank/remote-token";

/// The keychain calls a [`Keychain`] makes: a seam, so a test can see
/// every argument without touching a real keychain.
pub trait KeychainApi: Send + Sync {
    /// `None` when there is no such item.
    fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<Vec<u8>>>;
    fn set(&self, service: &str, account: &str, secret: &[u8]) -> anyhow::Result<()>;
}

/// A generic password, service [`KEYCHAIN_SERVICE`], account the home
/// it serves. The token goes to the Security framework as the item's
/// data and never through a subprocess's argv.
pub struct Keychain<A> {
    api: A,
    account: String,
}

impl<A: KeychainApi> Keychain<A> {
    pub fn new(api: A, home: &Path) -> Self {
        Self {
            api,
            account: identity(home).display().to_string(),
        }
    }
}

impl<A: KeychainApi> Secrets for Keychain<A> {
    fn read(&self) -> anyhow::Result<Option<String>> {
        let Some(bytes) = self.api.get(KEYCHAIN_SERVICE, &self.account)? else {
            return Ok(None);
        };
        let token = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{} holds something that is not text", self.describe()))?;
        Ok(Some(token).filter(|t| !t.is_empty()))
    }

    fn write(&self, token: &str) -> anyhow::Result<()> {
        self.api
            .set(KEYCHAIN_SERVICE, &self.account, token.as_bytes())
    }

    fn describe(&self) -> String {
        format!(
            "the login keychain's generic password `{KEYCHAIN_SERVICE}` (account `{}`)",
            self.account
        )
    }
}

#[cfg(target_os = "macos")]
pub struct SecurityFramework;

#[cfg(target_os = "macos")]
impl KeychainApi for SecurityFramework {
    fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<Vec<u8>>> {
        const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => anyhow::bail!("reading the keychain: {e}"),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &[u8]) -> anyhow::Result<()> {
        security_framework::passwords::set_generic_password(service, account, secret)
            .map_err(|e| anyhow::anyhow!("writing the keychain: {e}"))
    }
}

/// `~/.clank/remote-token`: every file it writes is created
/// `O_CREAT|O_EXCL` with mode 0600, so the token is never readable by
/// others for an instant, and is never chmodded after. Against a
/// process running as the user it is no protection at all.
pub struct TokenFile {
    path: PathBuf,
}

impl TokenFile {
    pub fn at(home: &Path) -> Self {
        Self {
            path: identity(home).join(TOKEN_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Secrets for TokenFile {
    fn read(&self) -> anyhow::Result<Option<String>> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("opening `{}`: {e}", self.path.display()),
        };
        let meta = file.metadata()?;
        anyhow::ensure!(
            meta.is_file(),
            "`{}` is not a regular file",
            self.path.display()
        );
        let mode = meta.mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "`{}` is readable by others (mode {mode:o}); the remote stays off until \
             `chmod 600 {}`",
            self.path.display(),
            self.path.display()
        );
        let mut token = String::new();
        file.read_to_string(&mut token)
            .map_err(|e| anyhow::anyhow!("reading `{}`: {e}", self.path.display()))?;
        let token = token.trim_end_matches('\n').to_string();
        Ok(Some(token).filter(|t| !t.is_empty()))
    }

    fn write(&self, token: &str) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", self.path.display()))?;
        std::fs::create_dir_all(dir)?;
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).expect("the OS random source answers");
        let tmp = dir.join(format!(
            ".remote-token.{}.tmp",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("writing `{}`: {e}", self.path.display());
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("`{}`", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const TOKEN: &str = "PLACEHOLDER-TOKEN-NOT-REAL";

    #[test]
    fn the_token_file_is_created_0600_and_a_looser_one_is_refused_until_fixed() {
        let home = tempfile::tempdir().unwrap();
        let file = TokenFile::at(home.path());
        assert_eq!(file.read().unwrap(), None);
        file.write(TOKEN).unwrap();
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "created owner-only, not chmodded after");
        assert_eq!(file.read().unwrap().as_deref(), Some(TOKEN));
        file.write("PLACEHOLDER-SECOND").unwrap();
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a replacement is created owner-only too");

        std::fs::remove_file(file.path()).unwrap();
        std::fs::write(file.path(), TOKEN).unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = file.read().expect_err("a world-readable token is refused");
        assert!(format!("{err:#}").contains("chmod 600"), "{err:#}");
        assert!(!format!("{err:#}").contains(TOKEN));
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            file.read().unwrap().as_deref(),
            Some(TOKEN),
            "fixed, it reads"
        );
    }

    #[test]
    fn a_symlinked_token_file_is_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let file = TokenFile::at(home.path());
        std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        let elsewhere = home.path().join("elsewhere");
        std::fs::write(&elsewhere, TOKEN).unwrap();
        std::os::unix::fs::symlink(&elsewhere, file.path()).unwrap();
        assert!(file.read().is_err());
    }

    /// Every call the keychain backend makes, argument by argument.
    #[derive(Default)]
    struct Recording {
        calls: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
        held: std::sync::Mutex<Option<Vec<u8>>>,
    }

    impl KeychainApi for &Recording {
        fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<Vec<u8>>> {
            self.calls
                .lock()
                .unwrap()
                .push((service.into(), account.into(), Vec::new()));
            Ok(self.held.lock().unwrap().clone())
        }
        fn set(&self, service: &str, account: &str, secret: &[u8]) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((service.into(), account.into(), secret.to_vec()));
            *self.held.lock().unwrap() = Some(secret.to_vec());
            Ok(())
        }
    }

    #[test]
    fn the_keychain_gets_the_token_as_item_data_only() {
        let api = Recording::default();
        let home = tempfile::tempdir().unwrap();
        let keychain = Keychain::new(&api, home.path());
        assert_eq!(keychain.read().unwrap(), None);
        keychain.write(TOKEN).unwrap();
        assert_eq!(keychain.read().unwrap().as_deref(), Some(TOKEN));
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for (service, account, _) in calls.iter() {
            assert_eq!(service, KEYCHAIN_SERVICE);
            assert_eq!(account, &identity(home.path()).display().to_string());
            assert!(!service.contains(TOKEN) && !account.contains(TOKEN));
        }
        assert_eq!(calls[1].2, TOKEN.as_bytes(), "the token is the item's data");
        assert!(!keychain.describe().contains(TOKEN));
    }
}
