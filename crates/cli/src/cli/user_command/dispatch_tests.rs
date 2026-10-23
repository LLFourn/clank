use super::*;
use crate::cli::composer::fixtures::claude;
use crate::cli::pane_target::Process;
use crate::cli::user_command::envelope::testkey::{NOW, Signer, payload};
use crate::proc_identity::ProcToken;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const TAG: &str = "tag";

fn label() -> AgentLabel {
    AgentLabel::parse("claude").unwrap()
}

fn id(s: &str) -> Id {
    Id::parse(s).unwrap()
}

fn sid(s: &str) -> SessionId {
    SessionId::parse(s).unwrap()
}

fn birth(n: u64) -> ProcToken {
    ProcToken::MacStart { sec: n, usec: 0 }
}

fn seen() -> PaneSeen {
    PaneSeen {
        session: "zs".into(),
        pane: "terminal_3".into(),
        pid: 42,
        birth: birth(1),
    }
}

fn process(pid: i32, b: u64) -> Process {
    Process {
        pid,
        birth: birth(b),
        harness: Ok("2.1.308".into()),
    }
}

const ROW: UserCommandEvidence = UserCommandEvidence {
    version: "2.1.308",
    max_command: 200,
    how: "test row",
};

/// How a fake write answers: by its index, or by crashing the attempt
/// before or after the write lands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    Ok,
    NotStarted,
    Unknown,
    CrashBefore,
    CrashAfter,
}

struct Fake {
    now: AtomicU64,
    key: Mutex<Option<SiteKey>>,
    binding: Mutex<Option<(Tool, SessionId)>>,
    discover: Mutex<Result<(PaneSeen, Tool), String>>,
    looks: Mutex<Result<(), String>>,
    launch: Mutex<Result<(Tool, Process), String>>,
    screen: Mutex<Option<String>>,
    answers: Vec<Write>,
    written: Mutex<Vec<Vec<String>>>,
    evidence: Vec<UserCommandEvidence>,
    history: Mutex<Vec<nudge_log::Record>>,
    /// A real repo whose binding lock and config are used instead.
    repo: Option<PathBuf>,
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    events: std::sync::Arc<Mutex<Vec<&'static str>>>,
    /// Run once the loop's last look has passed, before any other lock.
    after_look: Mutex<Option<fn(&Fake)>>,
    /// Run while the pane's input is being read.
    during_dump: Mutex<Option<fn(&Fake)>>,
    /// Run before the pane's lock is granted: whatever held it did this.
    before_pane: Mutex<Option<fn(&Fake)>>,
    pane_held: std::sync::Arc<std::sync::atomic::AtomicBool>,
    binding_held: std::sync::Arc<std::sync::atomic::AtomicBool>,
    unguarded: Unguarded,
}

/// Writes made without both guards held. Checked when the fake goes, so a
/// write typed without the pane's lock and the label's binding lock both
/// held fails whichever test made it.
#[derive(Default)]
struct Unguarded(Mutex<Vec<Vec<String>>>);

impl Drop for Unguarded {
    fn drop(&mut self) {
        let writes = self.0.lock().unwrap();
        if !std::thread::panicking() {
            assert!(writes.is_empty(), "written without its locks: {writes:?}");
        }
    }
}

