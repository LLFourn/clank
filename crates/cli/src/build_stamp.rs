//! Which build of clank is running, and which is installed.
//!
//! The running build is said by the executing image itself: a stamp the
//! build script derives from everything the build is made from, embedded
//! as a static. The installed build is said by the file at the path clank
//! was invoked by, scanned for that same stamp. Neither is inferred from a
//! path's metadata, which can name a different file from the one loaded.

include!(concat!(env!("OUT_DIR"), "/build_stamp.rs"));

const MARKER_LEN: usize = 16;
const END: [u8; 4] = *b"\x7fend";
const LEN: usize = MARKER_LEN + 16 + END.len();

const fn framed() -> [u8; LEN] {
    let marker = *b"\x7fclank-stamp-v1\x7f";
    let mut out = [0u8; LEN];
    let mut i = 0;
    while i < MARKER_LEN {
        out[i] = marker[i];
        i += 1;
    }
    let mut j = 0;
    while j < 16 {
        out[MARKER_LEN + j] = STAMP[j];
        j += 1;
    }
    let mut k = 0;
    while k < END.len() {
        out[MARKER_LEN + 16 + k] = END[k];
        k += 1;
    }
    out
}

/// The one copy of the marker in the binary. The scanner reads its marker
/// from here, so searching adds no second occurrence.
#[used]
static CLANK_BUILD_STAMP: [u8; LEN] = framed();

fn embedded() -> [u8; LEN] {
    // SAFETY: a read of an initialised static. Volatile, so the static is
    // what is read rather than the constant it was built from.
    unsafe { std::ptr::read_volatile(&CLANK_BUILD_STAMP) }
}

/// A build's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp(pub [u8; 16]);

impl std::fmt::Display for Stamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The build this process is running.
pub fn running() -> Stamp {
    let mut stamp = [0u8; 16];
    stamp.copy_from_slice(&embedded()[MARKER_LEN..MARKER_LEN + 16]);
    Stamp(stamp)
}

/// The stamp in an executable's bytes: exactly one well-formed one.
pub fn find(bytes: &[u8]) -> Result<Stamp, String> {
    let framed = embedded();
    let marker = &framed[..MARKER_LEN];
    let mut found = bytes
        .windows(MARKER_LEN)
        .enumerate()
        .filter(|(_, window)| *window == marker)
        .map(|(at, _)| at);
    let Some(at) = found.next() else {
        return Err("it carries no build stamp".into());
    };
    if found.next().is_some() {
        return Err("it carries more than one build stamp".into());
    }
    let body = &bytes[at + MARKER_LEN..];
    if body.len() < 16 + END.len() || body[16..16 + END.len()] != END {
        return Err("its build stamp is malformed".into());
    }
    let mut stamp = [0u8; 16];
    stamp.copy_from_slice(&body[..16]);
    Ok(Stamp(stamp))
}

/// What the installation says about the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    Current,
    /// A different build is installed: proof this one is stale.
    Replaced(Stamp),
    /// Nothing could be established. Never treated as replacement.
    Unavailable(String),
}

/// Which file, and its state: what a completed scan is cached by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Signature {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
}

fn signature(path: &std::path::Path) -> std::io::Result<Signature> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)?;
    Ok(Signature {
        dev: md.dev(),
        ino: md.ino(),
        len: md.len(),
        mtime: (md.mtime(), md.mtime_nsec()),
        ctime: (md.ctime(), md.ctime_nsec()),
        mode: md.mode(),
    })
}

/// The installation at one path, looked at again on each wake.
///
/// A completed scan (read whole, the file unchanged across the read) is
/// kept by signature whatever it found, so an unchanged file is read once.
/// What could not be established is kept for nothing and tried again.
#[derive(Debug)]
pub struct Installation {
    path: std::path::PathBuf,
    scanned: Option<(Signature, Result<Stamp, String>)>,
}

