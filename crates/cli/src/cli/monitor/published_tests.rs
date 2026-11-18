use crate::cli::monitor::fixtures::*;

/// The descriptor is read VERSION FIRST, so a monitor of another
/// version is a candidate rather than an absence.
///
/// Serde parses a struct whole: a body whose shape changed fails
/// before its `proto` can be read, and the reader cannot then tell
/// "something is here that I cannot serve" from "nothing is here".
/// The first is a holder to replace, the second a reason to start
/// one, and a successor that confused them would spawn against a
/// live monitor for ever.
#[test]
fn a_descriptor_of_another_version_is_a_holder_not_an_absence() {
    let repo = repo_with_roster();
    assert_eq!(published(repo.path()), Published::Nothing, "nothing yet");

    let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/x.sock"));
    publish(repo.path(), &d).unwrap();
    assert_eq!(published(repo.path()), Published::Ours(d));

    // A shape this build does not share, carrying a number it can.
    std::fs::write(
        runtime_path(repo.path(), "json"),
        r#"{"proto":3,"pid":7,"repo":"/w","socket":"/tmp/old.sock","fold":{"tick":1,"at_ms":2}}"#,
    )
    .unwrap();
    assert_eq!(
        published(repo.path()),
        Published::Other {
            proto: 3,
            pid: Some(7),
            socket: Some("/tmp/old.sock".to_string()),
        },
        "an older monitor is something to replace, not nothing"
    );
    assert_eq!(
        read_published(repo.path()),
        None,
        "and an ordinary client still refuses it"
    );

    // Our own number on a body we cannot read is also not an
    // absence: something is there and it is not what it says.
    std::fs::write(
        runtime_path(repo.path(), "json"),
        format!(r#"{{"proto":{PROTO},"wrong":"shape"}}"#),
    )
    .unwrap();
    assert_eq!(
        published(repo.path()),
        Published::Other {
            proto: PROTO,
            pid: None,
            socket: None
        }
    );
}

fn descriptor_for(repo: &std::path::Path) -> Descriptor {
    Descriptor {
        proto: PROTO,
        pid: 4242,
        repo: repo.to_string_lossy().to_string(),
        socket: "/tmp/clank-socks-501/4242.sock".into(),
        build: None,
    }
}

#[test]
fn a_published_descriptor_reads_back() {
    let repo = repo_dir();
    assert_eq!(read_published(repo.path()), None, "nothing published yet");

    let d = descriptor_for(repo.path());
    publish(repo.path(), &d).unwrap();
    assert_eq!(read_published(repo.path()), Some(d));
}

/// Rename, not truncate-in-place.
///
/// A shorter descriptor replacing a longer one is where an
/// in-place write shows: it leaves the predecessor's tail behind
/// and the reader gets trailing bytes after valid JSON. And the
/// temp file must not survive, or a stale one accumulates per
/// publish.
#[test]
fn publishing_replaces_a_longer_descriptor_wholly() {
    let repo = repo_dir();
    let mut long = descriptor_for(repo.path());
    long.socket = format!("/tmp/{}/4242.sock", "d".repeat(60));
    publish(repo.path(), &long).unwrap();

    let short = descriptor_for(repo.path());
    publish(repo.path(), &short).unwrap();

    assert_eq!(read_published(repo.path()), Some(short));
    let strays: Vec<_> = std::fs::read_dir(repo.path().join(".clank"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(strays.is_empty(), "a temp file survived: {strays:?}");
}
