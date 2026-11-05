//! `book/install.sh` against a local fixture server standing in for
//! GitHub: releases packed by the real release scripts from stand-in
//! binaries, and a `releases/latest` API answer. The script runs under
//! `sh` with only the test-only overrides it documents; nothing real is
//! downloaded or installed.

use crate::common::crates_dir;
use crate::release_workflow::{TARGETS, fake_bins, pack, run_script};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

/// Only the latest-lookup test fetches this release, so its requests
/// can be told apart on the shared server.
const LATEST: &str = "1.0.0";
const PINNED: &str = "0.9.0";
/// Its x86_64 Linux archive was replaced after SHA256SUMS was written.
const TAMPERED: &str = "0.8.0";
/// Its SHA256SUMS lacks the x86_64 Linux archive.
const UNSUMMED: &str = "0.7.0";

/// How each platform `uname` reports maps to a release target.
const PLATFORMS: [(&str, &str, &str); 5] = [
    ("Darwin", "arm64", "aarch64-apple-darwin"),
    ("Darwin", "x86_64", "x86_64-apple-darwin"),
    ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
    ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
    ("Linux", "arm64", "aarch64-unknown-linux-musl"),
];

struct Releases {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    _root: tempfile::TempDir,
}

/// Serves files under `root` by request path, 404 otherwise, recording
/// each path asked for.
fn serve(root: PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                header.clear();
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            seen.lock().unwrap().push(path.clone());
            let file = root.join(path.trim_start_matches('/'));
            let reply = match (path.contains(".."), std::fs::read(&file)) {
                (false, Ok(body)) => (200, body),
                _ => (404, b"not found".to_vec()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.0,
                reply.1.len()
            );
            let _ = stream.write_all(&reply.1);
        }
    });
    (base, requests)
}

fn release(root: &Path, version: &str) -> PathBuf {
    let dir = root.join(format!("releases/download/v{version}"));
    for (target, _) in TARGETS {
        let bins = root.join("bins").join(version).join(target);
        fake_bins(&bins, version, target);
        pack(&bins, version, target, &dir);
    }
    let (ok, err) = run_script("release-assets.sh", &[version, dir.to_str().unwrap()]);
    assert!(ok, "release-assets.sh {version}: {err}");
    dir
}

