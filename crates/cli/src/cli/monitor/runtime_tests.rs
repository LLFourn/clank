use super::*;
use crate::cli::monitor::fixtures::*;

/// The gate has ONE identity, whatever the environment says.
///
/// Socket allocation may try candidates because the published
/// descriptor then tells everyone which was chosen. A coordination
/// lock has no such discovery, so two clients differing only in
/// `XDG_RUNTIME_DIR` each took a permit in a different directory,
/// locked different inodes, and both forked (codex on 88b6234).
#[test]
fn the_runtime_namespace_does_not_move_with_the_environment() {
    assert_eq!(
        runtime_dir_under(None, 501),
        std::path::PathBuf::from("/tmp/clank-run-501"),
        "fixed per uid, and derived from nothing else"
    );
    assert_eq!(
        runtime_dir_under(Some("/elsewhere".into()), 501),
        std::path::PathBuf::from("/elsewhere/clank-run-501"),
    );
    assert_ne!(
        runtime_dir(501),
        runtime_dir_under(None, 501),
        "a test build never joins the live namespace"
    );
    assert_ne!(
        runtime_dir(501),
        runtime_dir(502),
        "but not shared between users"
    );

    // The socket's candidates DO move with the environment, which
    // is exactly why the gate must not be derived from them.
    let with = socket_dirs(501, Some(std::path::Path::new("/run/user/501")));
    let without = socket_dirs(501, None);
    assert_ne!(with, without, "the socket list is environment-dependent");

    // And the production path USES that fixed namespace: the gate
    // file lands there. Asserting only that `repair_dir` is fixed
    // left the wiring free to derive the directory some other way,
    // which a mutation duly did.
    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();
    let gate = runtime_dir(me).join(format!("{}.repair", repo_tag(repo.path())));
    let _ = std::fs::remove_file(&gate);
    let held = take_repair_permit(repo.path());
    assert!(matches!(held, Repair::Acquired(_)), "{held:?}");
    assert!(
        gate.exists(),
        "the gate every contender must find is at {}",
        gate.display()
    );
}

/// The repo digest is stable across builds.
///
/// `DefaultHasher` was wrong twice over: Rust leaves its algorithm
/// unspecified between releases — which bites exactly when two
/// installed binaries contend during an upgrade — and
/// `to_string_lossy` is not the path. Pinned to a known value so a
/// change to the scheme has to be deliberate.
#[test]
fn the_repo_digest_is_pinned_and_byte_exact() {
    let tag = repo_tag(std::path::Path::new("/w/proj"));
    assert_eq!(tag.len(), 32);
    assert_eq!(
        tag,
        blake3::hash(b"/w/proj").to_hex()[..32].to_string(),
        "blake3 over the path's raw bytes, and nothing else"
    );
    assert_ne!(tag, repo_tag(std::path::Path::new("/w/proj2")));
}
