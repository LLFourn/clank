use super::*;
use std::collections::BTreeMap;
use std::sync::{Mutex, Weak};

/// One repo as the fake panel sees it.
#[derive(Default)]
struct Repo {
    exists: bool,
    version: u64,
    attaches: u32,
    probes: u32,
    builds: u32,
    wake: Option<Wake<Ring>>,
    path_wake: Option<Wake<Ring>>,
    fault: Fault,
    watching: Weak<()>,
}

#[derive(Clone, Default)]
struct World {
    repos: Arc<Mutex<BTreeMap<PathBuf, Repo>>>,
    log: Arc<Mutex<Vec<String>>>,
    /// A watch came or went since the last commit.
    pending: Arc<Mutex<bool>>,
    commits: Arc<Mutex<u32>>,
}

impl World {
    fn with(&self, repo: &str, f: impl FnOnce(&mut Repo)) {
        f(self
            .repos
            .lock()
            .unwrap()
            .entry(PathBuf::from(repo))
            .or_default())
    }

    fn exists(&self, repo: &str) {
        self.with(repo, |r| r.exists = true);
    }

    fn probes(&self, repo: &str) -> u32 {
        self.repos.lock().unwrap()[Path::new(repo)].probes
    }

    fn watching(&self, repo: &str) -> bool {
        self.repos.lock().unwrap()[Path::new(repo)]
            .watching
            .upgrade()
            .is_some()
    }

    fn wake(&self, repo: &str) -> Wake<Ring> {
        self.repos.lock().unwrap()[Path::new(repo)]
            .wake
            .clone()
            .expect("watched")
    }

    /// The repo's snapshot moves, and its watcher says so.
    fn change(&self, repo: &str) {
        self.with(repo, |r| r.version += 1);
        assert!(self.wake(repo).ring());
    }
}

/// The fake's snapshot: its repo's version.
struct Snap(u64);

impl Shown for Snap {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({"v": self.0})
    }

    fn human(&self, _paint: bool) -> String {
        format!("v {}", self.0)
    }
}

struct FakeWatch {
    fault: Fault,
    _alive: Arc<()>,
    pending: Arc<Mutex<bool>>,
}

impl Drop for FakeWatch {
    fn drop(&mut self) {
        *self.pending.lock().unwrap() = true;
    }
}

impl Watching for FakeWatch {
    fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }
}

enum FakePath {
    Fake,
    Real(PathWatch),
}

impl Watching for FakePath {
    fn fault(&self) -> Option<String> {
        match self {
            FakePath::Fake => None,
            FakePath::Real(watch) => watch.fault(),
        }
    }
}

/// A panel over [`World`]. With `real_paths`, a repo exists when its
/// directory holds `.git`, and its path is watched by the kernel.
struct Fake<S> {
    world: World,
    real_paths: bool,
    shown: fn(u64) -> S,
}

impl<S: Shown> Panel for Fake<S> {
    type Watch = FakeWatch;
    type PathWatch = FakePath;
    type Sig = u64;
    type Shown = S;

    fn watch(&self, repo: &Path, wake: Wake<Ring>, _git: &GitStatus) -> anyhow::Result<FakeWatch> {
        let real = self.real_paths && repo.join(".git").exists();
        let mut repos = self.world.repos.lock().unwrap();
        let r = repos.entry(repo.to_path_buf()).or_default();
        r.attaches += 1;
        if !(r.exists || real) {
            anyhow::bail!("`{}` is not a git repository", repo.display());
        }
        self.world
            .log
            .lock()
            .unwrap()
            .push(format!("watch {}", repo.display()));
        let alive = Arc::new(());
        r.watching = Arc::downgrade(&alive);
        r.wake = Some(wake);
        r.fault = Fault::default();
        *self.world.pending.lock().unwrap() = true;
        Ok(FakeWatch {
            fault: r.fault.clone(),
            _alive: alive,
            pending: self.world.pending.clone(),
        })
    }

    fn await_path(&mut self, repo: &Path, wake: Wake<Ring>) -> anyhow::Result<FakePath> {
        if self.real_paths {
            return Ok(FakePath::Real(watch_path(repo, wake)?));
        }
        self.world
            .repos
            .lock()
            .unwrap()
            .entry(repo.to_path_buf())
            .or_default()
            .path_wake = Some(wake);
        Ok(FakePath::Fake)
    }

    fn commit(&self) -> anyhow::Result<bool> {
        let restarted = std::mem::take(&mut *self.world.pending.lock().unwrap());
        if restarted {
            *self.world.commits.lock().unwrap() += 1;
        }
        Ok(restarted)
    }

