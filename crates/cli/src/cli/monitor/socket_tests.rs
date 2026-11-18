use super::*;
use crate::cli::monitor::fixtures::*;

/// The reason the socket is not under the repo.
#[test]
fn a_socket_path_does_not_grow_with_the_repo() {
    // Under the repo, this one spends 95 of the 104 before the
    // filename; the candidates spend a constant 20-odd.
    let deep = "/Users/llfourn/src/frostsnap-ci/.clank/worktrees/nonce-aware-coin-selection";
    assert!(
        format!("{deep}/.clank/monitor.sock").len() > 90,
        "the case this avoids"
    );
    for dir in socket_dirs(501, None) {
        assert!(
            socket_path(&dir, 99999).is_some(),
            "{} must hold a socket",
            dir.display()
        );
    }
}

/// A candidate's own length is not known in advance.
///
/// `$XDG_RUNTIME_DIR` is read from the environment and is tried
/// FIRST, so it is the one candidate that can be too long. Skipping
/// it must leave the shorter ones usable rather than failing the
/// monitor.
#[test]
fn a_runtime_dir_too_long_for_a_socket_is_skipped_not_fatal() {
    let long = std::path::PathBuf::from(format!("/run/user/501/{}", "d".repeat(120)));
    let dirs = socket_dirs(501, Some(&long));

    assert!(
        socket_path(&dirs[0], 99999).is_none(),
        "the long candidate cannot hold one"
    );
    assert!(
        dirs[1..].iter().any(|d| socket_path(d, 99999).is_some()),
        "and a shorter candidate still can"
    );
}

/// The boundary itself, since a path that is exactly the limit does
/// not fit once the kernel's terminator is counted.
#[test]
fn the_length_boundary_leaves_room_for_the_terminator() {
    // Pinned as a LITERAL, not through the constant: every other
    // assertion here is relative to it, so they all move with it
    // and a widening to Linux's 108 would pass them while failing
    // to bind on macOS.
    assert_eq!(
        SUN_PATH_MAX, 104,
        "macOS `sockaddr_un`, the stricter of the two platforms"
    );

    let pad = |n: usize| std::path::PathBuf::from(format!("/{}", "d".repeat(n)));
    let name = "/99999.sock".len();
    // dir + "/99999.sock" lands exactly on SUN_PATH_MAX.
    let exact = pad(SUN_PATH_MAX - name - 1);
    assert_eq!(
        exact.join("99999.sock").to_string_lossy().len(),
        SUN_PATH_MAX
    );
    assert!(socket_path(&exact, 99999).is_none(), "exactly at it is out");
    assert!(
        socket_path(&pad(SUN_PATH_MAX - name - 2), 99999).is_some(),
        "one under it is in"
    );
}

/// The shared directory first, the private one as the fallback.
#[test]
fn a_private_directory_is_the_fallback_not_the_default() {
    let dirs = socket_dirs(501, Some(std::path::Path::new("/run/user/501")));
    let shown: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    assert_eq!(
        shown,
        vec![
            "/run/user/501/clank-socks".to_string(),
            "/tmp/clank-socks".to_string(),
            "/tmp/clank-socks-501".to_string(),
        ]
    );
    assert!(
        !socket_dirs(501, None).iter().any(|d| d.starts_with("/run")),
        "no XDG dir, no XDG candidate"
    );
}

/// `/tmp` is world-writable, so the directory has to be ours and
/// ours alone before a socket goes in it.
#[test]
fn a_directory_someone_else_can_write_is_refused() {
    assert!(dir_is_acceptable(501, 0o700, 501));
    assert!(!dir_is_acceptable(502, 0o700, 501), "another user owns it");
    assert!(!dir_is_acceptable(501, 0o770, 501), "group-WRITABLE");
    assert!(!dir_is_acceptable(501, 0o777, 501), "world-writable");
    assert!(!dir_is_acceptable(501, 0o720, 501), "group-write alone");
    assert!(!dir_is_acceptable(501, 0o702, 501), "other-write alone");
    // Read and traverse are not the threat: the socket is 0600, so
    // listing the directory buys nothing, and a 022 umask makes
    // this the ordinary case for a dir we just created.
    assert!(dir_is_acceptable(501, 0o755, 501), "readable is fine");
    assert!(dir_is_acceptable(501, 0o500, 501), "tighter is fine");
}