impl Installation {
    pub fn at(path: std::path::PathBuf) -> Self {
        Installation {
            path,
            scanned: None,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn look(&mut self, running: Stamp) -> Install {
        self.look_with(running, |path| std::fs::read(path))
    }

    fn look_with(
        &mut self,
        running: Stamp,
        read: impl FnOnce(&std::path::Path) -> std::io::Result<Vec<u8>>,
    ) -> Install {
        let shown = self.path.display();
        let before = match signature(&self.path) {
            Ok(signature) => signature,
            Err(e) => return Install::Unavailable(format!("{shown}: {e}")),
        };
        let found = match &self.scanned {
            Some((signature, found)) if *signature == before => found.clone(),
            _ => {
                let bytes = match read(&self.path) {
                    Ok(bytes) => bytes,
                    Err(e) => return Install::Unavailable(format!("cannot read {shown}: {e}")),
                };
                if signature(&self.path).ok() != Some(before) {
                    return Install::Unavailable(format!("{shown} changed while it was read"));
                }
                let found = find(&bytes);
                self.scanned = Some((before, found.clone()));
                found
            }
        };
        match found {
            Ok(stamp) if stamp == running => Install::Current,
            Ok(stamp) => Install::Replaced(stamp),
            Err(why) => Install::Unavailable(format!("{shown}: {why}")),
        }
    }
}

/// Where a rendezvous path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `CLANK_INSTALL_PATH`.
    Override,
    /// The spelling clank was invoked by.
    Invocation,
    /// `current_exe()`, which may be a resolved target a retargeted
    /// launcher would not move.
    Fallback,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Override => "override",
            Source::Invocation => "invocation",
            Source::Fallback => "fallback",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Source::Override, Source::Invocation, Source::Fallback]
            .into_iter()
            .find(|source| source.as_str() == s)
    }
}

/// The path clank is installed at, as this process knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendezvous {
    pub path: std::path::PathBuf,
    pub source: Source,
}

/// The installation path: the override, else the spelling clank was
/// invoked by (joined to `cwd`, or found on `path` as exec would, with
/// symlinks kept), else `current_exe`.
pub fn rendezvous_of(
    override_path: Option<std::ffi::OsString>,
    argv0: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    current_exe: impl FnOnce() -> std::io::Result<std::path::PathBuf>,
) -> Option<Rendezvous> {
    if let Some(given) = override_path {
        return Some(Rendezvous {
            path: given.into(),
            source: Source::Override,
        });
    }
    let invoked = argv0.and_then(|argv0| {
        if argv0.as_encoded_bytes().contains(&b'/') {
            Some(cwd.join(argv0)).filter(|p| executable(p))
        } else {
            std::env::split_paths(path?)
                .map(|dir| cwd.join(dir).join(argv0))
                .find(|candidate| executable(candidate))
        }
    });
    if let Some(path) = invoked {
        return Some(Rendezvous {
            path,
            source: Source::Invocation,
        });
    }
    current_exe().ok().map(|path| Rendezvous {
        path,
        source: Source::Fallback,
    })
}

/// Whether exec would run `path`: a file this process may execute.
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path; access has no memory effects.
    path.is_file() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
}

/// This process's installation path.
pub fn rendezvous() -> Option<Rendezvous> {
    let cwd = std::env::current_dir().unwrap_or_default();
    rendezvous_of(
        std::env::var_os("CLANK_INSTALL_PATH"),
        std::env::args_os().next().as_deref(),
        &cwd,
        std::env::var_os("PATH").as_deref(),
        std::env::current_exe,
    )
}