/// Holds `0` true for exactly as long as the guard lives.
struct Held(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Held {
    fn take(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        assert!(!flag.swap(true, Ordering::SeqCst), "taken twice");
        Held(flag.clone())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Fake {
    fn new(key: &SiteKey) -> Self {
        Fake {
            now: AtomicU64::new(NOW),
            key: Mutex::new(Some(key.clone())),
            binding: Mutex::new(Some((Tool::Claude, sid("session-one")))),
            discover: Mutex::new(Ok((seen(), Tool::Claude))),
            looks: Mutex::new(Ok(())),
            launch: Mutex::new(Ok((Tool::Claude, process(42, 1)))),
            screen: Mutex::new(Some(claude(&["❯ "]))),
            answers: vec![Write::Ok, Write::Ok],
            written: Mutex::new(Vec::new()),
            evidence: vec![ROW],
            history: Mutex::new(Vec::new()),
            repo: None,
            during: Mutex::new(None),
            events: Default::default(),
            after_look: Mutex::new(None),
            during_dump: Mutex::new(None),
            before_pane: Mutex::new(None),
            pane_held: Default::default(),
            binding_held: Default::default(),
            unguarded: Unguarded::default(),
        }
    }

    fn writes(&self) -> usize {
        self.written.lock().unwrap().len()
    }
}

struct Term<'a>(&'a Fake);

impl Terminal for Term<'_> {
    async fn dump(&self) -> Option<String> {
        if let Some(during) = self.0.during_dump.lock().unwrap().take() {
            during(self.0);
        }
        self.0.screen.lock().unwrap().clone()
    }
    async fn write(&self, argv: Vec<String>) -> Result<(), PaneWrite> {
        let n = self.0.writes();
        if n == 0
            && let Some(during) = self.0.during.lock().unwrap().take()
        {
            during();
        }
        if !self.0.pane_held.load(Ordering::SeqCst) || !self.0.binding_held.load(Ordering::SeqCst) {
            self.0.unguarded.0.lock().unwrap().push(argv.clone());
        }
        let answer = self.0.answers.get(n).copied().unwrap_or(Write::Ok);
        if answer == Write::CrashBefore {
            panic!("the dispatcher died before write {n}");
        }
        self.0.written.lock().unwrap().push(argv);
        self.0.events.lock().unwrap().push("write");
        match answer {
            Write::Ok => Ok(()),
            Write::NotStarted => Err(PaneWrite::NotStarted("no zellij".into())),
            Write::Unknown => Err(PaneWrite::Unknown("timed out".into())),
            Write::CrashAfter => panic!("the dispatcher died after write {n}"),
            Write::CrashBefore => unreachable!(),
        }
    }
}

impl Seams for Fake {
    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    fn key(&self) -> Option<SiteKey> {
        self.key.lock().unwrap().clone()
    }
    fn binding(&self, label: &AgentLabel) -> Option<(Tool, SessionId)> {
        match &self.repo {
            Some(repo) => crate::agent_store::load_agent_config(repo, label)
                .ok()
                .flatten()
                .and_then(|c| c.session)
                .map(|s| (s.tool, s.id)),
            None => self.binding.lock().unwrap().clone(),
        }
    }
    async fn discover(&self, _: &AgentLabel) -> Result<(PaneSeen, Tool), String> {
        self.discover.lock().unwrap().clone()
    }
    async fn last_look(&self, _: &AgentLabel, _: &SessionId) -> Result<(), String> {
        let answer = self.looks.lock().unwrap().clone();
        if let Some(after) = self.after_look.lock().unwrap().take() {
            after(self);
        }
        answer
    }
    async fn hold_binding<'a>(
        &'a self,
        label: &'a AgentLabel,
    ) -> Result<Box<dyn Send + 'a>, String> {
        match &self.repo {
            Some(repo) => crate::agent_store::hold_binding(repo, label)
                .map(|g| Box::new((g, Held::take(&self.binding_held))) as Box<dyn Send + 'a>)
                .map_err(|e| format!("{e:#}")),
            None => Ok(Box::new(Held::take(&self.binding_held))),
        }
    }
    async fn lock_pane(&self, _: &PaneSeen) -> Result<Box<dyn Send>, String> {
        if let Some(before) = self.before_pane.lock().unwrap().take() {
            before(self);
        }
        Ok(Box::new(Held::take(&self.pane_held)))
    }
    fn launch(&self, _: &AgentLabel, _: &PaneSeen) -> Result<(Tool, Process), String> {
        self.launch.lock().unwrap().clone()
    }
    type Term<'a> = Term<'a>;

    fn terminal<'a>(&'a self, _: &'a PaneSeen) -> Term<'a> {
        Term(self)
    }
    fn evidence(&self) -> &[UserCommandEvidence] {
        &self.evidence
    }
    fn history(&self, _: &AgentLabel, record: nudge_log::Record) {
        self.history.lock().unwrap().push(record);
    }
}

struct World {
    _home: tempfile::TempDir,
    store: Store,
    repo: PathBuf,
    site: Signer,
}