    fn probe(&mut self, repo: &Path, _git: &GitStatus) -> anyhow::Result<u64> {
        self.world
            .log
            .lock()
            .unwrap()
            .push(format!("probe {}", repo.display()));
        let mut repos = self.world.repos.lock().unwrap();
        let r = repos.entry(repo.to_path_buf()).or_default();
        r.probes += 1;
        Ok(r.version)
    }

    async fn build(&mut self, repo: &Path, _git: &GitStatus) -> anyhow::Result<S> {
        let mut repos = self.world.repos.lock().unwrap();
        let r = repos.entry(repo.to_path_buf()).or_default();
        r.builds += 1;
        Ok((self.shown)(r.version))
    }
}

#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Out {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }

    /// The lines written since the last call, as JSON.
    fn take(&self) -> Vec<serde_json::Value> {
        let bytes = std::mem::take(&mut *self.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[derive(Debug, PartialEq)]
enum Resumed {
    AsStarted,
    Handoff(Handoff),
}

type Execs = Arc<Mutex<Vec<Resumed>>>;

type Watch<S> = StatusWatch<Fake<S>, Out, Box<dyn FnMut(Resume<'_>) -> std::io::Error>>;

struct Harness<S: Shown> {
    watch: Watch<S>,
    world: World,
    out: Out,
    execs: Execs,
}

fn harness_of<S: Shown>(
    wire: Wire,
    shown: fn(u64) -> S,
    real_paths: bool,
    authority: Authority,
    stdin: Stdin,
) -> Harness<S> {
    let world = World::default();
    let out = Out::default();
    let execs = Execs::default();
    let exec: Box<dyn FnMut(Resume<'_>) -> std::io::Error> = Box::new({
        let execs = execs.clone();
        move |resume| {
            execs.lock().unwrap().push(match resume {
                Resume::AsStarted => Resumed::AsStarted,
                Resume::Handoff(handoff) => Resumed::Handoff(handoff.clone()),
            });
            std::io::Error::other("no exec in a test")
        }
    });
    let watch = StatusWatch::new(
        Fake {
            world: world.clone(),
            real_paths,
            shown,
        },
        wire,
        false,
        out.clone(),
        authority,
        exec,
        stdin,
    )
    .unwrap();
    Harness {
        watch,
        world,
        out,
        execs,
    }
}

fn many() -> Harness<Snap> {
    harness_of(
        Wire::Many,
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Unread,
    )
}

fn key(path: &str) -> PathBuf {
    PathBuf::from(path)
}

impl<S: Shown> Harness<S> {
    fn add(&mut self, repo: &str) {
        self.watch.add(key(repo));
    }

    /// Wait as the loop does, rung after `budget` if nothing else rings,
    /// so a wake that never comes fails the test instead of hanging it.
    async fn wait_and_turn(&mut self, budget: Duration) {
        let fallback = Wake::new(self.watch.tx.clone(), Ring::Install);
        let (cancel, cancelled) = mpsc::channel::<()>();
        let guard = std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = cancelled.recv_timeout(budget) {
                fallback.ring();
            }
        });
        self.watch.wait().unwrap();
        drop(cancel);
        guard.join().unwrap();
        self.watch.turn().await.unwrap();
    }

    async fn settle(&mut self) {
        self.wait_and_turn(Duration::from_secs(5)).await
    }
}

fn line(repo: &str, v: u64) -> serde_json::Value {
    serde_json::json!({"repo": repo, "v": v})
}

fn errors(lines: &[serde_json::Value]) -> Vec<(&str, &str)> {
    lines
        .iter()
        .filter_map(|l| Some((l["repo"].as_str()?, l["error"].as_str()?)))
        .collect()
}

/// The one-repo watch writes, byte for byte, what it wrote before it
/// could watch many: each update whole, separated by a blank line, and
/// nothing when an update renders the same.
#[tokio::test]
async fn one_repo_writes_what_it_always_wrote() {
    fn shown(v: u64) -> StatusSnapshot {
        let mut snap = crate::cli::status::dirty_and_wake_tests::minimal_snapshot();
        if v >= 2 {
            snap.head_sha = Some("ef01234".into());
            snap.head_subject = Some("do the next thing".into());
        }
        snap
    }
    for (json, fixture) in [
        (
            true,
            include_str!("../../tests/fixtures/formats/status-watch-one-repo.jsonl"),
        ),
        (
            false,
            include_str!("../../tests/fixtures/formats/status-watch-one-repo.txt"),
        ),
    ] {
        let mut h = harness_of(
            Wire::One { json },
            shown,
            false,
            Authority::unwatched(),
            Stdin::Unread,
        );
        h.world.exists("/repo");
        h.add("/repo");
        h.watch.turn().await.unwrap();
        for _ in 0..2 {
            h.world.change("/repo");
            h.settle().await;
        }
        assert_eq!(h.world.probes("/repo"), 3, "each change was looked at");
        assert_eq!(
            String::from_utf8(h.out.bytes()).unwrap(),
            fixture,
            "json: {json}"
        );
    }
}