/// The bytes of an executable carrying `stamp`, for fixtures.
#[cfg(test)]
pub fn executable_with(stamp: Stamp) -> Vec<u8> {
    let mut bytes = b"preamble".to_vec();
    bytes.extend_from_slice(&embedded()[..MARKER_LEN]);
    bytes.extend_from_slice(&stamp.0);
    bytes.extend_from_slice(&END);
    bytes.extend_from_slice(b"trailer");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    mod inputs {
        include!("../build_stamp_inputs.rs");

        #[test]
        fn the_closure_covers_every_linked_crate_and_embedded_asset() {
            let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .unwrap();
            let files = stamp_files(workspace);
            for want in [
                "crates/web/assets/page.html",
                "crates/web/assets/vendor/plate.woff2",
                "crates/web/src/assets.rs",
                "crates/web-cloudflare/src/lib.rs",
                "crates/core/src/lib.rs",
                "crates/cli/build.rs",
                "crates/cli/src/build_stamp.rs",
            ] {
                assert!(
                    files.iter().any(|f| f == std::path::Path::new(want)),
                    "{want} is not a stamp input"
                );
            }
            assert!(
                files
                    .iter()
                    .all(|f| !f.components().any(|c| c.as_os_str() == "target")),
                "a build directory is an input"
            );
        }

        fn tree() -> tempfile::TempDir {
            let ws = tempfile::tempdir().unwrap();
            for (path, body) in [
                ("Cargo.toml", "[workspace]"),
                ("Cargo.lock", "lock"),
                ("crates/cli/src/main.rs", "fn main() {}"),
                ("crates/web/assets/page.html", "<html>"),
                ("crates/web/src/lib.rs", "pub fn f() {}"),
            ] {
                let path = ws.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, body).unwrap();
            }
            ws
        }

        fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        const CARGO: &[(&str, &str)] = &[
            ("PROFILE", "release"),
            ("TARGET", "aarch64-apple-darwin"),
            ("OPT_LEVEL", "3"),
            ("DEBUG", "false"),
            ("CARGO_ENCODED_RUSTFLAGS", ""),
            ("CARGO_CFG_PANIC", "unwind"),
            ("CARGO_FEATURE_DEFAULT", "1"),
            ("HOME", "/Users/someone"),
        ];

        /// Any input alone moves the stamp; what is not an input does not.
        #[test]
        fn each_input_alone_moves_the_stamp() {
            let ws = tree();
            let base = stamp_of(ws.path(), &env(CARGO), "rustc 1.98.0").unwrap();
            assert_eq!(
                base,
                stamp_of(ws.path(), &env(CARGO), "rustc 1.98.0").unwrap()
            );

            let changed = |edit: &dyn Fn(&std::path::Path)| {
                let ws = tree();
                edit(ws.path());
                stamp_of(ws.path(), &env(CARGO), "rustc 1.98.0").unwrap()
            };
            for (what, edit) in [
                (
                    "an embedded asset",
                    &(|ws: &std::path::Path| {
                        std::fs::write(ws.join("crates/web/assets/page.html"), "<html>!").unwrap()
                    }) as &dyn Fn(&std::path::Path),
                ),
                ("a path dependency's source", &|ws: &std::path::Path| {
                    std::fs::write(ws.join("crates/web/src/lib.rs"), "pub fn g() {}").unwrap()
                }),
                ("the lockfile", &|ws: &std::path::Path| {
                    std::fs::write(ws.join("Cargo.lock"), "lock2").unwrap()
                }),
                ("a workspace cargo config", &|ws: &std::path::Path| {
                    std::fs::create_dir_all(ws.join(".cargo")).unwrap();
                    std::fs::write(
                        ws.join(".cargo/config.toml"),
                        "[profile.release]\noverflow-checks = true",
                    )
                    .unwrap()
                }),
            ] {
                assert_ne!(changed(edit), base, "{what}");
            }
            assert_eq!(
                changed(&|ws| {
                    std::fs::create_dir_all(ws.join("crates/cli/target")).unwrap();
                    std::fs::write(ws.join("crates/cli/target/out"), "x").unwrap();
                    std::fs::create_dir_all(ws.join("crates/.hidden")).unwrap();
                    std::fs::write(ws.join("crates/.hidden/x"), "x").unwrap();
                }),
                base,
                "build output and hidden directories"
            );

            for (name, value) in [
                ("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}x"),
                ("OPT_LEVEL", "2"),
                ("CARGO_CFG_DEBUG_ASSERTIONS", ""),
                ("CARGO_PROFILE_DEV_OVERFLOW_CHECKS", "false"),
            ] {
                let mut with = env(CARGO);
                with.retain(|(n, _)| n != name);
                with.push((name.to_string(), value.to_string()));
                assert_ne!(
                    stamp_of(ws.path(), &with, "rustc 1.98.0").unwrap(),
                    base,
                    "{name}"
                );
            }
            let mut unrelated = env(CARGO);
            unrelated.push(("TERM".into(), "xterm".into()));
            assert_eq!(
                stamp_of(ws.path(), &unrelated, "rustc 1.98.0").unwrap(),
                base
            );
            assert_ne!(
                stamp_of(ws.path(), &env(CARGO), "rustc 1.99.0").unwrap(),
                base
            );
            assert!(stamp_rerun_vars().contains(&"CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS".into()));
        }

        /// A boundary moved between a name and its value is a different
        /// input, not the same bytes.
        #[test]
        fn the_framing_keeps_names_and_values_apart() {
            let ws = tree();
            let a = stamp_of(ws.path(), &env(&[("CARGO_FEATURE_AB", "C")]), "r").unwrap();
            let b = stamp_of(ws.path(), &env(&[("CARGO_FEATURE_A", "BC")]), "r").unwrap();
            assert_ne!(a, b);
        }
    }

    /// The test binary carries exactly one stamp, the one it is running.
    #[test]
    fn the_running_image_carries_its_stamp_once() {
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(find(&me), Ok(running()));
        assert_eq!(running(), Stamp(STAMP));
    }

    fn installed(dir: &std::path::Path, name: &str, stamp: Option<[u8; 16]>) -> std::path::PathBuf {
        let mut bytes = b"ELF-ish preamble".to_vec();
        if let Some(stamp) = stamp {
            bytes.extend_from_slice(&embedded()[..MARKER_LEN]);
            bytes.extend_from_slice(&stamp);
            bytes.extend_from_slice(&END);
        }
        bytes.extend_from_slice(b"trailer");
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn an_installation_is_current_replaced_or_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let me = running();
        let mut other = me.0;
        other[0] ^= 1;
        let look = |path: std::path::PathBuf| Installation::at(path).look(me);
        assert_eq!(
            look(installed(dir.path(), "same", Some(me.0))),
            Install::Current
        );
        assert_eq!(
            look(installed(dir.path(), "other", Some(other))),
            Install::Replaced(Stamp(other))
        );
        assert!(matches!(
            look(dir.path().join("missing")),
            Install::Unavailable(_)
        ));
        assert!(matches!(
            look(installed(dir.path(), "plain", None)),
            Install::Unavailable(_)
        ));

        let twice = installed(dir.path(), "twice", Some(me.0));
        let mut bytes = std::fs::read(&twice).unwrap();
        bytes.extend_from_slice(&embedded());
        std::fs::write(&twice, bytes).unwrap();
        assert!(matches!(look(twice), Install::Unavailable(why) if why.contains("more than one")));

        let cut = dir.path().join("cut");
        let mut bytes = b"x".to_vec();
        bytes.extend_from_slice(&embedded()[..MARKER_LEN + 8]);
        std::fs::write(&cut, bytes).unwrap();
        assert!(matches!(look(cut), Install::Unavailable(why) if why.contains("malformed")));

        let unreadable = installed(dir.path(), "unreadable", Some(me.0));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(matches!(look(unreadable), Install::Unavailable(_)));
    }

    /// A completed scan is read once, whatever it found; a changed file
    /// is read again; a failed read is retried though nothing changed.
    #[test]
    fn only_completed_scans_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let me = running();
        let reads = std::cell::Cell::new(0);
        let read = |path: &std::path::Path| {
            reads.set(reads.get() + 1);
            std::fs::read(path)
        };
        for stamp in [Some(me.0), None] {
            reads.set(0);
            let mut installation = Installation::at(installed(dir.path(), "x", stamp));
            installation.look_with(me, read);
            installation.look_with(me, read);
            assert_eq!(reads.get(), 1, "unchanged, stamped: {}", stamp.is_some());
        }

        reads.set(0);
        let path = installed(dir.path(), "y", Some(me.0));
        let mut installation = Installation::at(path.clone());
        installation.look_with(me, read);
        let mut other = me.0;
        other[1] ^= 1;
        let replacement = installed(dir.path(), "y.new", Some(other));
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(
            installation.look_with(me, read),
            Install::Replaced(Stamp(other))
        );
        assert_eq!(reads.get(), 2, "a rename is read again");

        let mut installation = Installation::at(installed(dir.path(), "z", Some(other)));
        let failed = installation.look_with(me, |_| Err(std::io::Error::other("busy")));
        assert!(matches!(failed, Install::Unavailable(_)));
        assert_eq!(
            installation.look_with(me, |p| std::fs::read(p)),
            Install::Replaced(Stamp(other)),
            "a failed read is tried again with nothing changed"
        );
    }

    /// The alias the user invoked, kept unresolved: retargeting it is a
    /// replacement, though `current_exe` named the old target.
    #[test]
    fn the_rendezvous_is_the_invoked_alias() {
        let root = tempfile::tempdir().unwrap();
        let me = running();
        let mut other = me.0;
        other[2] ^= 1;
        let versions = root.path().join("versions");
        std::fs::create_dir_all(&versions).unwrap();
        let a = installed(&versions, "a", Some(me.0));
        let b = installed(&versions, "b", Some(other));
        use std::os::unix::fs::PermissionsExt;
        for exe in [&a, &b] {
            std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let alias = bin.join("clank");
        std::os::unix::fs::symlink(&a, &alias).unwrap();

        let chosen = rendezvous_of(
            None,
            Some(std::ffi::OsStr::new("clank")),
            root.path(),
            Some(bin.as_os_str()),
            || Ok(a.clone()),
        )
        .unwrap();
        assert_eq!(
            chosen,
            Rendezvous {
                path: alias.clone(),
                source: Source::Invocation
            }
        );
        let mut installation = Installation::at(chosen.path);
        assert_eq!(installation.look(me), Install::Current);
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&b, &alias).unwrap();
        assert_eq!(installation.look(me), Install::Replaced(Stamp(other)));
        std::fs::remove_file(&alias).unwrap();
        assert!(matches!(installation.look(me), Install::Unavailable(_)));

        assert_eq!(
            rendezvous_of(
                Some("/opt/clank".into()),
                Some("clank".as_ref()),
                root.path(),
                None,
                || Ok(a.clone())
            )
            .map(|r| r.source),
            Some(Source::Override)
        );
        assert_eq!(
            rendezvous_of(None, Some("clank".as_ref()), root.path(), None, || Ok(
                a.clone()
            )),
            Some(Rendezvous {
                path: a.clone(),
                source: Source::Fallback
            })
        );
        assert_eq!(
            rendezvous_of(
                None,
                Some("./versions/a".as_ref()),
                root.path(),
                None,
                || Ok(b.clone())
            )
            .map(|r| r.source),
            Some(Source::Invocation)
        );
    }

    /// As exec would: a file on PATH this process may not execute is
    /// passed over for the one it would run, which is still named by its
    /// PATH entry, not resolved.
    #[test]
    fn the_invocation_is_found_as_exec_would() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let (first, second) = (root.path().join("first"), root.path().join("second"));
        for (dir, mode) in [(&first, 0o644), (&second, 0o755)] {
            std::fs::create_dir(dir).unwrap();
            let exe = installed(dir, "clank", Some(running().0));
            std::fs::set_permissions(exe, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = std::env::join_paths([&first, &second]).unwrap();
        let chosen = rendezvous_of(
            None,
            Some("clank".as_ref()),
            root.path(),
            Some(&path),
            || Err(std::io::Error::other("unused")),
        );
        assert_eq!(chosen.map(|r| r.path), Some(second.join("clank")));
    }
}
