use crate::build_stamp::{Install, Installation, Rendezvous, Source, Stamp, running};
use crate::cli::monitor::fixtures::*;

fn other() -> Stamp {
    let mut stamp = running().0;
    stamp[0] ^= 1;
    Stamp(stamp)
}

/// Records written before versions were embedded still read, with
/// no version rather than a guessed one.
#[test]
fn a_record_from_before_versions_reads_without_one() {
    let described: DescribedBuild = serde_json::from_str(
        r#"{"running":"ab","installed_at":"/bin/clank","source":"invocation"}"#,
    )
    .unwrap();
    assert_eq!(described.version, None);
    let observed: Observed =
        serde_json::from_str(r#"{"state":"replaced","installed":"ab"}"#).unwrap();
    assert_eq!(
        observed,
        Observed::Replaced {
            installed: "ab".into(),
            installed_version: None
        }
    );
}

fn install(path: &std::path::Path, stamp: Stamp) {
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, crate::build_stamp::executable_with(stamp, "9.9.9")).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

type Noted = std::sync::Arc<std::sync::Mutex<Vec<Install>>>;
type Note = Box<dyn Fn(&Install) + Send>;

fn noted() -> (Noted, Note) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let into = seen.clone();
    (
        seen,
        Box::new(move |i: &Install| into.lock().unwrap().push(i.clone())),
    )
}

fn counting() -> (
    std::rc::Rc<std::cell::Cell<u32>>,
    impl Fn() -> Result<(), String>,
) {
    let count = std::rc::Rc::new(std::cell::Cell::new(0));
    let c = count.clone();
    (count, move || {
        c.set(c.get() + 1);
        Ok(())
    })
}

fn repo() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A replaced build retires without repairing a lost lease or
/// republishing, and stays retired; an unavailable one keeps owning.
#[test]
fn a_replaced_build_retires_and_never_repairs() {
    let repo = repo();
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join("clank");
    install(&path, running());
    let (seen, note) = noted();
    let lease = acquire_lease(repo.path()).unwrap();
    let lease_path = lease.path().to_path_buf();
    let mut tenure = Tenure::new(lease).with_build(Built::new(
        crate::upgrade::Authority::watching(Installation::at(path.clone()), running()),
        path.clone(),
        running(),
        note,
    ));
    let (published, publish) = counting();
    assert!(tenure.may_act(&publish));

    std::fs::remove_file(&path).unwrap();
    assert!(
        tenure.may_act(&publish),
        "an unavailable installation keeps ownership"
    );

    install(&path, other());
    std::fs::remove_file(&lease_path).unwrap();
    assert_eq!(
        tenure.settle(&publish),
        Settled::Stale {
            running: running(),
            installed: other()
        }
    );
    assert!(
        !lease_path.exists(),
        "the lease was repaired by a stale build"
    );
    assert!(!tenure.may_act(&publish));
    assert_eq!(published.get(), 0, "a stale build advertised");
    let seen = seen.lock().unwrap();
    assert!(
        matches!(seen.as_slice(), [Install::Current, Install::Unavailable(_), Install::Replaced(s)] if s.stamp == other())
    );
}

/// A stale build stands down once nothing it handed out is still in
/// flight, or once the bound its hooks answer within has passed, and
/// the loop asks exactly that before every wake.
#[test]
fn a_stale_build_stands_down_when_nothing_is_in_flight() {
    let repo = repo();
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join("clank");
    install(&path, running());
    let authority = crate::upgrade::Authority::watching(Installation::at(path.clone()), running());
    let (_, note) = noted();
    let mut tenure = Tenure::new(acquire_lease(repo.path()).unwrap()).with_build(Built::new(
        authority.clone(),
        path.clone(),
        running(),
        note,
    ));
    let (_, publish) = counting();
    let now = std::time::Instant::now();
    assert!(tenure.may_act(&publish) && !tenure.standing_down(now));

    let presenting = authority.permit().expect("current");
    install(&path, other());
    assert!(!tenure.may_act(&publish), "stale at once");
    assert!(!tenure.standing_down(std::time::Instant::now()));
    assert!(
        tenure.standing_down(std::time::Instant::now() + QUIESCE_BUDGET),
        "a hook that never answers does not hold the monitor"
    );
    drop(presenting);
    assert!(tenure.standing_down(std::time::Instant::now()));

    let src = include_str!("mod.rs");
    let from = src.find("let ended = loop {").expect("the loop");
    assert!(
        src[from..].starts_with("let ended = loop {\n        if hold.standing_down() {"),
        "the loop's first question"
    );
    let lease = include_str!("lease.rs");
    let asked = &lease[lease
        .find("pub(super) fn standing_down(&self) -> bool {")
        .expect("the hold asks its tenure")..];
    assert!(
        asked[..asked.find("\n    }\n").unwrap()]
            .contains("self.tenure.standing_down(std::time::Instant::now())"),
        "{asked}"
    );
}