/// An event in one repo is work for that repo alone: only its snapshot
/// is looked at, and only its line is written, naming it.
#[tokio::test]
async fn an_event_in_one_repo_is_work_for_that_repo_alone() {
    let mut h = many();
    for repo in ["/w/a", "/w/b", "/w/c"] {
        h.world.exists(repo);
        h.add(repo);
    }
    h.watch.turn().await.unwrap();
    assert_eq!(
        h.out.take(),
        vec![line("/w/a", 0), line("/w/b", 0), line("/w/c", 0)]
    );
    h.world.change("/w/a");
    h.settle().await;
    assert_eq!(h.out.take(), vec![line("/w/a", 1)]);
    assert_eq!(
        ["/w/a", "/w/b", "/w/c"].map(|r| h.world.probes(r)),
        [2, 1, 1],
        "the quiet repos were looked at"
    );
}

/// With no event, no repo's snapshot is looked at, an install's wake
/// included, and the loop has no timer of its own to wake it.
#[tokio::test]
async fn idle_repos_cost_nothing() {
    let mut h = many();
    for repo in ["/w/a", "/w/b"] {
        h.world.exists(repo);
        h.add(repo);
    }
    h.watch.turn().await.unwrap();
    h.out.take();
    for _ in 0..3 {
        h.watch.turn().await.unwrap();
    }
    assert!(Wake::new(h.watch.tx.clone(), Ring::Install).ring());
    h.settle().await;
    assert_eq!(h.watch.timer(), None);
    assert_eq!([h.world.probes("/w/a"), h.world.probes("/w/b")], [1, 1]);
    assert!(h.out.take().is_empty());
}

/// Stdin adds and removes repos while the watch runs: an added repo is
/// watched and shown, and a removed one's watchers stop and nothing more
/// is written for it, whatever they had queued.
#[tokio::test]
async fn stdin_adds_and_removes_repos() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let mut h = harness_of(
        Wire::Many,
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Open {
            fd: reader.into(),
            partial: Vec::new(),
        },
    );
    for repo in ["/w/a", "/w/b", "/w/c"] {
        h.world.exists(repo);
    }
    h.add("/w/a");
    h.add("/w/b");
    h.watch.turn().await.unwrap();
    assert_eq!(h.out.take(), vec![line("/w/a", 0), line("/w/b", 0)]);

    writer.write_all(b"+/w/c\n").unwrap();
    h.settle().await;
    assert_eq!(h.out.take(), vec![line("/w/c", 0)]);

    let b = h.world.wake("/w/b");
    writer.write_all(b"-/w/b\n").unwrap();
    h.settle().await;
    assert!(!h.world.watching("/w/b"), "its watchers stopped");
    let looked = h.world.probes("/w/b");
    h.world.with("/w/b", |r| r.version += 1);
    assert!(b.ring());
    h.settle().await;
    assert!(h.out.take().is_empty());
    assert_eq!(h.world.probes("/w/b"), looked);
}

/// A repo removed and added again is a new member: what the old one's
/// watchers had queued is dropped, and only its own wakes count.
#[tokio::test]
async fn a_removed_repos_queued_wakes_are_dropped_once_it_is_back() {
    let mut h = many();
    h.world.exists("/w/b");
    h.add("/w/b");
    h.watch.turn().await.unwrap();
    h.out.take();
    let old = h.world.wake("/w/b");
    h.watch.remove(Path::new("/w/b"));
    h.add("/w/b");
    h.watch.turn().await.unwrap();
    assert_eq!(h.out.take(), vec![line("/w/b", 0)], "shown afresh");
    assert_eq!(h.world.probes("/w/b"), 2);

    assert!(old.ring());
    h.settle().await;
    assert_eq!(h.world.probes("/w/b"), 2, "the old member's wake did work");
    h.world.change("/w/b");
    h.settle().await;
    assert_eq!(h.world.probes("/w/b"), 3);
    assert_eq!(h.out.take(), vec![line("/w/b", 1)]);
}