impl World {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        World {
            store: Store::at(home.path().join("uc")),
            repo: PathBuf::from("/w/proj"),
            _home: home,
            site: Signer::generate(),
        }
    }

    /// File `envelope` as admitted under `id`, the way `submit` does.
    fn file(&self, id: &Id, envelope: envelope::Envelope) {
        let held = self.store.lock(TAG, &label(), id).unwrap();
        held.save(&Record::admitted(
            envelope,
            self.site.key().fingerprint(),
            NOW,
        ))
        .unwrap();
    }

    fn admit(&self) -> Id {
        self.file(&id("r1"), self.site.sign(&payload(&self.repo)));
        id("r1")
    }

    fn record(&self, id: &Id) -> Record {
        self.store.read(TAG, &label(), id).unwrap().unwrap()
    }

    fn put(&self, id: &Id, record: &Record) {
        self.store
            .lock(TAG, &label(), id)
            .unwrap()
            .save(record)
            .unwrap();
    }

    fn job(&self, id: &Id) -> Job {
        Job {
            label: label(),
            id: id.clone(),
            dispatch: true,
        }
    }

    async fn attempt(&self, seams: &Fake, id: &Id) -> Ended {
        attempt(&self.store, TAG, &self.repo, &self.job(id), seams).await
    }

    fn pass(
        &self,
        key: Option<&SiteKey>,
        now: u64,
        agents: &mut Agents_,
    ) -> (Pass, Vec<nudge_log::Record>) {
        let history = Mutex::new(Vec::new());
        let note = |_: &AgentLabel, r: nudge_log::Record| history.lock().unwrap().push(r);
        let ctx = Ctx {
            store: &self.store,
            tag: TAG,
            repo: &self.repo,
            key,
            now,
            history: &note,
        };
        let out = pass(&ctx, agents, |_, _| false);
        (out, history.into_inner().unwrap())
    }
}

#[allow(non_camel_case_types)]
struct Agents_ {
    binding: Option<(Tool, SessionId)>,
    look: Look,
}

impl Agents for Agents_ {
    fn binding(&self, _: &AgentLabel) -> Option<(Tool, SessionId)> {
        self.binding.clone()
    }
    fn look(&mut self, _: &AgentLabel, _: &SessionId) -> Look {
        self.look.clone()
    }
}

fn agents(look: Look) -> Agents_ {
    Agents_ {
        binding: Some((Tool::Claude, sid("session-one"))),
        look,
    }
}

/// A clean attempt types `!` and the line, then Enter, and records it
/// delivered, with the line in the agent's nudge history.
#[tokio::test]
async fn an_idle_empty_prompt_gets_the_command_once() {
    let w = World::new();
    let r1 = w.admit();
    let seams = Fake::new(&w.site.key());
    assert_eq!(w.attempt(&seams, &r1).await, Ended::Done);
    let written = seams.written.lock().unwrap().clone();
    assert_eq!(written.len(), 2);
    assert_eq!(written[0].last().unwrap(), "!git push origin feature");
    assert_eq!(written[1].last().unwrap(), "13");
    let record = w.record(&r1);
    assert_eq!(record.state, State::Delivered);
    assert_eq!(record.pane, Some(seen()));
    let history = seams.history.lock().unwrap().clone();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome, nudge_log::Outcome::Submitted);
    assert_eq!(history[0].text, "!git push origin feature");
    assert_eq!(w.attempt(&seams, &r1).await, Ended::Done);
    assert_eq!(
        seams.writes(),
        2,
        "a delivered request is never typed again"
    );
}

/// A failure before the text wrote nothing; one between the text and Enter
/// may have typed it. Neither is retried.
#[tokio::test]
async fn a_failed_write_is_recorded_as_what_it_may_have_done() {
    for (answers, state, writes) in [
        (vec![Write::NotStarted], State::FailedNotStarted, 1),
        (vec![Write::Ok, Write::Unknown], State::Unconfirmed, 2),
        (vec![Write::Ok, Write::NotStarted], State::Unconfirmed, 2),
    ] {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake {
            answers,
            ..Fake::new(&w.site.key())
        };
        w.attempt(&seams, &r1).await;
        assert_eq!(w.record(&r1).state, state);
        w.attempt(&seams, &r1).await;
        let (out, _) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Go));
        assert!(out.jobs.is_empty());
        assert_eq!(seams.writes(), writes, "{state:?}: never retried");
    }
}

