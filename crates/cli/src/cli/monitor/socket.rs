use super::lease::MonitorLease;

/// Where monitor sockets live, in the order they are tried.
///
/// Copied from Claude Code's own arrangement, which solved this
/// problem first: a short flat directory, never under the repo.
/// `sun_path` is 104 bytes on macOS and clank nests worktrees — the
/// longest repo on the machine this was written for reaches 97 bytes
/// for `<repo>/.clank/monitor.sock`, seven short of unusable, and a
/// longer worktree name is all it takes. Claude Code's regexes name
/// `/tmp/cc-socks`, `/tmp/cc-socks-<uid>` and
/// `/run/user/<uid>/cc-socks`; this is the same shape.
///
/// The shared directory is tried first and the per-uid one is the
/// fallback for when it is not ours, mirroring the documented
/// behaviour: *"refuses to create the socket in a directory it can't
/// accept, for example one that another user owns, and uses a private
/// per-user directory … instead."*
pub(crate) fn socket_dirs(
    uid: u32,
    xdg_runtime: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(run) = xdg_runtime {
        out.push(run.join("clank-socks"));
    }
    out.push(std::path::PathBuf::from("/tmp/clank-socks"));
    out.push(std::path::PathBuf::from(format!("/tmp/clank-socks-{uid}")));
    out
}

/// The longest socket path the kernel will accept.
///
/// `sun_path` in `sockaddr_un` is 104 bytes on macOS and 108 on Linux.
/// The stricter one is used on both: a path that fits it fits
/// everywhere, and a monitor that binds on Linux but not on macOS is a
/// difference nobody notices until it bites someone else.
pub(crate) const SUN_PATH_MAX: usize = 104;

/// The socket path for `pid` in `dir`, or `None` when the kernel would
/// not take it.
///
/// Checked PER CANDIDATE, because a candidate can be arbitrarily long:
/// `$XDG_RUNTIME_DIR` comes from the environment, so the first
/// candidate tried is precisely the one whose length is not known in
/// advance (codex on 06ccd66). A candidate that cannot hold a socket is
/// skipped rather than fatal — the next one is shorter.
pub(crate) fn socket_path(dir: &std::path::Path, pid: i32) -> Option<std::path::PathBuf> {
    let path = dir.join(format!("{pid}.sock"));
    // The kernel copies the bytes and needs room for the terminator.
    (path.to_string_lossy().len() < SUN_PATH_MAX).then_some(path)
}

/// Whether a socket directory may be used.
///
/// `/tmp` is world-writable, so a directory we do not own — or one
/// anyone else can WRITE — would let them plant or replace a socket and
/// intercept or deny a monitor. Write is the whole threat: the socket
/// itself is 0600, so group or other READ on the directory grants
/// nothing, and refusing it would reject an ordinary `/tmp/clank-socks`
/// created under a 022 umask.
///
/// Pure over the stat results, so it is testable without chown.
pub(crate) fn dir_is_acceptable(owner_uid: u32, mode: u32, me: u32) -> bool {
    owner_uid == me && mode & 0o022 == 0
}

/// Whether a mode keeps a socket to its owner.
fn mode_is_private(mode: u32) -> bool {
    mode & 0o177 == 0
}

/// Vet a directory we are about to put a socket in.
///
/// `md` must come from `symlink_metadata`, so the final component is
/// not followed: an attacker-owned symlink at the candidate pointing
/// into a directory we DO own would otherwise pass, because the uid and
/// mode read back belong to the target and not to the link. They could
/// then replace their own link — theirs to remove, even under a sticky
/// `/tmp` — and redirect every later `remove_file`, `bind`, `chmod` and
/// client `connect` through it (codex on 9228189).
///
/// The indirection is the thing refused. With a real directory we own,
/// `/tmp`'s sticky bit stops anyone else replacing it.
fn vet_dir(dir: &std::path::Path, md: &std::fs::Metadata, me: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if md.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; a socket directory must be the real thing",
            dir.display()
        ));
    }
    if !md.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if dir_is_acceptable(md.uid(), md.mode(), me) {
        return Ok(());
    }
    Err(format!(
        "{} is owned by uid {} with mode {:o}",
        dir.display(),
        md.uid(),
        md.mode() & 0o777
    ))
}