/// A repo that cannot be watched is one error line, the others carry
/// on, it is tried again when its path changes, and once it exists,
/// removing it and adding it back starts it.
#[tokio::test]
async fn a_repo_that_cannot_be_watched_is_one_error_line_and_the_others_carry_on() {
    let mut h = many();
    h.world.exists("/w/a");
    h.add("/w/a");
    h.add("/w/x");
    h.watch.turn().await.unwrap();
    let lines = h.out.take();
    assert!(lines.contains(&line("/w/a", 0)), "{lines:?}");
    assert_eq!(
        errors(&lines),
        vec![("/w/x", "`/w/x` is not a git repository")]
    );
    assert_eq!(lines.len(), 2);

    h.world.change("/w/a");
    h.settle().await;
    assert_eq!(h.out.take(), vec![line("/w/a", 1)]);

    let path = h.world.repos.lock().unwrap()[Path::new("/w/x")]
        .path_wake
        .clone()
        .expect("its path is watched");
    assert!(path.ring());
    h.settle().await;
    assert!(h.out.take().is_empty(), "the same error is not said again");
    assert_eq!(
        h.world.repos.lock().unwrap()[Path::new("/w/x")].attaches,
        4,
        "tried before and after its path was watched, and again on the change"
    );

    h.watch.remove(Path::new("/w/x"));
    h.world.exists("/w/x");
    h.add("/w/x");
    h.watch.turn().await.unwrap();
    assert_eq!(h.out.take(), vec![line("/w/x", 0)]);
}

/// A watcher that fails in one repo makes that repo an error line and
/// leaves the others watched; a change to its path starts it again.
#[tokio::test]
async fn a_watcher_fault_in_one_repo_leaves_the_others_watched() {
    let mut h = many();
    for repo in ["/w/a", "/w/b"] {
        h.world.exists(repo);
        h.add(repo);
    }
    h.watch.turn().await.unwrap();
    h.out.take();

    let fault = h.world.repos.lock().unwrap()[Path::new("/w/a")]
        .fault
        .clone();
    crate::fs_notice::record(&fault, "the stream died".into());
    assert!(h.world.wake("/w/a").ring());
    h.settle().await;
    assert_eq!(errors(&h.out.take()), vec![("/w/a", "the stream died")]);
    assert!(!h.world.watching("/w/a"), "its watchers stopped");

    h.world.change("/w/b");
    h.settle().await;
    assert_eq!(h.out.take(), vec![line("/w/b", 1)]);

    let path = h.world.repos.lock().unwrap()[Path::new("/w/a")]
        .path_wake
        .clone()
        .unwrap();
    assert!(path.ring());
    h.settle().await;
    assert_eq!(h.out.take(), vec![line("/w/a", 0)]);
    assert!(h.world.watching("/w/a"));
}

/// Watching one repo, a watcher's fault and a repo that cannot be
/// watched end the watch, as they always did.
#[tokio::test]
async fn one_repo_ends_on_its_fault() {
    let mut h = harness_of(
        Wire::One { json: true },
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Unread,
    );
    h.add("/w/x");
    assert!(h.watch.turn().await.is_err());

    let mut h = harness_of(
        Wire::One { json: true },
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Unread,
    );
    h.world.exists("/w/a");
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    let fault = h.world.repos.lock().unwrap()[Path::new("/w/a")]
        .fault
        .clone();
    crate::fs_notice::record(&fault, "the stream died".into());
    assert!(h.world.wake("/w/a").ring());
    h.watch.wait().unwrap();
    let ended = h.watch.turn().await.unwrap_err();
    assert!(ended.to_string().contains("the stream died"), "{ended}");
}

/// A repo whose path does not exist yet is watched for through its
/// nearest existing ancestor, and starts when it appears.
#[tokio::test]
async fn an_initially_absent_repo_appearing_is_watched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let absent = root.join("later/repo");
    let mut h = harness_of(
        Wire::Many,
        Snap,
        true,
        Authority::unwatched(),
        Stdin::Unread,
    );
    let name = absent.to_str().unwrap();
    h.add(name);
    h.watch.turn().await.unwrap();
    assert_eq!(errors(&h.out.take()).len(), 1);

    let staged = root.join("staged");
    std::fs::create_dir_all(staged.join("repo/.git")).unwrap();
    std::fs::rename(&staged, root.join("later")).unwrap();
    h.wait_and_turn(Duration::from_secs(10)).await;
    assert_eq!(h.out.take(), vec![line(name, 0)]);
}