/// One fixture server for every test: it only ever serves.
fn releases() -> &'static Releases {
    static RELEASES: OnceLock<Releases> = OnceLock::new();
    RELEASES.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        release(root.path(), LATEST);
        release(root.path(), PINNED);

        let tampered = release(root.path(), TAMPERED);
        let bins = root.path().join("bins/tampered");
        fake_bins(&bins, "evil", "x86_64-unknown-linux-musl");
        pack(&bins, TAMPERED, "x86_64-unknown-linux-musl", &root.path().join("evil"));
        std::fs::rename(
            root.path()
                .join(format!("evil/clank-{TAMPERED}-x86_64-unknown-linux-musl.tar.gz")),
            tampered.join(format!("clank-{TAMPERED}-x86_64-unknown-linux-musl.tar.gz")),
        )
        .unwrap();

        let unsummed = release(root.path(), UNSUMMED);
        let sums = std::fs::read_to_string(unsummed.join("SHA256SUMS")).unwrap();
        let kept: String = sums
            .lines()
            .filter(|l| !l.ends_with("x86_64-unknown-linux-musl.tar.gz"))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(unsummed.join("SHA256SUMS"), kept).unwrap();

        std::fs::create_dir_all(root.path().join("api/releases")).unwrap();
        std::fs::write(
            root.path().join("api/releases/latest"),
            format!(
                "{{\n  \"url\": \"https://api.github.com/repos/LLFourn/clank/releases/1\",\n  \"tag_name\": \"v{LATEST}\",\n  \"name\": \"clank {LATEST}\",\n  \"draft\": false\n}}\n"
            ),
        )
        .unwrap();

        let (base, requests) = serve(root.path().to_path_buf());
        Releases {
            base,
            requests,
            _root: root,
        }
    })
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
    requests: Vec<String>,
}

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Runs the installer as `uname -s`/`-m` = `os`/`arch` with `home` as
/// HOME, `path` as PATH and `env` on top.
fn install(os: &str, arch: &str, home: &Path, path: &str, env: &[(&str, &str)]) -> Run {
    let r = releases();
    let mut cmd = Command::new("sh");
    cmd.arg(crates_dir().join("../book/install.sh"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("CLANK_RELEASE_BASE", format!("{}/releases", r.base))
        .env("CLANK_API_BASE", format!("{}/api", r.base))
        .env("CLANK_TEST_UNAME_S", os)
        .env("CLANK_TEST_UNAME_M", arch);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let before = r.requests.lock().unwrap().len();
    let out = cmd.output().expect("run sh book/install.sh");
    // Tests share the server, so another test's requests may interleave.
    let requests = r.requests.lock().unwrap()[before..].to_vec();
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        requests,
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn listing(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn assert_installed(dir: &Path, version: &str, target: &str) {
    assert_eq!(
        listing(dir),
        BTreeSet::from(["clank".to_string(), "clank-web-cloudflare".to_string()])
    );
    for bin in ["clank", "clank-web-cloudflare"] {
        let path = dir.join(bin);
        assert_eq!(
            read(&path),
            format!("#!/bin/sh\necho '{bin} {version} {target}'\n")
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "{bin}");
    }
}

#[test]
fn every_platform_installs_both_executables_of_its_own_archive() {
    let mapped: BTreeSet<&str> = PLATFORMS.iter().map(|(_, _, t)| *t).collect();
    let released: BTreeSet<&str> = TARGETS.iter().map(|(t, _)| *t).collect();
    assert_eq!(mapped, released);
    for (os, arch, target) in PLATFORMS {
        let home = tempfile::tempdir().unwrap();
        let run = install(
            os,
            arch,
            home.path(),
            SYSTEM_PATH,
            &[("CLANK_VERSION", PINNED)],
        );
        assert!(run.ok, "{os} {arch}: {}", run.stderr);
        assert_installed(&home.path().join(".local/bin"), PINNED, target);
    }
}

#[test]
fn latest_resolves_to_one_version_then_fetches_that_release() {
    let home = tempfile::tempdir().unwrap();
    let run = install("Linux", "x86_64", home.path(), SYSTEM_PATH, &[]);
    assert!(run.ok, "{}", run.stderr);
    assert_installed(
        &home.path().join(".local/bin"),
        LATEST,
        "x86_64-unknown-linux-musl",
    );
    let release = format!("/releases/download/v{LATEST}/");
    let mine: Vec<&String> = run
        .requests
        .iter()
        .filter(|p| *p == "/api/releases/latest" || p.starts_with(&release))
        .collect();
    assert_eq!(
        mine,
        [
            "/api/releases/latest".to_string(),
            format!("{release}clank-{LATEST}-x86_64-unknown-linux-musl.tar.gz"),
            format!("{release}SHA256SUMS"),
        ]
        .iter()
        .collect::<Vec<_>>()
    );
}

#[test]
fn a_pinned_version_installs_that_release() {
    for pin in [PINNED.to_string(), format!("v{PINNED}")] {
        let home = tempfile::tempdir().unwrap();
        let run = install(
            "Darwin",
            "arm64",
            home.path(),
            SYSTEM_PATH,
            &[("CLANK_VERSION", &pin)],
        );
        assert!(run.ok, "{pin}: {}", run.stderr);
        assert_installed(
            &home.path().join(".local/bin"),
            PINNED,
            "aarch64-apple-darwin",
        );
    }
}

#[test]
fn the_install_dir_is_honoured_and_named_when_off_path() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("my tools/bin");
    let run = install(
        "Linux",
        "aarch64",
        home.path(),
        SYSTEM_PATH,
        &[
            ("CLANK_VERSION", PINNED),
            ("CLANK_INSTALL_DIR", dir.to_str().unwrap()),
        ],
    );
    assert!(run.ok, "{}", run.stderr);
    assert_installed(&dir, PINNED, "aarch64-unknown-linux-musl");
    assert!(!home.path().join(".local").exists());
    assert!(
        run.stdout
            .contains(&format!("{} is not on your PATH", dir.display())),
        "{}",
        run.stdout
    );

    let on_path = format!("{SYSTEM_PATH}:{}", dir.display());
    let run = install(
        "Linux",
        "aarch64",
        home.path(),
        &on_path,
        &[
            ("CLANK_VERSION", PINNED),
            ("CLANK_INSTALL_DIR", dir.to_str().unwrap()),
        ],
    );
    assert!(run.ok, "{}", run.stderr);
    assert!(!run.stdout.contains("not on your PATH"), "{}", run.stdout);
}

fn old_install(home: &Path) -> PathBuf {
    let dir = home.join(".local/bin");
    std::fs::create_dir_all(&dir).unwrap();
    for bin in ["clank", "clank-web-cloudflare"] {
        std::fs::write(dir.join(bin), format!("old {bin}\n")).unwrap();
    }
    dir
}

fn assert_old_install_untouched(dir: &Path) {
    assert_eq!(
        listing(dir),
        BTreeSet::from(["clank".to_string(), "clank-web-cloudflare".to_string()])
    );
    for bin in ["clank", "clank-web-cloudflare"] {
        assert_eq!(read(&dir.join(bin)), format!("old {bin}\n"));
    }
}

#[test]
fn a_checksum_mismatch_installs_nothing_over_the_old_binaries() {
    let home = tempfile::tempdir().unwrap();
    let dir = old_install(home.path());
    let run = install(
        "Linux",
        "x86_64",
        home.path(),
        SYSTEM_PATH,
        &[("CLANK_VERSION", TAMPERED)],
    );
    assert!(!run.ok);
    assert!(
        run.stderr.contains(&format!(
            "clank-{TAMPERED}-x86_64-unknown-linux-musl.tar.gz does not match its SHA256SUMS entry"
        )),
        "{}",
        run.stderr
    );
    assert_old_install_untouched(&dir);
}

#[test]
fn an_archive_missing_from_the_checksums_installs_nothing() {
    let home = tempfile::tempdir().unwrap();
    let dir = old_install(home.path());
    let run = install(
        "Linux",
        "x86_64",
        home.path(),
        SYSTEM_PATH,
        &[("CLANK_VERSION", UNSUMMED)],
    );
    assert!(!run.ok);
    assert!(
        run.stderr.contains(&format!(
            "SHA256SUMS has no entry for clank-{UNSUMMED}-x86_64-unknown-linux-musl.tar.gz"
        )),
        "{}",
        run.stderr
    );
    assert_old_install_untouched(&dir);
}

#[test]
fn an_unsupported_platform_is_refused() {
    for (os, arch) in [
        ("FreeBSD", "amd64"),
        ("Linux", "i686"),
        ("MINGW64_NT-10.0", "x86_64"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let run = install(os, arch, home.path(), SYSTEM_PATH, &[]);
        assert!(!run.ok);
        assert!(
            run.stderr
                .contains(&format!("no prebuilt binary for {os} {arch}")),
            "{os} {arch}: {}",
            run.stderr
        );
        assert!(!home.path().join(".local").exists());
    }
}

#[test]
fn a_version_that_is_not_one_is_refused() {
    for pin in ["1.0.0/../../evil", "latest", "1..0", ".1.0"] {
        let home = tempfile::tempdir().unwrap();
        let run = install(
            "Linux",
            "x86_64",
            home.path(),
            SYSTEM_PATH,
            &[("CLANK_VERSION", pin)],
        );
        assert!(!run.ok);
        assert!(
            run.stderr.contains("is not a release version"),
            "{pin}: {}",
            run.stderr
        );
    }
}

#[test]
fn the_installer_and_release_scripts_pass_shellcheck() {
    let Ok(out) = Command::new("shellcheck").arg("--version").output() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must install shellcheck"
        );
        println!("SKIPPED: shellcheck is not installed");
        return;
    };
    assert!(out.status.success());
    let root = crates_dir().join("..");
    let out = Command::new("shellcheck")
        .arg("--shell=sh")
        .arg(root.join("book/install.sh"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = Command::new("shellcheck")
        .args([
            "scripts/release-check.sh",
            "scripts/release-archive.sh",
            "scripts/release-assets.sh",
            "scripts/release-notes.sh",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