/// A starting monitor forgets the record its dead predecessor left,
/// and keeps one whose process lives.
#[test]
fn a_dead_predecessors_observation_is_forgotten() {
    let repo = repo();
    let record = |pid: i32, birth| Observation {
        pid,
        birth,
        running: running().to_string(),
        installed_at: "/bin/clank".into(),
        source: Source::Invocation,
        state: Observed::Current,
    };
    let predecessor = |pid: i32| {
        let d = Descriptor {
            pid,
            ..describe(repo.path(), pid, std::path::Path::new("/tmp/old.sock"))
        };
        publish(repo.path(), &d).unwrap();
    };

    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id() as i32;
    let birth = crate::proc_identity::token_for(dead);
    child.wait().unwrap();
    note_observation(repo.path(), &record(dead, birth)).unwrap();
    predecessor(dead);
    forget_dead_observation(repo.path());
    assert!(
        !observation_path(repo.path(), dead).exists(),
        "a dead one's"
    );

    let me = std::process::id() as i32;
    note_observation(
        repo.path(),
        &record(me, crate::proc_identity::token_for(me)),
    )
    .unwrap();
    predecessor(me);
    forget_dead_observation(repo.path());
    assert!(observation_path(repo.path(), me).exists(), "a live one's");
    let _ = std::fs::remove_file(observation_path(repo.path(), me));
}

/// The old monitor's last word lands under its own process, never on
/// the routing descriptor: a successor that took the repo after the
/// old one's last owner look keeps it.
#[test]
fn a_late_observation_cannot_take_routing_back() {
    let repo = repo();
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join("clank");
    install(&path, running());
    let old_pid = std::process::id() as i32;
    let note: Box<dyn Fn(&Install) + Send> = {
        let repo = repo.path().to_path_buf();
        Box::new(move |install: &Install| {
            note_observation(
                &repo,
                &Observation {
                    pid: old_pid,
                    birth: crate::proc_identity::token_for(old_pid),
                    running: running().to_string(),
                    installed_at: "/bin/clank".into(),
                    source: Source::Invocation,
                    state: install.into(),
                },
            )
            .unwrap()
        })
    };
    let lease = acquire_lease(repo.path()).unwrap();
    let lease_path = lease.path().to_path_buf();
    let mut tenure = Tenure::new(lease).with_build(Built::new(
        crate::upgrade::Authority::watching(Installation::at(path.clone()), running()),
        path.clone(),
        running(),
        note,
    ));
    let (_, owner_publish) = counting();
    assert!(
        tenure.may_act(&owner_publish),
        "the old monitor's last owner look"
    );

    std::fs::remove_file(&lease_path).unwrap();
    let _successor = acquire_lease(repo.path()).unwrap();
    let successor = Descriptor {
        pid: 999_999,
        ..describe(
            repo.path(),
            999_999,
            std::path::Path::new("/tmp/successor.sock"),
        )
    };
    publish(repo.path(), &successor).unwrap();
    install(&path, other());
    assert!(
        !tenure.may_act(&counting().1),
        "a stale build acted as owner"
    );

    assert_eq!(
        read_published(repo.path()),
        Some(successor),
        "routing was taken back"
    );
    let observed: Observation =
        serde_json::from_slice(&std::fs::read(observation_path(repo.path(), old_pid)).unwrap())
            .unwrap();
    assert_eq!(
        observed.state,
        Observed::Replaced {
            installed: other().to_string(),
            installed_version: Some("9.9.9".into()),
        }
    );
    let _ = std::fs::remove_file(observation_path(repo.path(), old_pid));
}