/// Each member's watchers are in place before its first snapshot is
/// looked at, when it is first added and when it is added again.
#[tokio::test]
async fn a_repo_is_watched_before_it_is_looked_at() {
    let mut h = many();
    h.world.exists("/w/a");
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    h.watch.remove(Path::new("/w/a"));
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    assert_eq!(
        *h.world.log.lock().unwrap(),
        ["watch /w/a", "probe /w/a", "watch /w/a", "probe /w/a"]
    );
}

/// The line format is chosen at start: a set that shrinks to one repo,
/// or to none and back, still writes one JSON line per repo naming it.
#[tokio::test]
async fn a_set_that_shrinks_keeps_its_line_format() {
    let mut h = many();
    for repo in ["/w/a", "/w/b"] {
        h.world.exists(repo);
        h.add(repo);
    }
    h.watch.turn().await.unwrap();
    h.out.take();
    h.watch.remove(Path::new("/w/b"));
    h.world.change("/w/a");
    h.settle().await;
    h.watch.remove(Path::new("/w/a"));
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    assert_eq!(
        String::from_utf8(h.out.bytes()).unwrap(),
        "{\"repo\":\"/w/a\",\"v\":1}\n{\"repo\":\"/w/a\",\"v\":1}\n"
    );
}

/// Every spelling of one repo is one member.
#[tokio::test]
async fn spellings_of_one_repo_are_one_member() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("real/repo")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let real = root.join("real/repo");
    for spelling in [
        root.join("link/repo"),
        root.join("real/./repo/"),
        root.join("real/other/../repo"),
    ] {
        assert_eq!(repo_key(&spelling).unwrap(), real, "{}", spelling.display());
    }
    assert_eq!(
        repo_key(&root.join("link/later/../absent")).unwrap(),
        root.join("real/absent"),
        "a repo not made yet keeps the name it will have"
    );

    let mut h = many();
    h.world.exists(real.to_str().unwrap());
    let (reader, mut writer) = std::io::pipe().unwrap();
    h.watch.control = Control::Open(
        Reader::start(
            reader.into(),
            Vec::new(),
            Wake::new(h.watch.tx.clone(), Ring::Stdin),
        )
        .unwrap(),
    );
    writeln!(writer, "{}", root.join("link/repo").display()).unwrap();
    writeln!(writer, "+{}", real.display()).unwrap();
    h.settle().await;
    assert_eq!(h.watch.members.len(), 1);
    assert_eq!(h.out.take(), vec![line(real.to_str().unwrap(), 0)]);
}

#[test]
fn a_repo_line_adds_removes_or_says_nothing() {
    assert_eq!(parse_line(b"+/w/a"), Some(Ok(Change::Add("/w/a".into()))));
    assert_eq!(
        parse_line(b"-/w/a\r"),
        Some(Ok(Change::Remove("/w/a".into())))
    );
    assert_eq!(parse_line(b" /w/a "), Some(Ok(Change::Add("/w/a".into()))));
    assert_eq!(parse_line(b"   "), None);
    assert!(matches!(parse_line(b"+/w/\xff"), Some(Err(_))));
    assert_eq!(
        parse_line("项目/repo".as_bytes()),
        Some(Ok(Change::Add("项目/repo".into())))
    );
    assert_eq!(
        parse_line("-项目/repo".as_bytes()),
        Some(Ok(Change::Remove("项目/repo".into())))
    );
    assert!(matches!(parse_line(b"- "), Some(Err(_))));
}

/// Stdin read up to the exec leaves the start of a line with the
/// watch, and a last line with no newline still counts at the end.
#[test]
fn a_line_half_read_is_kept_and_a_last_one_counts() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let (tx, rx) = mpsc::channel();
    let mut stdin =
        Reader::start(reader.into(), b"+/w".to_vec(), Wake::new(tx, Ring::Stdin)).unwrap();
    writer.write_all(b"/a\n-/w/b\n+/w/c").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Ring::Stdin));
    let (lines, ended) = stdin.read().unwrap();
    assert_eq!(lines, [b"+/w/a".to_vec(), b"-/w/b".to_vec()]);
    assert!(!ended);
    assert_eq!(stdin.partial, b"+/w/c");
    drop(writer);
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Ring::Stdin));
    assert_eq!(stdin.read().unwrap(), (vec![b"+/w/c".to_vec()], true));
}