/// The production bind path, not a harness around it.
#[test]
fn binding_takes_the_first_candidate_that_will_hold_a_socket() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();

    let too_long = home.path().join("d".repeat(120));
    let good = home.path().join("socks");
    let (_listener, path) = bind_listener(
        &[too_long.clone(), good.clone()],
        4242,
        rustix::process::getuid().as_raw(),
        &lease,
    )
    .expect("the second candidate takes it");

    assert!(path.starts_with(&good), "{}", path.display());
    assert!(path.exists(), "the socket is there");
    assert!(!too_long.exists(), "and the long one was not created");

    // The directory keeps others out, and so does the socket: a
    // world-connectable one would let any local user talk to this
    // repo's monitor.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o177, 0, "socket mode {:o}", mode & 0o777);
}

/// A directory someone else could write is skipped, not used.
#[test]
fn a_loose_directory_is_skipped_for_the_next_candidate() {
    use std::os::unix::fs::PermissionsExt;
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();

    let loose = home.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
    let tight = home.path().join("tight");

    let (_l, path) = bind_listener(
        &[loose.clone(), tight.clone()],
        4242,
        rustix::process::getuid().as_raw(),
        &lease,
    )
    .expect("the tight one is used");
    assert!(path.starts_with(&tight), "{}", path.display());
}

/// A socket left by a dead monitor must not block the next one
/// forever: binding over it is EADDRINUSE, and the lease is what
/// makes removing it safe.
#[test]
fn a_stale_socket_file_does_not_block_a_new_monitor() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("socks");
    std::fs::create_dir_all(&dir).unwrap();

    let path = socket_path(&dir, 4242).unwrap();
    std::fs::write(&path, b"a corpse").unwrap();

    let (_l, bound) = bind_listener(&[dir], 4242, rustix::process::getuid().as_raw(), &lease)
        .expect("binds anyway");
    assert_eq!(bound, path);
}

#[test]
fn every_candidate_failing_says_why() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let why = bind_listener(
        &[home.path().join("d".repeat(120))],
        4242,
        rustix::process::getuid().as_raw(),
        &lease,
    )
    .expect_err("nothing usable")
    .to_string();
    assert!(why.contains("too long"), "{why}");
}

/// The creation race: `recursive(true)` succeeds on a directory
/// that already exists, so a racer who creates it between our stat
/// and our create would have theirs accepted unvetted.
#[test]
fn a_directory_created_under_us_is_still_vetted() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("socks");
    let me = rustix::process::getuid().as_raw();

    let planted = dir.clone();
    let why = ensure_dir_with(&dir, me, move |_| {
        // The racer wins: a world-writable directory now exists
        // where we were about to create our own 0700 one.
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o777)).unwrap();
    })
    .expect_err("a planted directory must not be accepted");
    assert!(why.contains("mode 777"), "{why}");
}

/// And the ordinary path still creates and accepts its own.
#[test]
fn a_directory_we_create_is_accepted() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("socks");
    ensure_dir(&dir, rustix::process::getuid().as_raw()).expect("ours is fine");
    assert!(dir.is_dir());
}

/// A stat failure that is not NotFound is a reason to stop, not a
/// licence to create: a file in the way must not be replaced.
#[test]
fn something_that_is_not_a_directory_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("socks");
    std::fs::write(&path, b"in the way").unwrap();
    let why = ensure_dir(&path, rustix::process::getuid().as_raw()).expect_err("refused");
    assert!(why.contains("not a directory"), "{why}");
}

/// A socket that cannot be restricted is abandoned, not published.
#[test]
fn a_socket_that_cannot_be_restricted_is_not_published() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let first = home.path().join("first");
    let second = home.path().join("second");
    let me = rustix::process::getuid().as_raw();

    // Fails for the first candidate only, so the fallback is
    // exercised as well as the refusal.
    let first_for_hook = first.clone();
    let (_l, path) = bind_listener_with(
        &[first.clone(), second.clone()],
        4242,
        me,
        &lease,
        move |p: &std::path::Path| {
            if p.starts_with(&first_for_hook) {
                Err("chmod refused".to_string())
            } else {
                restrict(p)
            }
        },
    )
    .expect("the second candidate is used");

    assert!(path.starts_with(&second), "{}", path.display());
    assert!(
        !socket_path(&first, 4242).unwrap().exists(),
        "the abandoned socket was removed, not left reachable"
    );
}

#[test]
fn restriction_failing_everywhere_is_an_error_not_a_socket() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let only = home.path().join("only");
    let why = bind_listener_with(
        std::slice::from_ref(&only),
        4242,
        rustix::process::getuid().as_raw(),
        &lease,
        |_| Err("chmod refused".to_string()),
    )
    .expect_err("no socket may be returned");
    assert!(why.contains("chmod refused"), "{why}");
    assert!(
        !socket_path(&only, 4242).unwrap().exists(),
        "and none left behind"
    );
}