/// A dispatcher that dies after `dispatching` is recorded, at each of the
/// three points (before the text, between the text and Enter, after Enter
/// before the outcome), leaves `dispatching`; the next reader with the lock
/// records `unconfirmed`, and nothing is typed again.
#[tokio::test]
async fn a_crash_after_dispatching_is_unconfirmed_and_never_replayed() {
    use futures_util::FutureExt;
    for (answers, writes) in [
        (vec![Write::CrashBefore], 0),
        (vec![Write::Ok, Write::CrashBefore], 1),
        (vec![Write::Ok, Write::CrashAfter], 2),
    ] {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake {
            answers,
            ..Fake::new(&w.site.key())
        };
        let died = std::panic::AssertUnwindSafe(w.attempt(&seams, &r1))
            .catch_unwind()
            .await;
        assert!(died.is_err(), "the fake crash ran");
        assert_eq!(w.record(&r1).state, State::Dispatching);
        let (out, history) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Go));
        assert!(out.jobs.is_empty());
        let record = w.record(&r1);
        assert_eq!(record.state, State::Unconfirmed, "{writes}");
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history[0].outcome,
            nudge_log::Outcome::Indeterminate { .. }
        ));
        w.attempt(&Fake::new(&w.site.key()), &r1).await;
        assert_eq!(seams.writes(), writes, "no second write");
    }
}

/// The dispatch path itself, handed a record already `dispatching` (its
/// dispatcher died holding the lock), types nothing and records it
/// `unconfirmed`, with the outcome in the agent's history.
#[tokio::test]
async fn a_dispatching_record_is_settled_unconfirmed_by_the_attempt() {
    let w = World::new();
    let r1 = w.admit();
    let mut record = w.record(&r1);
    record.moved(State::Dispatching, None, NOW);
    w.put(&r1, &record);
    let seams = Fake::new(&w.site.key());
    assert_eq!(w.attempt(&seams, &r1).await, Ended::Done);
    assert_eq!(seams.writes(), 0);
    assert_eq!(w.record(&r1).state, State::Unconfirmed);
    let history = seams.history.lock().unwrap().clone();
    assert_eq!(history.len(), 1);
    assert!(matches!(
        history[0].outcome,
        nudge_log::Outcome::Indeterminate { .. }
    ));
    w.attempt(&seams, &r1).await;
    assert_eq!(seams.writes(), 0, "and never afterwards");
}

/// The decision to type is made after the input is read, against the
/// world as it is then: a request that expires, a key revoked, or the
/// agent no longer idle while the dump was being taken, and an ordinary
/// clank writer that won the pane first and started a turn, each end in
/// nothing typed.
#[tokio::test]
async fn nothing_decided_before_the_dump_or_the_pane_lock_is_trusted() {
    type Change = fn(&Fake);
    let busy: Change = |f| *f.looks.lock().unwrap() = Err("its turn is running".into());
    let cases: [(&str, Option<Change>, Option<Change>, State); 4] = [
        (
            "expired during the dump",
            Some(|f| f.now.store(NOW + 301 + envelope::SKEW, Ordering::SeqCst)),
            None,
            State::Refused,
        ),
        (
            "key revoked during the dump",
            Some(|f| *f.key.lock().unwrap() = None),
            None,
            State::Refused,
        ),
        (
            "busy after the dump",
            Some(busy),
            None,
            State::WaitingForIdle,
        ),
        (
            "an ordinary writer won the pane",
            None,
            Some(busy),
            State::WaitingForIdle,
        ),
    ];
    for (what, during_dump, before_pane, state) in cases {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake::new(&w.site.key());
        *seams.during_dump.lock().unwrap() = during_dump;
        *seams.before_pane.lock().unwrap() = before_pane;
        w.attempt(&seams, &r1).await;
        assert_eq!(seams.writes(), 0, "{what}");
        assert_eq!(w.record(&r1).state, state, "{what}");
    }
}

/// What changes after the loop's last look and before the locks are taken
/// is caught by the checks under them: the key replaced, the window run
/// out, the binding moved. Nothing is typed.
#[tokio::test]
async fn the_checks_under_the_locks_see_what_changed_after_the_last_look() {
    let changes: [(&str, fn(&Fake)); 3] = [
        ("key changed", |f| {
            *f.key.lock().unwrap() = Some(Signer::generate().key())
        }),
        ("expired", |f| {
            f.now.store(NOW + 301 + envelope::SKEW, Ordering::SeqCst)
        }),
        (
            "the session it was signed for no longer holds the label",
            |f| *f.binding.lock().unwrap() = Some((Tool::Claude, sid("session-two"))),
        ),
    ];
    for (reason, change) in changes {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake::new(&w.site.key());
        *seams.after_look.lock().unwrap() = Some(change);
        w.attempt(&seams, &r1).await;
        let record = w.record(&r1);
        assert_eq!(record.state, State::Refused, "{reason}");
        assert_eq!(record.reason.as_deref(), Some(reason));
        assert_eq!(seams.writes(), 0, "{reason}");
    }
}