/// An upgrade carries the set as it stands, not as argv had it: a repo
/// argv named and stdin removed stays removed, one stdin added stays,
/// and a line half read is finished by what stdin says next. The
/// installed build shows every repo once and goes on reading stdin.
#[tokio::test]
async fn an_upgrade_carries_the_set_as_it_stands_and_stdin_goes_on() {
    let installed = crate::upgrade::tests::Installed::current();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let mut old = harness_of(
        Wire::Many,
        Snap,
        false,
        installed.authority(),
        Stdin::Open {
            fd: reader.try_clone().unwrap().into(),
            partial: Vec::new(),
        },
    );
    for repo in ["/w/a", "/w/b", "/w/c", "/w/d", "/w/e"] {
        old.world.exists(repo);
    }
    old.add("/w/a");
    old.add("/w/b");
    old.watch.turn().await.unwrap();
    old.out.take();
    writer.write_all(b"-/w/a\n+/w/c\n+/w/d").unwrap();
    old.settle().await;
    assert_eq!(old.out.take(), vec![line("/w/c", 0)]);
    assert!(old.execs.lock().unwrap().is_empty());

    installed.replace(1);
    old.watch.turn().await.unwrap();
    let handoff = match old.execs.lock().unwrap().pop() {
        Some(Resumed::Handoff(handoff)) => handoff,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        handoff,
        Handoff::new(
            vec![key("/w/b"), key("/w/c")],
            HandedStdin::Open {
                partial: b"+/w/d".to_vec()
            }
        )
    );
    drop(old);

    let HandedStdin::Open { partial } = handoff.stdin else {
        unreachable!()
    };
    let mut new = harness_of(
        Wire::Many,
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Open {
            fd: reader.into(),
            partial,
        },
    );
    for repo in ["/w/b", "/w/c", "/w/d", "/w/e"] {
        new.world.exists(repo);
    }
    for repo in handoff.repos {
        new.watch.add(repo);
    }
    new.watch.turn().await.unwrap();
    assert_eq!(new.out.take(), vec![line("/w/b", 0), line("/w/c", 0)]);
    writer.write_all(b"\n+/w/e\n").unwrap();
    new.settle().await;
    assert_eq!(new.out.take(), vec![line("/w/d", 0), line("/w/e", 0)]);
    assert_eq!(
        new.watch.members.keys().cloned().collect::<Vec<_>>(),
        ["/w/b", "/w/c", "/w/d", "/w/e"].map(key)
    );
}

/// One repo's watch is restarted as it was started, and is looked for
/// even when nothing in its repo moved.
#[tokio::test]
async fn one_repo_is_restarted_as_it_was_started() {
    let installed = crate::upgrade::tests::Installed::current();
    let mut h = harness_of(
        Wire::One { json: true },
        Snap,
        false,
        installed.authority(),
        Stdin::Unread,
    );
    h.world.exists("/w/a");
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    installed.replace(1);
    h.watch.turn().await.unwrap();
    assert_eq!(*h.execs.lock().unwrap(), [Resumed::AsStarted]);
    assert_eq!(h.world.probes("/w/a"), 1);
}

