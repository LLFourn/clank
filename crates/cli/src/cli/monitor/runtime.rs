use super::socket::ensure_dir;

/// The right to try a REPAIR of this repo's monitor, held while one
/// client does it.
///
/// Not the canonical lease. That one stops two monitors OWNING a repo
/// and says nothing about how many clients may poll for it or fork a
/// child at it: each could take and drop it and each then start one,
/// which is a poller and a fork per waiting client (codex on
/// cf29219). This coalesces them to one useful repair per repo.
///
/// Flocked, so it coalesces ACROSS processes — every parked wait is
/// its own — and held until startup and classification are done,
/// including the failing and expiring paths, because a permit
/// released early is one the next client takes while this one is
/// still working.
///
/// It lives beside the SOCKET rather than under `.clank/`: it is
/// process state with no meaning once the process is gone, and the
/// repo is not where such things belong.
#[derive(Debug)]
pub(crate) struct RepairPermit {
    _file: std::fs::File,
}

impl Drop for RepairPermit {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this open-file description
        // before CLOEXEC closes it. Release explicitly when repair ends,
        // rather than leave admission waiting for that unrelated child.
        let _ = rustix::fs::flock(&self._file, rustix::fs::FlockOperation::Unlock);
    }
}

/// Where this uid's RUNTIME state lives: the descriptor, the monitor
/// lease, the repair gate and the status lease.
///
/// Outside the repo, because none of it means anything once its
/// process is gone — and because whether git could see it depended on
/// each repo's `.clank/.gitignore`, a file that propagates by being
/// TRACKED. A fork from a repo that never had one started dirty and
/// nothing in the new worktree fixed it.
///
/// FIXED, and deliberately independent of the environment. Socket
/// allocation may try candidates because the published descriptor then
/// tells everyone which was chosen; a rendezvous has no such
/// discovery, so every caller must derive the SAME path or it is not
/// one. Two clients differing only in `XDG_RUNTIME_DIR` each took a
/// repair permit in a different directory, locked different inodes,
/// and both forked (codex on 88b6234).
pub(crate) fn runtime_dir(me: u32) -> std::path::PathBuf {
    runtime_dir_under(test_runtime_root(), me)
}

/// `None` is the live namespace: `/tmp`, not `TMPDIR` or anything
/// else a process inherits.
fn runtime_dir_under(test_root: Option<std::path::PathBuf>, me: u32) -> std::path::PathBuf {
    test_root
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(format!("clank-run-{me}"))
}

#[cfg(not(any(test, feature = "test-runtime")))]
fn test_runtime_root() -> Option<std::path::PathBuf> {
    None
}

/// A test process never joins the developer's live namespace, however
/// it was started: the runner's `CLANK_TEST_RUNTIME` when given (shared
/// with the children it spawns), else a root private to this process.
/// Deciding here, at first use, rather than in a harness setup step
/// means no test can reach the runtime before isolation is in place.
#[cfg(any(test, feature = "test-runtime"))]
pub(crate) fn test_runtime_root() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("CLANK_TEST_RUNTIME") {
        let root = std::path::PathBuf::from(root);
        assert!(root.is_absolute(), "CLANK_TEST_RUNTIME must be absolute");
        return Some(root);
    }
    Some(
        PRIVATE_TEST_RUNTIME
            .get_or_init(|| {
                // `/tmp`, not `target/`: socket paths under it must fit
                // `SUN_PATH_MAX`, and a target dir can be arbitrarily deep.
                let root = tempfile::Builder::new()
                    .prefix("clank-ut-")
                    .tempdir_in("/tmp")
                    .unwrap()
                    .keep();
                remove_private_test_runtime_at_exit();
                root
            })
            .clone(),
    )
}