/// While a live dispatcher holds the id lock, a reader leaves its
/// `dispatching` record as it stands.
#[test]
fn a_held_dispatching_record_is_left_alone() {
    let w = World::new();
    let r1 = w.admit();
    let mut record = w.record(&r1);
    record.moved(State::Dispatching, None, NOW);
    w.put(&r1, &record);
    let held = w.store.lock(TAG, &label(), &r1).unwrap();
    let (out, history) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Go));
    assert_eq!(out, Pass::default());
    assert!(history.is_empty());
    drop(held);
    assert_eq!(w.record(&r1).state, State::Dispatching);
}

/// A record fabricated outright, or whose stored request, signature,
/// target or times were edited after admission, or filed under a path its
/// payload does not name, is refused at its next use: no park released,
/// no attempt, nothing typed.
#[tokio::test]
async fn stored_authority_is_judged_at_every_use() {
    let b64 = |b: &[u8]| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(b)
    };
    let w = World::new();
    let forger = Signer::generate();
    let good = w.site.sign(&payload(&w.repo));
    let mut edits: Vec<(Id, envelope::Envelope)> = Vec::new();
    edits.push((id("fabricated"), {
        let mut p = payload(&w.repo);
        p["id"] = "fabricated".into();
        forger.sign(&p)
    }));
    for (n, (field, value)) in [
        ("command", serde_json::Value::from("rm -rf ~")),
        ("session", "session-two".into()),
        ("agent", "codex".into()),
        ("expires_at", (NOW + 900).into()),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("edited{n}");
        let mut p = payload(&w.repo);
        p["id"] = name.clone().into();
        let signed = w.site.sign(&p);
        p[field] = value;
        edits.push((
            id(&name),
            envelope::Envelope {
                request: b64(&serde_json::to_vec(&p).unwrap()),
                sig: signed.sig,
            },
        ));
    }
    edits.push((
        id("resigned"),
        envelope::Envelope {
            request: good.request.clone(),
            sig: b64(&[0u8; 64]),
        },
    ));
    edits.push((id("misfiled"), good.clone()));
    for (id, envelope) in &edits {
        w.file(id, envelope.clone());
    }
    let (out, history) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Parked));
    assert_eq!(out, Pass::default(), "no release, no watch, no attempt");
    assert_eq!(history.len(), edits.len());
    for (id, _) in &edits {
        let record = w.record(id);
        assert_eq!(record.state, State::Refused, "{}", id.as_str());
    }
    assert!(
        w.record(&id("misfiled"))
            .reason
            .unwrap()
            .contains("disagree with where it was found")
    );
    let seams = Fake::new(&w.site.key());
    for (id, _) in &edits {
        w.attempt(&seams, id).await;
    }
    assert_eq!(seams.writes(), 0);
}

/// After the trusted key is replaced, or removed, an admitted request is
/// refused at its next use, and nothing is released or typed; a terminal
/// outcome recorded before the change stays as it was.
#[tokio::test]
async fn a_changed_or_removed_key_refuses_at_next_use_and_keeps_history() {
    let w = World::new();
    let delivered = id("done");
    let mut p = payload(&w.repo);
    p["id"] = "done".into();
    w.file(&delivered, w.site.sign(&p));
    let seams = Fake::new(&w.site.key());
    w.attempt(&seams, &delivered).await;
    assert_eq!(w.record(&delivered).state, State::Delivered);
    let before = w.record(&delivered);

    for (key, reason) in [
        (Some(Signer::generate().key()), "key changed"),
        (None, "no trusted key"),
    ] {
        let r1 = w.admit();
        let mut record = w.record(&r1);
        record.state = State::Admitted;
        record.reason = None;
        w.put(&r1, &record);
        let (out, _) = w.pass(key.as_ref(), NOW, &mut agents(Look::Parked));
        assert!(out.release.is_empty() && out.jobs.is_empty(), "{reason}");
        assert_eq!(w.record(&r1).state, State::Refused);
        assert_eq!(w.record(&r1).reason.as_deref(), Some(reason));
        assert_eq!(w.record(&delivered), before, "history is not rewritten");
    }

    let r2 = id("r2");
    let mut p = payload(&w.repo);
    p["id"] = "r2".into();
    w.file(&r2, w.site.sign(&p));
    let rotated = Fake::new(&Signer::generate().key());
    w.attempt(&rotated, &r2).await;
    assert_eq!(w.record(&r2).reason.as_deref(), Some("key changed"));
    assert_eq!(rotated.writes(), 0);
}