/// The installed build is started as this watch was, less every flag
/// naming repos, with the handoff in their place; and those arguments
/// start it on exactly the handed-over set.
#[test]
fn the_installed_build_starts_on_the_handed_over_set() {
    let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
    let resumed = resumed_args(
        args(&[
            "status",
            "--watch",
            "-j",
            "--repo",
            "/w/a",
            "--repo=/w/b",
            "--repos-from",
            "-",
            "--handoff",
            "/run/old.json",
            "--no-cache",
        ]),
        Path::new("/run/new.json"),
    );
    assert_eq!(
        resumed,
        args(&[
            "status",
            "--watch",
            "-j",
            "--no-cache",
            "--handoff",
            "/run/new.json"
        ])
    );

    let dir = tempfile::tempdir().unwrap();
    let handoff = Handoff::new(vec![key("/w/b"), key("/w/c")], HandedStdin::Ended);
    let failed = handoff.hand_to(dir.path(), |path| {
        use clap::Parser as _;
        let mut argv = vec![OsString::from("clank")];
        argv.extend(resumed_args(resumed.clone(), path));
        let cli = crate::cli::command::Cli::try_parse_from(argv).unwrap();
        let crate::cli::command::Command::Status(args) = cli.command else {
            panic!("not status");
        };
        assert!(watches_many(&args));
        let (lines, stdin) = starting_set(&args).unwrap();
        assert_eq!(watched_from(&lines).0, [key("/w/b"), key("/w/c")]);
        assert!(matches!(stdin, Stdin::Ended));
        assert!(!path.exists(), "the reader removes what it read");
        std::io::Error::other("no exec in a test")
    });
    assert_eq!(failed.to_string(), "no exec in a test");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_handoff_of_another_format_is_refused_by_name() {
    let mut other = serde_json::to_value(Handoff::new(vec![], HandedStdin::Unread)).unwrap();
    other["v"] = 2.into();
    let why = Handoff::parse(other.to_string().as_bytes()).unwrap_err();
    assert!(why.to_string().contains("format 2"), "{why}");
    assert!(Handoff::parse(b"not json").is_err());
}

/// The repos a watch holds once it has taken `lines`, and the error
/// lines it wrote for them.
fn watched_from(lines: &[Vec<u8>]) -> (Vec<PathBuf>, Vec<serde_json::Value>) {
    let mut h = many();
    for line in lines {
        h.watch.command(line).unwrap();
    }
    (h.watch.members.keys().cloned().collect(), h.out.take())
}

/// Lines with paths in any script, and lines that name nothing a watch
/// can act on. Each of the latter is an error line naming it, and the
/// lines after it are still taken.
const LINES: &[u8] = b"/w/b\n\n+/w/c\n-/w/a\n\xe9\xa1\xb9\xe7\x9b\xae/repo\n+/w/\xe9\xa1\xb9/x\n-/w/\xe9\xa1\xb9/x\n+\xe9\xa1\xb9/y\n+\n\xff\n+/w/d\n";

fn expected_from_lines() -> Vec<PathBuf> {
    let cwd = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let mut keys = vec![
        key("/w/b"),
        key("/w/c"),
        key("/w/d"),
        cwd.join("项目/repo"),
        cwd.join("项/y"),
    ];
    keys.sort();
    keys
}

fn expected_refusals(errors: &[serde_json::Value]) {
    assert_eq!(
        errors,
        &[
            serde_json::json!({"repo": "+", "error": "the line names no repo"}),
            serde_json::json!({"repo": "\u{fffd}", "error": "a repo line must be UTF-8"}),
        ]
    );
}

/// A repo list in a file, after the `--repo`s: a bare path or `+PATH`
/// adds, `-PATH` removes, whatever script the path is in.
#[test]
fn a_repo_file_lists_the_starting_set() {
    use clap::Parser as _;
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("repos");
    std::fs::write(&list, LINES).unwrap();
    let cli = crate::cli::command::Cli::try_parse_from([
        "clank",
        "status",
        "--watch",
        "-j",
        "--repo",
        "/w/a",
        "--repos-from",
        list.to_str().unwrap(),
    ])
    .unwrap();
    let crate::cli::command::Command::Status(args) = cli.command else {
        panic!("not status");
    };
    let (lines, stdin) = starting_set(&args).unwrap();
    assert!(matches!(stdin, Stdin::Unread));
    let (watched, errors) = watched_from(&lines);
    assert_eq!(watched, expected_from_lines());
    expected_refusals(&errors);
}

/// The same lines on live stdin are taken the same way, and the watch
/// goes on reading after the ones it refuses.
#[tokio::test]
async fn stdin_takes_any_script_and_refuses_what_names_nothing() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    let mut h = harness_of(
        Wire::Many,
        Snap,
        false,
        Authority::unwatched(),
        Stdin::Open {
            fd: reader.into(),
            partial: Vec::new(),
        },
    );
    h.add("/w/a");
    h.watch.turn().await.unwrap();
    h.out.take();
    writer.write_all(LINES).unwrap();
    h.settle().await;
    let watched: Vec<PathBuf> = h.watch.members.keys().cloned().collect();
    assert_eq!(watched, expected_from_lines());
    let errors: Vec<serde_json::Value> = h
        .out
        .take()
        .into_iter()
        .filter(|l| {
            l["error"]
                .as_str()
                .is_some_and(|e| !e.contains("not a git repository"))
        })
        .collect();
    expected_refusals(&errors);
}

/// The timers a watch sets itself: a failed refresh's retry, 1s doubling
/// to 30s.
#[test]
fn a_failed_refresh_is_retried_on_a_backoff() {
    assert_eq!(watch_retry(0), None);
    assert_eq!(watch_retry(1), Some(Duration::from_secs(1)));
    assert_eq!(watch_retry(9), Some(Duration::from_secs(30)));
}

#[test]
fn initial_failure_is_fatal_post_success_degrades() {
    // codex 697cdb1: before the first successful snapshot there is
    // nothing to retain, so it fails loud, like the one-shot path.
    let err = watch_failure(false, "input probe", "boom").unwrap_err();
    assert!(err.to_string().contains("input probe"), "{err}");
    let err = watch_failure(false, "refresh", "boom").unwrap_err();
    assert!(err.to_string().contains("refresh"), "{err}");
    assert!(watch_failure(true, "input probe", "boom").is_ok());
    assert!(watch_failure(true, "refresh", "boom").is_ok());
}