/// One per process, so every thread of it meets at the same leases.
#[cfg(any(test, feature = "test-runtime"))]
static PRIVATE_TEST_RUNTIME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Statics are never dropped, and libtest leaves `main` through
/// `process::exit` on failure, so a `TempDir` guard would never run.
#[cfg(any(test, feature = "test-runtime"))]
#[allow(unsafe_code)]
fn remove_private_test_runtime_at_exit() {
    extern "C" fn remove_private_test_runtime() {
        if let Some(root) = PRIVATE_TEST_RUNTIME.get() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
    // SAFETY: `atexit` only stores the pointer. The handler captures
    // nothing, cannot unwind (it discards its only error), and reads a
    // `OnceLock` that is set before registration and never cleared.
    unsafe {
        libc::atexit(remove_private_test_runtime);
    }
}

/// Vet and create this uid's runtime directory.
///
/// For the MUTATION boundaries only. Discovery derives a path without
/// it, so a read-only lookup never has to make a directory to find out
/// where something would be.
pub(crate) fn ensure_runtime_dir() -> Result<(), String> {
    let me = rustix::process::getuid().as_raw();
    ensure_dir(&runtime_dir(me), me)
}

/// This repo's runtime path with the given extension.
///
/// Derivation ONLY: it creates nothing and needs no write access, so a
/// read-only lookup — which is what discovery is — can ask where a
/// descriptor would be without making a directory to find out. The
/// mutation boundaries vet and create (codex on 2a0c59a).
pub(crate) fn runtime_path(repo: &std::path::Path, ext: &str) -> std::path::PathBuf {
    runtime_path_in(&runtime_dir(rustix::process::getuid().as_raw()), repo, ext)
}

/// [`runtime_path`] with the namespace supplied.
///
/// The derivation itself, so a test can hand it a directory that does
/// NOT exist and watch it stay that way. Asking against the real
/// namespace proves nothing: other tests have already created it.
pub(crate) fn runtime_path_in(
    dir: &std::path::Path,
    repo: &std::path::Path,
    ext: &str,
) -> std::path::PathBuf {
    dir.join(format!("{}.{ext}", repo_tag(repo)))
}

/// A temp repo that takes its RUNTIME entries with it.
///
/// The namespace outlives the repo it is keyed by, which is the whole
/// point — and which means a tempdir that used to carry its lock away
/// now leaves one behind. Production's set is bounded by the repos on
/// the machine; a suite that makes a fresh repo per test is bounded by
/// nothing, and left about four hundred entries per run.
///
/// Lives here rather than in each test module because there is one
/// namespace, and a second copy of this list would be a second answer
/// to the same question.
///
/// Unlinking is safe HERE and not in production: the name is derived
/// from this tempdir's own path, so no other repo's lease can be at it,
/// and drop order releases a lease declared after the repo before this
/// runs. In production, unlinking a lock a holder still has open would
/// let the next caller lock a different inode at the same name — which
/// is why nothing sweeps this directory.
#[cfg(test)]
pub(crate) struct TempRepo(tempfile::TempDir);

#[cfg(test)]
impl TempRepo {
    pub(crate) fn new() -> Self {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".clank")).unwrap();
        Self(d)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[cfg(test)]
impl Drop for TempRepo {
    fn drop(&mut self) {
        for ext in [
            "json",
            "lock",
            "status",
            "repair",
            "pane-owners.json",
            "pane-owners.lock",
        ] {
            let _ = std::fs::remove_file(runtime_path(self.0.path(), ext));
        }
        // Mutation callers canonicalize even when a test's temp root has a
        // /var or /tmp alias. Their inode is keyed by that canonical path.
        if let Ok(repo) = dunce::canonicalize(self.0.path()) {
            let _ = std::fs::remove_file(runtime_path(&repo, "plan-mutation"));
            let _ = std::fs::remove_file(runtime_path(&repo, "roster"));
        }
    }
}

/// What asking for the repair gate established.
///
/// `Busy` and `Unavailable` are different facts and were one: every
/// open, create and lock failure reported as "another client is
/// already starting one", which is not established when the namespace
/// itself could not be used.
#[derive(Debug)]
pub(crate) enum Repair {
    Acquired(RepairPermit),
    /// Somebody else is repairing this repo.
    Busy,
    /// The gate could not be used at all, and why.
    Unavailable(String),
}

/// Take this repo's repair gate.
pub(crate) fn take_repair_permit(repo: &std::path::Path) -> Repair {
    let me = rustix::process::getuid().as_raw();
    take_repair_permit_in(repo, &runtime_dir(me), me)
}

/// [`take_repair_permit`] with the namespace supplied, so a test can
/// drive an unusable one — and so the production choice of `repair_dir`
/// is observable rather than assumed.
pub(crate) fn take_repair_permit_in(
    repo: &std::path::Path,
    dir: &std::path::Path,
    me: u32,
) -> Repair {
    // The same vetting the socket boundary does, for a reason that
    // belongs to this lock rather than to security in general: a gate
    // file replaced while a holder keeps its old fd lets the next
    // caller lock a DIFFERENT inode at the same path, and then both
    // are repairing.
    if let Err(why) = ensure_dir(dir, me) {
        return Repair::Unavailable(why);
    }
    match repair_permit(repo, dir) {
        Ok(Some(permit)) => Repair::Acquired(permit),
        Ok(None) => Repair::Busy,
        Err(why) => Repair::Unavailable(why),
    }
}

/// Take the repair permit in `dir`.
pub(crate) fn repair_permit(
    repo: &std::path::Path,
    dir: &std::path::Path,
) -> Result<Option<RepairPermit>, String> {
    let path = dir.join(format!("{}.repair", repo_tag(repo)));
    // Never unlinked: the held fd and its flock ARE the state, and
    // removing the file on release would let the next caller create a
    // new inode at the same name beside a holder still on the old one.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open the repair gate at {}: {e}", path.display()))?;
    // One attempt: its caller polls it to a deadline on a runtime thread.
    let taken = clank_user_config::lease::take_now(&file)
        .map_err(|e| format!("cannot lock the repair gate at {}: {e}", path.display()))?;
    Ok(taken.then_some(RepairPermit { _file: file }))
}

/// A stable digest of a canonical repo path.
///
/// blake3 over the path's RAW BYTES. `DefaultHasher` was wrong twice
/// over: its algorithm is explicitly unspecified across Rust releases,
/// which bites exactly when binaries from two installs contend during
/// an upgrade, and `to_string_lossy` is not the path. The scheme must
/// not vary with `PROTO`, the compiler, or the standard library.
pub(crate) fn repo_tag(repo: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    blake3::hash(repo.as_os_str().as_bytes()).to_hex()[..32].to_string()
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