/// What a READ-ONLY look at a runtime namespace established.
///
/// Separate from [`ensure_dir`] because discovery must not create. A
/// client asking where a monitor is has no business making a directory,
/// and requiring write access to find out would make a read-only repo
/// undiscoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trust {
    /// Ours, and the same ownership/mode policy a mutation demands.
    Ours,
    /// Not there. An ordinary absence: nothing is published.
    Absent,
    /// There and NOT ours. The string says why.
    Theirs(String),
}

/// Whether a runtime namespace is ours to trust, creating nothing.
///
/// The read half of the boundary, and the half this plan nearly
/// shipped without. Moving the descriptor out of a repo we own and
/// into a PREDICTABLE path under world-writable `/tmp` means another
/// uid can get there first: create `/tmp/clank-run-<our-uid>`, leave a
/// descriptor naming the right repo and proto, and point `socket` at
/// theirs. Matching fields are not evidence of who wrote them. Our
/// client would then read it and connect before any mutation guard ran
/// — refusing to PUBLISH into a planted namespace does nothing about
/// consuming what is already in it (codex on 9dfc3e3).
///
/// Vetted with `vet_dir`, so it is the one policy and not a second
/// opinion: `symlink_metadata` refuses an indirection, and for a real
/// directory we own `/tmp`'s sticky bit stops anyone else replacing it
/// between this answer and the read that follows.
pub(crate) fn trusted_dir(dir: &std::path::Path, me: u32) -> Trust {
    match std::fs::symlink_metadata(dir) {
        Ok(md) => match vet_dir(dir, &md, me) {
            Ok(()) => Trust::Ours,
            Err(why) => Trust::Theirs(why),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Trust::Absent,
        // Not an absence: something is there that cannot be vetted,
        // and reading through it is the thing being prevented.
        Err(e) => Trust::Theirs(format!("cannot stat {}: {e}", dir.display())),
    }
}

/// Create a socket directory, or accept an existing one.
///
/// `/tmp` is world-writable, so a directory we do not own, or one
/// anyone else can write, would let them plant or replace a socket.
///
/// The vet runs AFTER creation as well as before, because
/// `DirBuilder::recursive(true)` succeeds on a directory that already
/// exists: a racer who creates `/tmp/clank-socks` between our stat and
/// our create would otherwise have their directory accepted without
/// its owner or mode ever being looked at (codex on dab07ce). And only
/// `NotFound` permits creating — any other stat error is a reason to
/// stop, not a licence to proceed.
pub(crate) fn ensure_dir(dir: &std::path::Path, me: u32) -> Result<(), String> {
    ensure_dir_with(dir, me, |_| {})
}

/// [`ensure_dir`] with a hook between the stat and the create, so the
/// creation race is reachable deterministically instead of by luck.
fn ensure_dir_with(
    dir: &std::path::Path,
    me: u32,
    between: impl FnOnce(&std::path::Path),
) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;

    // `symlink_metadata`, not `metadata`: the final component must not
    // be followed, or a symlink is vetted by what it points at.
    match std::fs::symlink_metadata(dir) {
        Ok(md) => return vet_dir(dir, &md, me),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot stat {}: {e}", dir.display())),
    }

    between(dir);

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // Also unfollowed: `recursive(true)` succeeds when the path is
    // already a symlink to a directory, so the creation race can plant
    // an indirection as easily as a loose directory.
    let md = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat {} after creating it: {e}", dir.display()))?;
    vet_dir(dir, &md, me)
}

/// Keep the socket to this user, and REPORT when that fails.
///
/// Not best-effort. An earlier version ignored the failure on the
/// grounds that the directory was 0700, which stopped being true once
/// existing 0755 directories were accepted — under a permissive umask
/// the socket itself could then be writable by others and would be
/// published anyway (codex on dab07ce). The achieved mode is read back
/// rather than assumed, because a chmod that reports success on an
/// unusual filesystem still has to produce the mode we need.
pub(crate) fn restrict(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
    verify_private(path)
}