/// Production source, so its wiring can be read where it cannot be run.
fn production() -> &'static str {
    let src = include_str!("status_watch.rs");
    &src[..src.find("#[cfg(test)]").expect("the tests")]
}

/// `status --watch` subscribes read-only and never reaches a starter.
/// Starting a monitor is banned in this suite, so the wiring is read:
/// the panel watches through [`watch_status_paths`], which takes
/// `core_wakes`, which only subscribes.
#[test]
fn status_watch_never_reaches_a_starter() {
    let src = production();
    let from = src.find("impl Panel for StatusPanel").expect("the panel");
    let from = from + src[from..].find("fn watch(").expect("its watch");
    let watch = &src[from..from + src[from..].find("\n    }\n").unwrap()];
    assert!(
        watch.contains("status::watch_status_paths(wake.clone(), repo, git, &self.watches)?"),
        "{watch}"
    );
    for starting in ["watch_tui_paths", "keeper", "spawn", "attach_or_spawn"] {
        assert!(!watch.contains(starting), "{starting} in {watch}");
    }

    let status = include_str!("status.rs");
    let from = status.find("pub(crate) fn watch_status_paths").unwrap();
    let paths = &status[from..from + status[from..].find("\n}\n").unwrap()];
    assert!(paths.contains("monitor::core_wakes(repo"), "{paths}");
    for starting in ["keeper", "spawn", "attempt", "attach_or_spawn"] {
        assert!(!paths.contains(starting), "{starting} in {paths}");
    }
    let monitor = include_str!("monitor.rs");
    let from = monitor
        .find("pub(crate) fn core_wakes")
        .expect("core_wakes");
    let core = &monitor[from..from + monitor[from..].find("\n}\n").unwrap()];
    assert!(core.contains("match subscribe(repo)"), "{core}");
    for starting in ["spawn", "attempt", "keeper"] {
        assert!(!core.contains(starting), "{starting} in {core}");
    }
}

/// The watch is this process's own authority, restarted with its own
/// arguments, and sleeps for nothing but its own timers.
#[test]
fn status_watch_is_this_process_and_never_polls() {
    let src = production();
    assert!(src.contains("crate::upgrade::process().clone()"));
    assert!(src.contains("crate::upgrade::exec_installed_as_started()"));
    assert!(src.contains(
        "crate::upgrade::exec_installed(resumed_args(std::env::args_os().skip(1), path))"
    ));
    for poll in ["sleep(", "recv_timeout(Duration", "interval("] {
        assert!(!src.contains(poll), "{poll} in the watch");
    }
}

/// The open-file limit is raised only upwards, never past the hard
/// limit, and to `OPEN_MAX` when the hard limit is unlimited.
#[test]
fn the_open_file_limit_is_raised_within_the_hard_limit() {
    assert_eq!(open_file_targets(Some(256), None), [10240]);
    assert_eq!(open_file_targets(Some(256), Some(65536)), [65536, 10240]);
    assert_eq!(open_file_targets(Some(256), Some(4096)), [4096]);
    assert_eq!(open_file_targets(Some(65536), None), Vec::<u64>::new());
    assert_eq!(open_file_targets(None, None), Vec::<u64>::new());
}

/// Repos added together start the worktree stream once; a membership
/// change restarts it once, and every repo already watched is looked at
/// again for what the restart may have missed. A quiet turn restarts
/// nothing.
#[tokio::test]
async fn a_membership_change_restarts_the_stream_once() {
    let mut h = many();
    for repo in ["/w/a", "/w/b", "/w/c", "/w/d", "/w/e"] {
        h.world.exists(repo);
    }
    for repo in ["/w/a", "/w/b", "/w/c"] {
        h.add(repo);
    }
    h.watch.turn().await.unwrap();
    assert_eq!(*h.world.commits.lock().unwrap(), 1);
    h.add("/w/d");
    h.add("/w/e");
    h.watch.turn().await.unwrap();
    assert_eq!(*h.world.commits.lock().unwrap(), 2);
    assert_eq!(
        ["/w/a", "/w/b", "/w/c", "/w/d", "/w/e"].map(|r| h.world.probes(r)),
        [2, 2, 2, 1, 1]
    );
    h.watch.remove(Path::new("/w/e"));
    h.watch.turn().await.unwrap();
    assert_eq!(*h.world.commits.lock().unwrap(), 3);
    h.watch.turn().await.unwrap();
    assert_eq!(*h.world.commits.lock().unwrap(), 3);
    assert_eq!(h.world.probes("/w/a"), 3);
}