/// The pass records why a request waits, asks for a park to end only when
/// a park alone stands in the way, hands an idle agent's request to an
/// attempt, and rewrites nothing that did not change.
#[test]
fn a_pass_waits_releases_or_dispatches_and_writes_only_changes() {
    let w = World::new();
    let r1 = w.admit();
    let (out, _) = w.pass(
        Some(&w.site.key()),
        NOW,
        &mut agents(Look::Wait("its turn is running".into())),
    );
    assert_eq!(
        out.jobs,
        vec![Job {
            label: label(),
            id: r1.clone(),
            dispatch: false
        }],
        "the first look finds the pane"
    );
    assert_eq!(out.watch, [label()].into_iter().collect());
    assert_eq!(out.next_expiry, Some(NOW + 300 + envelope::SKEW + 1));
    let record = w.record(&r1);
    assert_eq!(record.state, State::WaitingForIdle);
    assert_eq!(record.reason.as_deref(), Some("its turn is running"));
    let stamp = w.store.stamp(TAG, &label(), &r1);
    w.pass(
        Some(&w.site.key()),
        NOW + 5,
        &mut agents(Look::Wait("its turn is running".into())),
    );
    assert_eq!(
        w.store.stamp(TAG, &label(), &r1),
        stamp,
        "unchanged, not rewritten"
    );

    let (out, _) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Parked));
    assert_eq!(out.release, [label()].into_iter().collect());
    assert_eq!(
        w.record(&r1).reason.as_deref(),
        Some("releasing its parked turn")
    );
    let (out, _) = w.pass(Some(&w.site.key()), NOW, &mut agents(Look::Go));
    assert!(out.release.is_empty());
    assert_eq!(
        out.jobs,
        vec![Job {
            label: label(),
            id: r1.clone(),
            dispatch: true
        }]
    );
}

/// Expiry at a pass or at dispatch is refused, never typed.
#[tokio::test]
async fn an_expired_request_is_refused() {
    let w = World::new();
    let r1 = w.admit();
    let (_, history) = w.pass(
        Some(&w.site.key()),
        NOW + 301 + envelope::SKEW,
        &mut agents(Look::Go),
    );
    assert_eq!(w.record(&r1).reason.as_deref(), Some("expired"));
    assert_eq!(history.len(), 1);

    let w = World::new();
    let r1 = w.admit();
    let seams = Fake::new(&w.site.key());
    seams
        .now
        .store(NOW + 301 + envelope::SKEW, Ordering::SeqCst);
    w.attempt(&seams, &r1).await;
    assert_eq!(w.record(&r1).reason.as_deref(), Some("expired"));
    assert_eq!(seams.writes(), 0);
}

/// Refused, with nothing typed: a binding that moved, a pane replaced
/// since the first look, a pane that runs another tool, a pane that cannot
/// be located with certainty, an unverified launch, no evidence row for
/// the version, and a command longer than the row's bound.
#[tokio::test]
async fn a_changed_target_is_refused() {
    type Change = fn(&mut Fake);
    let changes: [(&str, Change); 8] = [
        ("binding", |f| {
            *f.binding.lock().unwrap() = Some((Tool::Claude, sid("session-two")))
        }),
        ("replaced", |f| {
            *f.launch.lock().unwrap() = Ok((Tool::Claude, process(42, 2)))
        }),
        ("pid", |f| {
            *f.launch.lock().unwrap() = Ok((Tool::Claude, process(43, 1)))
        }),
        ("tool", |f| {
            *f.discover.lock().unwrap() = Ok((seen(), Tool::Codex))
        }),
        ("ambiguous", |f| {
            *f.discover.lock().unwrap() = Err("live panes in 2 sessions".into())
        }),
        ("unverified", |f| {
            *f.launch.lock().unwrap() = Err("process ended".into())
        }),
        ("evidence", |f| f.evidence.clear()),
        ("bound", |f| f.evidence[0].max_command = 5),
    ];
    for (what, change) in changes {
        let w = World::new();
        let r1 = w.admit();
        let mut seams = Fake::new(&w.site.key());
        change(&mut seams);
        w.attempt(&seams, &r1).await;
        assert_eq!(w.record(&r1).state, State::Refused, "{what}");
        assert_eq!(seams.writes(), 0, "{what}");
    }
}