/// Read back what a chmod actually achieved.
///
/// Split out so it is callable in a test: a `set_permissions` that
/// reports success without applying the mode is a filesystem quirk
/// rather than something that can be provoked here, but the CHECK can
/// be driven directly against a file we made permissive on purpose.
fn verify_private(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat {} after restricting it: {e}", path.display()))?
        .permissions()
        .mode();
    if mode_is_private(mode) {
        Ok(())
    } else {
        Err(format!(
            "{} stayed at mode {:o} after being restricted",
            path.display(),
            mode & 0o777
        ))
    }
}

/// Bind a listener in the first candidate directory that will take one.
///
/// A socket file left by a dead monitor is unlinked rather than
/// refused: binding over it would fail with `EADDRINUSE` forever, and
/// the caller holds the LEASE, which is what makes removing it safe —
/// no live owner can exist while that is held.
///
/// Returns a std listener so the async loop can adopt it; the setup
/// path stays synchronous because every failure here is a reason to
/// stop, not to await.
pub(crate) fn bind_listener(
    candidates: &[std::path::PathBuf],
    pid: i32,
    me: u32,
    lease: &MonitorLease,
) -> Result<(std::os::unix::net::UnixListener, std::path::PathBuf), String> {
    bind_listener_with(candidates, pid, me, lease, restrict)
}

/// [`bind_listener`] with the restriction step supplied, so its failure
/// path is reachable without an exotic filesystem.
fn bind_listener_with(
    candidates: &[std::path::PathBuf],
    pid: i32,
    me: u32,
    _lease: &MonitorLease,
    restrict: impl Fn(&std::path::Path) -> Result<(), String>,
) -> Result<(std::os::unix::net::UnixListener, std::path::PathBuf), String> {
    let mut why = Vec::new();
    for dir in candidates {
        let Some(path) = socket_path(dir, pid) else {
            why.push(format!("{} is too long for a socket path", dir.display()));
            continue;
        };
        if let Err(e) = ensure_dir(dir, me) {
            why.push(e);
            continue;
        }
        // Safe only because the lease is held: nothing live owns this.
        let _ = std::fs::remove_file(&path);
        sweep_dead_sockets(dir, pid, crate::signal::alive);
        let listener = match std::os::unix::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(e) => {
                why.push(format!("cannot bind {}: {e}", path.display()));
                continue;
            }
        };
        // An unrestricted socket is worse than none: it is reachable by
        // any local user. Abandon the candidate rather than publish it.
        if let Err(e) = restrict(&path) {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            why.push(e);
            continue;
        }
        return Ok((listener, path));
    }
    Err(format!("no usable socket directory: {}", why.join("; ")))
}

/// Remove sockets left behind by monitors that are gone.
///
/// A socket's name carries its monitor's pid, and a monitor killed with
/// SIGKILL cannot unlink its own. The directory is shared by every
/// repo's monitor, so nothing else is in a position to tidy up, and
/// without this it grows by one dead file per killed monitor forever.
/// The next monitor to bind is the one that can: `pid` is its own and
/// is never swept.
///
/// Conservative on purpose. An unparseable name is left alone — the
/// directory may hold something this version did not create — and a
/// pid that has been REUSED by an unrelated process reads as live, so
/// its socket survives. Leaving litter is the failure to prefer over
/// unlinking a socket somebody is serving on.
fn sweep_dead_sockets(dir: &std::path::Path, mine: i32, live: impl Fn(i32) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = std::path::Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".sock"))
        else {
            continue;
        };
        let Ok(pid) = stem.parse::<i32>() else {
            continue;
        };
        if pid == mine || live(pid) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            swept += 1;
        }
    }
    swept
}

#[cfg(test)]
#[path = "socket_tests.rs"]
mod tests;