/// Running only for a live pid whose recorded birth, where it has
/// one, still holds; and a published descriptor never reads as
/// nothing published.
#[test]
fn liveness_is_whether_the_published_process_lives() {
    let repo = repo();
    let me = std::process::id() as i32;
    // Above every platform's pid limit, so no process holds it.
    let dead = i32::MAX;
    let socket = std::path::Path::new("/tmp/x.sock");
    let now = || liveness(repo.path());
    assert_eq!(now(), Liveness::None);
    assert_eq!(now().line(), "none");

    publish(repo.path(), &describe(repo.path(), me, socket)).unwrap();
    assert_eq!(
        now(),
        Liveness::Running {
            pid: me,
            build: None,
            observed: None
        }
    );

    publish(repo.path(), &describe(repo.path(), dead, socket)).unwrap();
    assert_eq!(now(), Liveness::Gone { pid: dead });
    assert_eq!(now().line(), format!("gone (pid {dead} exited)"));

    // `me` stands in for a reused pid: alive, but not the process
    // that wrote the record under that number.
    publish(repo.path(), &describe(repo.path(), me, socket)).unwrap();
    note_observation(
        repo.path(),
        &Observation {
            pid: me,
            birth: Some(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 0 }),
            running: running().to_string(),
            installed_at: "/bin/clank".into(),
            source: Source::Invocation,
            state: Observed::Current,
        },
    )
    .unwrap();
    assert_eq!(
        now(),
        Liveness::Gone { pid: me },
        "a stranger is not Running"
    );
    let _ = std::fs::remove_file(observation_path(repo.path(), me));

    std::fs::write(
        runtime_path(repo.path(), "json"),
        format!(r#"{{"proto":3,"pid":{me},"repo":"/w","socket":"/tmp/old.sock"}}"#),
    )
    .unwrap();
    assert_eq!(
        now(),
        Liveness::Running {
            pid: me,
            build: None,
            observed: None
        },
        "a legacy descriptor's pid is asked too"
    );
    assert_eq!(now().line(), format!("pid {me}, build unknown (legacy)"));
    std::fs::write(
        runtime_path(repo.path(), "json"),
        format!(r#"{{"proto":3,"pid":{dead}}}"#),
    )
    .unwrap();
    assert_eq!(now(), Liveness::Gone { pid: dead });

    std::fs::write(runtime_path(repo.path(), "json"), r#"{"proto":3}"#).unwrap();
    assert!(
        matches!(now(), Liveness::Unknown(_)),
        "published without a pid is neither absent nor running: {:?}",
        now()
    );
    assert!(now().line().starts_with("unknown: "), "{}", now().line());
    let _ = std::fs::remove_file(runtime_path(repo.path(), "json"));
}

/// Status joins the routed monitor to its own observation record,
/// and believes the record only of the living process it names.
#[test]
fn status_shows_the_routed_monitors_build() {
    let repo = repo();
    let me = std::process::id() as i32;
    let build = DescribedBuild {
        running: running().to_string(),
        version: None,
        installed_at: "/bin/clank".into(),
        source: Source::Invocation,
    };
    publish(
        repo.path(),
        &Descriptor {
            build: Some(build.clone()),
            ..describe(repo.path(), me, std::path::Path::new("/tmp/x.sock"))
        },
    )
    .unwrap();
    let observe = |state: Observed, birth| {
        note_observation(
            repo.path(),
            &Observation {
                pid: me,
                birth,
                running: build.running.clone(),
                installed_at: build.installed_at.clone(),
                source: build.source,
                state,
            },
        )
        .unwrap()
    };
    let line = || liveness(repo.path()).line();
    assert!(line().ends_with("not yet observed"), "{}", line());
    observe(Observed::Current, crate::proc_identity::token_for(me));
    assert!(line().ends_with(": current"), "{}", line());
    observe(
        Observed::Replaced {
            installed: other().to_string(),
            installed_version: None,
        },
        crate::proc_identity::token_for(me),
    );
    assert!(line().contains("retiring"), "{}", line());
    observe(
        Observed::Unavailable { why: "gone".into() },
        crate::proc_identity::token_for(me),
    );
    assert!(
        line().contains("install path unavailable: gone"),
        "{}",
        line()
    );
    observe(Observed::Current, None);
    assert!(
        line().ends_with("not yet observed"),
        "a record with no birth is not believed"
    );

    publish(
        repo.path(),
        &describe(repo.path(), me, std::path::Path::new("/tmp/x.sock")),
    )
    .unwrap();
    assert!(line().contains("unknown (legacy)"), "{}", line());
    let _ = std::fs::remove_file(observation_path(repo.path(), me));
    assert_eq!(
        describe_build(
            1,
            Some(&DescribedBuild {
                source: Source::Fallback,
                ..build
            }),
            None
        ),
        format!(
            "pid 1, build {} at /bin/clank (fallback path): not yet observed",
            running()
        )
    );
}

/// The successor is started from the same path it will compare
/// against, and told so.
#[test]
fn a_monitor_is_started_from_its_rendezvous() {
    let command = monitor_command(
        &Rendezvous {
            path: "/opt/bin/clank".into(),
            source: Source::Invocation,
        },
        std::path::Path::new("/w/repo"),
    );
    assert_eq!(command.get_program(), "/opt/bin/clank");
    let args: Vec<_> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "monitor",
            "start",
            "--repo",
            "/w/repo",
            "--installed-at",
            "/opt/bin/clank",
            "--installed-from",
            "invocation"
        ]
    );
}

/// A proto-12 client is still refused by this build, so it works
/// alone rather than being served by a monitor it cannot read.
#[test]
fn an_older_client_is_declined() {
    let repo = repo();
    let hello = format!(
        "{{\"proto\":12,\"repo\":\"{}\",\"who\":{{\"as\":\"observer\"}}}}",
        repo.path().display()
    );
    assert!(matches!(
        greeting_of(&hello, repo.path()),
        Greeting::Decline(Welcome::Decline { proto: PROTO, .. })
    ));
}