/// A symlink is refused for being a symlink, however good its
/// target looks: the uid and mode would be the target's, and the
/// link is replaceable by whoever owns it.
#[test]
fn a_symlinked_candidate_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let real = home.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let me = rustix::process::getuid().as_raw();

    // The target is ours and perfectly acceptable.
    ensure_dir(&real, me).expect("the target itself is fine");

    let why = ensure_dir(&link, me).expect_err("the link is not");
    assert!(why.contains("symlink"), "{why}");
}

/// And one planted in the creation gap, where `recursive(true)`
/// succeeds because the path now exists.
#[test]
fn a_symlink_planted_under_us_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let real = home.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let dir = home.path().join("socks");

    let planted = dir.clone();
    let target = real.clone();
    let why = ensure_dir_with(&dir, rustix::process::getuid().as_raw(), move |_| {
        std::os::unix::fs::symlink(&target, &planted).unwrap();
    })
    .expect_err("a planted link must not be accepted");
    assert!(why.contains("symlink"), "{why}");
}

/// Refusing it lets the private fallback do its job.
#[test]
fn a_symlinked_candidate_falls_through_to_the_next() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).unwrap();
    let home = tempfile::tempdir().unwrap();

    let real = home.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let private = home.path().join("private");

    let (_l, path) = bind_listener(
        &[link.clone(), private.clone()],
        4242,
        rustix::process::getuid().as_raw(),
        &lease,
    )
    .expect("the private candidate is used");
    assert!(path.starts_with(&private), "{}", path.display());
}

/// A stat that fails for a reason OTHER than NotFound must stop,
/// not fall through to creating. Both outcomes are errors, so the
/// message is what says which branch ran.
#[test]
fn a_stat_failure_that_is_not_absence_refuses_without_creating() {
    let home = tempfile::tempdir().unwrap();
    let blocker = home.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a directory").unwrap();

    // Statting through a file gives ENOTDIR, not NotFound.
    let why = ensure_dir(&blocker.join("socks"), rustix::process::getuid().as_raw())
        .expect_err("refused");
    assert!(
        why.contains("cannot stat"),
        "the stat decided, not the create: {why}"
    );
}

/// The read-back, driven against a file made permissive on purpose.
#[test]
fn a_mode_that_did_not_take_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("sock");
    std::fs::write(&path, b"").unwrap();

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    verify_private(&path).expect("0600 is private");

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let why = verify_private(&path).expect_err("0666 is not");
    assert!(why.contains("stayed at mode 666"), "{why}");
}

#[test]
fn a_private_mode_is_owner_only() {
    assert!(mode_is_private(0o600));
    assert!(mode_is_private(0o400));
    assert!(!mode_is_private(0o660), "group");
    assert!(!mode_is_private(0o606), "other");
    assert!(!mode_is_private(0o666));
}

#[test]
fn a_dead_monitors_socket_is_swept_and_a_live_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["11.sock", "22.sock", "33.sock", "notes.txt", "x.sock"] {
        std::fs::write(dir.path().join(name), b"").unwrap();
    }
    // 22 is live, 33 is ours, and the two non-pid names are not
    // this module's to judge.
    let swept = sweep_dead_sockets(dir.path(), 33, |pid| pid == 22);
    assert_eq!(swept, 1);
    let left: std::collections::BTreeSet<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        left,
        ["22.sock", "33.sock", "notes.txt", "x.sock"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

#[test]
fn our_own_pid_is_live() {
    assert!(crate::signal::alive(std::process::id() as i32));
    assert!(!crate::signal::alive(0), "no process is pid 0");
    assert!(
        !crate::signal::alive(-1),
        "and -1 is every process, not one"
    );
}

/// Binding clears the litter, which is the only place that can.
#[test]
fn binding_sweeps_what_dead_monitors_left() {
    let lease = acquire_lease(repo_dir().path()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("999999.sock"), b"").unwrap();
    let (_listener, path) = bind_listener(
        &[dir.path().to_path_buf()],
        std::process::id() as i32,
        rustix::process::getuid().as_raw(),
        &lease,
    )
    .unwrap();
    assert!(path.is_file() || path.exists(), "ours is bound");
    assert!(
        !dir.path().join("999999.sock").exists(),
        "a dead monitor's socket is gone"
    );
}