/// Only an `Empty` input is typed into: a draft, Claude's shell mode, an
/// unreadable or missing dump, or a loop that says the agent is not idle
/// (a running turn, a park) leave the request waiting, typed into nothing.
#[tokio::test]
async fn only_an_empty_prompt_at_an_idle_agent_is_typed_into() {
    let screens = [
        Some(claude(&["❯ half a thought"])),
        Some(claude(&["! "])),
        Some("❯ old\n╭ Allow this tool? ╮".to_string()),
        None,
    ];
    for screen in screens {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake::new(&w.site.key());
        *seams.screen.lock().unwrap() = screen.clone();
        let ended = w.attempt(&seams, &r1).await;
        assert!(
            matches!(ended, Ended::OnScreen { stamp: Some(_), expires } if expires > NOW + 300),
            "{screen:?}"
        );
        assert_eq!(w.record(&r1).state, State::WaitingForIdle, "{screen:?}");
        assert_eq!(seams.writes(), 0);
    }
    for why in ["its turn is running", "its turn is parked"] {
        let w = World::new();
        let r1 = w.admit();
        let seams = Fake::new(&w.site.key());
        *seams.looks.lock().unwrap() = Err(why.into());
        w.attempt(&seams, &r1).await;
        assert_eq!(w.record(&r1).reason.as_deref(), Some(why));
        assert_eq!(seams.writes(), 0);
    }
}

/// A repo whose master `claude` runs Claude, bound to `s1`.
fn bound_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    crate::cli::agent::add_repo_roster_agent(
        dir.path(),
        &label(),
        crate::cli::teams_config::AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_master(dir.path(), &label()).unwrap();
    crate::agent_store::bind_session_to_agent(
        dir.path(),
        &label(),
        Tool::Claude,
        &sid("session-one"),
    )
    .unwrap();
    dir
}

/// Through the label's real binding lock: a rebind attempted while the
/// command is being typed waits until the attempt is done, and an attempt
/// that starts after a rebind is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebind_waits_for_a_dispatch_and_one_before_it_refuses_it() {
    let repo = bound_repo();
    let w = World {
        repo: repo.path().to_path_buf(),
        ..World::new()
    };
    let r1 = w.admit();
    let seams = Fake {
        repo: Some(repo.path().to_path_buf()),
        ..Fake::new(&w.site.key())
    };
    let (thread_repo, events) = (repo.path().to_path_buf(), seams.events.clone());
    let rebind: std::sync::Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Default::default();
    let slot = rebind.clone();
    *seams.during.lock().unwrap() = Some(Box::new(move || {
        *slot.lock().unwrap() = Some(std::thread::spawn(move || {
            crate::agent_store::bind_session_to_agent(
                &thread_repo,
                &AgentLabel::parse("claude").unwrap(),
                Tool::Claude,
                &SessionId::parse("session-two").unwrap(),
            )
            .unwrap();
            events.lock().unwrap().push("rebound");
        }));
        std::thread::sleep(std::time::Duration::from_millis(150));
    }));
    w.attempt(&seams, &r1).await;
    assert_eq!(w.record(&r1).state, State::Delivered);
    rebind.lock().unwrap().take().unwrap().join().unwrap();
    assert_eq!(
        *seams.events.lock().unwrap(),
        ["write", "write", "rebound"],
        "the rebind waited for the dispatch to finish"
    );

    let mut p = payload(&w.repo);
    p["id"] = "r2".into();
    let r2 = id("r2");
    w.file(&r2, w.site.sign(&p));
    let after = Fake {
        repo: Some(repo.path().to_path_buf()),
        ..Fake::new(&w.site.key())
    };
    w.attempt(&after, &r2).await;
    assert_eq!(w.record(&r2).state, State::Refused);
    assert_eq!(after.writes(), 0);
}
