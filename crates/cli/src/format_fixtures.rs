//! The bytes clank wrote before its config documents carried a version,
//! read through their production readers and, where the format has one,
//! written back through its production read-modify-write.
//!
//! The fixtures under `tests/fixtures/formats/` are frozen: an upgrade
//! meets exactly these bytes on disk, so a test here that fails means an
//! upgrade would lose what they say. Change the reader, never the fixture.

use std::path::{Path, PathBuf};

use clank_core::ids::{AgentLabel, SessionId};
use clank_core::vocab::{AutoMode, Tool};
use serde_json::{Value, json};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../tests/fixtures/formats/", $name))
    };
}

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn place(at: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, body).unwrap();
    at.to_path_buf()
}

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

fn raw(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    repo
}

// ── config documents ──────────────────────────────────────────────

/// What the repo fixture says, as every reader of it must still see it.
fn assert_repo_fixture_meaning(repo: &Path, port: u16) {
    use crate::cli::teams_config::RosterRole;
    let cfg = crate::agent_store::repo_config::read(repo)
        .unwrap()
        .unwrap();
    let roles: Vec<(&str, RosterRole)> = cfg
        .agents
        .iter()
        .map(|(l, a)| (l.as_str(), a.role))
        .collect();
    assert_eq!(
        roles,
        [
            ("claude", RosterRole::Master),
            ("codex", RosterRole::Commit),
            ("ruthless", RosterRole::Gate),
        ]
    );
    let codex = &cfg.agents[&label("codex")];
    let launch = codex.launch.as_ref().unwrap();
    assert_eq!(launch.args, ["--profile", "deep"]);
    assert_eq!(launch.env["CODEX_EXAMPLE"], "1");
    assert_eq!(
        cfg.agents[&label("ruthless")].initial_prompt.as_deref(),
        Some("Review each commit as it lands.")
    );
    let review = cfg.review.as_ref().unwrap();
    assert_eq!(
        review.adhoc_feedback,
        Some(true),
        "force_review_on_misc_commits"
    );
    assert_eq!(
        review.plan_feedback,
        Some(false),
        "force_review_on_plan_commits"
    );
    assert_eq!(review.require_commit_prefix, Some(true));
    assert_eq!(
        cfg.hooks.as_ref().unwrap().extra["future_hook"],
        json!({"cmd": "echo", "env": {"A": "1"}})
    );
    assert_eq!(cfg.web.as_ref().unwrap().port, Some(port));
    assert_eq!(
        cfg.extra["a_newer_section"],
        json!({"kept": ["exactly", 1, true]})
    );

    let set = crate::agent_store::try_resolve_via_team_with(repo, None)
        .unwrap()
        .unwrap();
    assert_eq!(set.master.as_str(), "claude");
    assert_eq!(set.commit_tier(), [label("codex")]);
    assert_eq!(set.final_tier(), [label("ruthless")]);

    let effective = crate::cli::config::load_with_home(repo, None).unwrap();
    assert!(effective.review.adhoc_feedback);
    assert!(!effective.review.plan_feedback);
    assert!(effective.review.require_commit_prefix);
    assert_eq!(
        effective.hooks[&clank_core::HookEvent::Blocked].as_deref(),
        Some("notify-send clank-blocked")
    );
    let editor = effective.diff.editor.as_ref().unwrap();
    assert_eq!(editor.command.as_deref(), Some("emacsclient"));
    assert_eq!(editor.args, ["-n"]);
    assert_eq!(effective.diff.wait, Some(true));
    assert!(effective.finish.autosquash);
    assert_eq!(effective.recovery.stall_warn, Some(30 * 60));
    assert_eq!(effective.recovery.stall_nudge, None);
}

#[test]
fn an_unversioned_repo_config_keeps_its_meaning_through_every_writer() {
    let repo = tempfile::tempdir().unwrap();
    let path = place(
        &crate::agent_store::repo_config::path(repo.path()),
        fixture!("repo-config.json"),
    );
    assert_repo_fixture_meaning(repo.path(), 64473);

    crate::agent_store::record_web_port(repo.path(), 50000).unwrap();
    assert_eq!(
        raw(&path)["version"],
        1,
        "the next write stores the version"
    );
    assert_repo_fixture_meaning(repo.path(), 50000);

    crate::cli::config::set_repo_key(repo.path(), &["review", "plan_feedback"], "false", "bool")
        .unwrap();
    assert_repo_fixture_meaning(repo.path(), 50000);

    crate::cli::agent::set_repo_review(
        repo.path(),
        &label("ruthless"),
        crate::cli::teams_config::ReviewKind::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_review(
        repo.path(),
        &label("ruthless"),
        crate::cli::teams_config::ReviewKind::Gate,
    )
    .unwrap();
    assert_repo_fixture_meaning(repo.path(), 50000);
}

#[test]
fn the_legacy_repo_shape_still_gets_its_legacy_error_and_init_recreates_it() {
    let repo = tempfile::tempdir().unwrap();
    let path = place(
        &crate::agent_store::repo_config::path(repo.path()),
        fixture!("repo-config-legacy.json"),
    );
    for err in [
        crate::agent_store::repo_config::read(repo.path()).map(|_| ()),
        crate::cli::config::set_repo_key(repo.path(), &["diff", "wait"], "true", "bool"),
        crate::agent_store::record_web_port(repo.path(), 1),
    ] {
        let err = format!("{:#}", err.unwrap_err());
        assert!(
            err.contains("old team schema") && err.contains("re-run `clank init`"),
            "{err}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        fixture!("repo-config-legacy.json")
    );

    crate::cli::init::seed_bare(None, repo.path()).unwrap();
    let recreated = crate::agent_store::repo_config::read(repo.path())
        .unwrap()
        .unwrap();
    assert!(recreated.agents.is_empty() && recreated.extra.is_empty());
}

fn assert_user_fixture_meaning(home: &Path) {
    let cfg = crate::cli::team::read_user_config(home).unwrap();
    assert_eq!(cfg.auto, Some(AutoMode::On), "the `wait` alias");
    assert_eq!(
        crate::cli::team::resolve_effective_auto_mode(None, Some(home)).unwrap(),
        AutoMode::On
    );
    assert_eq!(cfg.agents.len(), 2);
    assert_eq!(cfg.agents[&label("codex")].tool, Tool::Codex);
    let dev = crate::cli::init::load_user_team_roster(home, "dev").unwrap();
    let dev: Vec<(&str, Tool)> = dev.iter().map(|(l, a)| (l.as_str(), a.tool)).collect();
    assert_eq!(
        dev,
        [
            ("claude", Tool::Claude),
            ("reviewer", Tool::Codex),
            ("scratch", Tool::Grok)
        ]
    );
    let remote = cfg.remote.as_ref().unwrap();
    assert_eq!(
        remote.token.as_deref(),
        Some("PLACEHOLDER-NOT-A-REAL-TOKEN")
    );
    assert!(cfg.zellij.unwrap().layout.unwrap().contains("clank_agents"));
    assert_eq!(cfg.extra["default_agents"], json!(["claude"]));
    assert_eq!(cfg.extra["a_newer_section"], json!({"kept": true}));

    let effective =
        crate::cli::config::load_with_home(tempfile::tempdir().unwrap().path(), Some(home))
            .unwrap();
    assert!(
        effective.review.adhoc_feedback,
        "force_review_on_misc_commits"
    );
    assert!(effective.finish.autosquash);
}

/// The names the user trusts, by name and the key each holds.
fn trusted(home: &Path) -> Vec<(String, String)> {
    crate::cli::user_command::trust_set(home)
        .keys()
        .unwrap()
        .iter()
        .map(|(name, key)| (name.to_string(), key.encoded()))
        .collect()
}

const LEGACY_SITE_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

#[test]
fn an_unversioned_user_config_keeps_its_meaning_through_every_writer() {
    let home = tempfile::tempdir().unwrap();
    let path = place(
        &clank_user_config::path(home.path()),
        fixture!("user-config.json"),
    );
    assert_user_fixture_meaning(home.path());
    assert_eq!(
        trusted(home.path()),
        [("default".to_string(), LEGACY_SITE_KEY.to_string())],
        "the legacy site_key is the key named `default`"
    );

    let other = crate::cli::user_command::envelope::testkey::Signer::generate()
        .key()
        .encoded();
    crate::cli::user_command::trust_key(home.path(), "site-b", &other, false).unwrap();
    assert_eq!(
        raw(&path)["version"],
        1,
        "the next write stores the version"
    );
    assert_eq!(raw(&path)["user_command"].get("site_key"), None);
    assert_eq!(
        trusted(home.path()),
        [
            ("default".to_string(), LEGACY_SITE_KEY.to_string()),
            ("site-b".to_string(), other.clone()),
        ],
        "the trust write folded the legacy key into `keys`"
    );
    assert_user_fixture_meaning(home.path());

    crate::cli::config::set_user_key(home.path(), &["diff", "wait"], "true", "bool").unwrap();
    assert_user_fixture_meaning(home.path());
}

fn assert_agent_fixture_meaning(repo: &Path, auto: AutoMode) {
    use clank_core::agent_config::WaitEventSource;
    let cfg = crate::agent_store::load_agent_config(repo, &label("claude"))
        .unwrap()
        .unwrap();
    assert_eq!(cfg.auto_mode, Some(auto));
    let session = cfg.session.as_ref().unwrap();
    assert_eq!(session.id.as_str(), SESSION);
    assert_eq!(session.tool, Tool::Claude);
    assert!(session.transcript.as_deref().unwrap().ends_with(".jsonl"));
    assert_eq!(
        cfg.resume_pending.as_ref().unwrap().session.as_str(),
        SESSION
    );
    let kinds: Vec<&str> = cfg
        .wait_events
        .iter()
        .map(|s| match s {
            WaitEventSource::Github(g) => {
                assert_eq!(g.repo, "example/clank");
                assert!(g.prompt.as_deref().unwrap().starts_with("CI finished"));
                assert_eq!(g.watch.len(), 1);
                "github"
            }
            WaitEventSource::Timer(t) => {
                assert_eq!(
                    (t.name.as_str(), t.repeat_every.as_deref()),
                    ("standup", Some("24h"))
                );
                "timer"
            }
            WaitEventSource::Command(c) => {
                assert_eq!(c.command, ["example-mail-watch", "--json"]);
                "command"
            }
        })
        .collect();
    assert_eq!(kinds, ["github", "timer", "command"]);
    assert_eq!(
        (cfg.extra.get("role"), cfg.extra.get("tool")),
        (Some(&json!("reviewers")), Some(&json!("claude"))),
        "keys this build does not read are kept"
    );
}

#[test]
fn an_unversioned_agent_config_keeps_its_meaning_through_every_writer() {
    let repo = tempfile::tempdir().unwrap();
    let path = place(
        &crate::agent_store::agent_config_path(repo.path(), &label("claude")),
        fixture!("agent-config.json"),
    );
    assert_agent_fixture_meaning(repo.path(), AutoMode::On);

    crate::agent_store::set_auto_mode(repo.path(), &label("claude"), AutoMode::Off).unwrap();
    assert_eq!(
        raw(&path)["version"],
        1,
        "the next write stores the version"
    );
    assert_agent_fixture_meaning(repo.path(), AutoMode::Off);

    crate::agent_store::record_transcript(
        repo.path(),
        &label("claude"),
        &SessionId::parse(SESSION).unwrap(),
        "/home/user/.claude/projects/example/moved.jsonl",
    )
    .unwrap();
    assert_agent_fixture_meaning(repo.path(), AutoMode::Off);
}

#[test]
fn an_unknown_agent_config_key_survives_update_agent_config() {
    let repo = tempfile::tempdir().unwrap();
    let path = place(
        &crate::agent_store::agent_config_path(repo.path(), &label("claude")),
        r#"{"auto_mode": "on", "a_newer_field": {"nested": [1, "two"]}}"#,
    );
    crate::agent_store::update_agent_config(repo.path(), &label("claude"), |cfg| {
        cfg.auto_mode = Some(AutoMode::Off)
    })
    .unwrap();
    assert_eq!(
        raw(&path),
        json!({"version": 1, "auto_mode": "off", "a_newer_field": {"nested": [1, "two"]}})
    );
}

// ── state files ───────────────────────────────────────────────────

/// What a parked Stop hook of the build before this one left for the
/// installed build it became: the harness's own input, byte for byte, and
/// the report its park was armed with, the evidence and the deadline
/// included. Written again, it is the same bytes.
#[test]
fn a_hook_handoff_reads_as_what_the_parked_hook_held() {
    use crate::cli::turn_wire::Report;
    let bytes = fixture!("hook-handoff.json");
    let handoff = crate::cli::hook_handoff::Handoff::parse(bytes.as_bytes()).unwrap();
    let input: clank_core::HookInput = serde_json::from_str(&handoff.input).unwrap();
    assert_eq!(input.session_id.as_str(), SESSION);
    assert_eq!(input.cwd, "/w/example");
    assert!(handoff.input.contains("\"permission_mode\":\"default\""));
    let Report::TurnEnd {
        stop,
        deadline,
        questions,
    } = &handoff.report
    else {
        panic!("{:?}", handoff.report);
    };
    assert_eq!(stop.tool, Tool::Claude);
    assert_eq!(stop.session.as_str(), SESSION);
    assert_eq!(stop.place.turn.as_deref(), Some("prompt-41"));
    assert_eq!(stop.place.position, Some(8812));
    assert!(stop.place.human_turn);
    assert_eq!(
        stop.tasks,
        clank_core::turn_policy::TaskSnapshot::of(&input),
        "the listing the Stop carried"
    );
    assert_eq!(*deadline, 1793845190330);
    assert_eq!(questions.len(), 2);
    assert_eq!(
        serde_json::to_string(&handoff).unwrap(),
        bytes.trim_end(),
        "a handoff written by this build is the same format"
    );
}

/// A check-in presentation in flight when its monitor stopped: the owed
/// window and the window the presentation carries read back as written,
/// and a monitor restored from them takes the presentation as failed, so
/// the check-in is still owed.
#[test]
fn turn_state_with_a_check_in_in_flight_reads_and_round_trips() {
    use clank_core::turn_policy::{CheckIn, CheckInRef, Resolution, SessionState};
    let dir = tempfile::tempdir().unwrap();
    let path = place(
        &dir.path().join("turn.json"),
        fixture!("turn-check-in-in-flight.json"),
    );
    let stored = crate::cli::turn_custody::load_stored(dir.path()).unwrap();
    let carried = stored.outstanding.as_ref().expect("in flight");
    assert_eq!(carried.check_ins, [CheckInRef { window: 4 }]);
    assert!(carried.identities.is_empty());
    assert_eq!(stored.attendances[0].window.id, 4);
    assert_eq!(stored.attendances[0].window.check_in, CheckIn::Due);
    assert_eq!(serde_json::to_value(&stored).unwrap(), raw(&path));
    let restored = SessionState::restored(stored.clone());
    assert_eq!(restored.outstanding, None);
    assert_eq!(
        restored.resolved,
        Some((carried.id.clone(), Resolution::Failed))
    );
    assert_eq!(restored.attendances[0].window.check_in, CheckIn::Due);
}

/// The same, as the build before windows had identities wrote it: the
/// entry is given one on loading, and the reference, keyed by selector and
/// clock, names no window, so nothing a dead presentation carried is spent
/// by it; the check-in stays owed.
#[test]
fn a_check_in_in_flight_from_before_windows_loads_owed() {
    use clank_core::turn_policy::{CheckIn, CheckInRef, Resolution, SessionState};
    let dir = tempfile::tempdir().unwrap();
    place(
        &dir.path().join("turn.json"),
        fixture!("turn-check-in-in-flight-keyed.json"),
    );
    let stored = crate::cli::turn_custody::load_stored(dir.path()).unwrap();
    let carried = stored.outstanding.as_ref().expect("in flight");
    assert_eq!(carried.check_ins, [CheckInRef { window: 0 }]);
    assert_eq!(stored.attendances[0].window.id, 1);
    assert_eq!(stored.next_window, 1);
    let restored = SessionState::restored(stored.clone());
    assert_eq!(
        restored.resolved,
        Some((carried.id.clone(), Resolution::Failed))
    );
    assert_eq!(restored.attendances[0].window.check_in, CheckIn::Due);
}

/// The entries the build that ended a declaration at the next turn start
/// wrote: the declaration reads as one not yet resolved to a listed task,
/// whose `seen_turn_end` no longer means anything, and the run keeps its
/// registration.
#[test]
fn turn_state_with_turn_scoped_declarations_reads_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    place(&dir.path().join("turn.json"), fixture!("turn.json"));
    let stored = crate::cli::turn_custody::load_stored(dir.path()).unwrap();
    assert_eq!(stored.binding.as_str(), SESSION);
    assert_eq!(stored.next_seq, 38);
    let mut expected = raw(&dir.path().join("turn.json"))["attendances"].clone();
    expected[0]["work"]["Declared"]
        .as_object_mut()
        .unwrap()
        .remove("seen_turn_end");
    for (i, entry) in expected.as_array_mut().unwrap().iter_mut().enumerate() {
        entry["window"] = json!(i + 1);
    }
    assert_eq!(serde_json::to_value(&stored.attendances).unwrap(), expected);
    assert_eq!(stored.attendances.len(), 2);
    crate::cli::turn_custody::write_stored(
        dir.path(),
        &stored,
        &crate::upgrade::process().permit().unwrap(),
    )
    .unwrap();
    assert_eq!(
        crate::cli::turn_custody::load_stored(dir.path()),
        Some(stored)
    );
}

/// The entries a build that resolved attendance against task listings
/// wrote: a run keeps its registration, so its end is still recorded, and
/// a named attendance keeps the task it resolved to, so the next listing
/// still ends it.
#[test]
fn turn_state_with_listed_attendances_reads_as_declared_work() {
    let dir = tempfile::tempdir().unwrap();
    place(
        &dir.path().join("turn.json"),
        fixture!("turn-listed-attendances.json"),
    );
    let stored = crate::cli::turn_custody::load_stored(dir.path()).unwrap();
    assert_eq!(stored.next_seq, 38);
    assert_eq!(
        serde_json::to_value(&stored.attendances).unwrap(),
        json!([
            {
                "work": {"Declared": {"name": "Implement the example plan", "task": "a0000000000000001"}},
                "desc": "Implement the example plan",
                "started": 1793845190330u64,
                "window": 1,
                "since": 1793845190330u64,
                "expect": 7200000,
                "check_in": "Armed"
            },
            {
                "work": {"Run": {"pid": 7001, "token": "mac1:7001:1"}},
                "desc": "test run",
                "started": 1793845200000u64,
                "window": 2,
                "since": 1793845200000u64,
                "expect": 600000,
                "check_in": "Armed"
            }
        ])
    );
    crate::cli::turn_custody::write_stored(
        dir.path(),
        &stored,
        &crate::upgrade::process().permit().unwrap(),
    )
    .unwrap();
    assert_eq!(
        crate::cli::turn_custody::load_stored(dir.path()),
        Some(stored)
    );
}

#[test]
fn turn_state_with_one_old_attendance_reads_as_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    place(
        &dir.path().join("turn.json"),
        fixture!("turn-single-attendance.json"),
    );
    let stored = crate::cli::turn_custody::load_stored(dir.path()).unwrap();
    assert_eq!(
        serde_json::to_value(&stored.attendances).unwrap(),
        json!([
            {
                "work": {"Declared": {"name": "capture-probe sleeper", "task": "a0000000000000002"}},
                "desc": "capture-probe sleeper",
                "started": 1000000,
                "window": 1,
                "since": 1000000,
                "expect": 600000,
                "check_in": "Armed"
            },
            {
                "work": {"Run": {"pid": 7001, "token": "mac1:7001:1"}},
                "desc": "test run",
                "started": 1120000,
                "window": 2,
                "since": 1120000,
                "expect": 1200000,
                "check_in": "Armed"
            }
        ]),
        "the standing attendance and its renewal for another task"
    );
    assert_eq!(stored.received, ["event:a"]);
    crate::cli::turn_custody::write_stored(
        dir.path(),
        &stored,
        &crate::upgrade::process().permit().unwrap(),
    )
    .unwrap();
    assert_eq!(
        crate::cli::turn_custody::load_stored(dir.path()),
        Some(stored)
    );
    assert!(
        raw(&dir.path().join("turn.json"))
            .get("attendance")
            .is_none()
    );
}

#[test]
fn a_question_record_reads_and_takes_a_new_observation() {
    use crate::cli::question_state::{Observation, load, observe};
    let repo = tempfile::tempdir().unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &label("claude"),
        Tool::Claude,
        &SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    let dir = crate::agent_store::agents_root(repo.path()).join("claude");
    place(&dir.join("question.json"), fixture!("question.json"));
    let before = load(&dir).unwrap();
    assert_eq!(before.session.as_deref(), Some(SESSION));
    assert_eq!(before.pending.len(), 1);
    assert_eq!(
        before.pending[0].header,
        "Which tunnel should the remote use?"
    );
    assert_eq!(before.over.len(), 2);
    assert_eq!(before.returned.len(), 1);

    let ended = "10000000-0000-4000-8000-000000000009".to_string();
    observe(
        &dir,
        [Observation::TurnOver {
            session: SESSION.into(),
            prompt: ended.clone(),
        }],
    );
    let after = load(&dir).unwrap();
    assert_eq!(after.pending, before.pending);
    assert_eq!(after.returned, before.returned);
    assert_eq!(after.over, [before.over.clone(), vec![ended]].concat());
}

#[test]
fn the_issued_recovery_count_reads_and_counts_on() {
    let dir = tempfile::tempdir().unwrap();
    place(
        &dir.path().join("recovery-issued.json"),
        fixture!("recovery-issued.json"),
    );
    assert_eq!(crate::cli::recovery::load_issued(dir.path()).unwrap(), 79);
    crate::cli::recovery::store_issued(dir.path(), 80).unwrap();
    assert_eq!(crate::cli::recovery::load_issued(dir.path()).unwrap(), 80);
}

#[test]
fn a_published_state_with_one_attendance_reads_as_a_list() {
    let dir = tempfile::tempdir().unwrap();
    place(&dir.path().join("state.json"), fixture!("state.json"));
    let state = crate::cli::agent_state::load(dir.path()).unwrap();
    assert_eq!(state.session, SESSION);
    assert_eq!(state.attending.len(), 1, "the single attendance");
    assert_eq!(state.attending[0].task, "a0000000000000001");
    assert_eq!(
        state.attending[0].desc.as_deref(),
        Some("Implement the example plan")
    );
    assert!(state.attending[0].declared);
    crate::cli::agent_state::store(
        dir.path(),
        &state,
        &crate::upgrade::process().permit().unwrap(),
    );
    assert_eq!(crate::cli::agent_state::load(dir.path()), Some(state));
    assert!(raw(&dir.path().join("state.json"))["attending"].is_array());
}

#[test]
fn stash_records_read_from_both_locations() {
    let repo = tempfile::tempdir().unwrap();
    place(
        &repo.path().join(".clank/stash/example-plan.json"),
        fixture!("stash-record.json"),
    );
    place(
        &repo.path().join(".clank/shelved/older-plan.json"),
        fixture!("shelved-record.json"),
    );
    let stashed: Vec<(String, Vec<String>, String, Option<String>)> =
        crate::cli::stash::scan_stash(repo.path())
            .into_iter()
            .map(|(stem, r)| (stem, r.shas, r.git_ref, r.waiting_for))
            .collect();
    assert_eq!(
        stashed,
        [
            (
                "example-plan".to_string(),
                vec![
                    "0000000000000000000000000000000000000001".to_string(),
                    "0000000000000000000000000000000000000002".to_string()
                ],
                "refs/clank/stash/example-plan".to_string(),
                Some("other-plan".to_string())
            ),
            (
                "older-plan".to_string(),
                vec!["0000000000000000000000000000000000000003".to_string()],
                "refs/clank/shelved/older-plan".to_string(),
                None
            ),
        ]
    );
}

#[test]
fn fork_specs_read_and_write_back_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("codex", fixture!("fork-spec.json")),
        ("grok", fixture!("fork-spec-grok.json")),
    ] {
        let l = label(name);
        let path = place(&crate::cli::fork::fork_spec_path(repo.path(), &l), body);
        let spec = crate::cli::fork::load_fork_spec(repo.path(), &l)
            .unwrap()
            .unwrap();
        assert!(
            spec.prompt
                .starts_with("You are a fork of the example repo")
        );
        assert!(spec.opencode.is_none());
        crate::cli::fork::save_fork_spec(repo.path(), &l, &spec).unwrap();
        assert_eq!(
            raw(&path),
            serde_json::from_str::<Value>(body).unwrap(),
            "{name}"
        );
    }
    let grok = crate::cli::fork::load_fork_spec(repo.path(), &label("grok"))
        .unwrap()
        .unwrap();
    let intent = grok.grok.unwrap();
    assert_eq!(intent.child_id, "00000000-0000-4000-8000-000000000004");
    assert!(intent.prepared);
    let codex = crate::cli::fork::load_fork_spec(repo.path(), &label("codex"))
        .unwrap()
        .unwrap();
    assert_eq!(codex.tool, Tool::Codex);
    assert_eq!(
        codex.from_session.as_deref(),
        Some("00000000-0000-4000-8000-000000000002")
    );
    assert!(codex.grok.is_none());
}

#[test]
fn a_clone_forks_descriptor_reads() {
    let dest = tempfile::tempdir().unwrap();
    place(
        &crate::cli::fork::fork_descriptor_path(dest.path()),
        fixture!("fork-descriptor.json"),
    );
    let d = crate::cli::fork::load_fork_descriptor(dest.path())
        .unwrap()
        .unwrap();
    assert_eq!(
        (d.kind.as_str(), d.branch.as_str(), d.base.as_str()),
        ("clone", "spike", "0000000000000000000000000000000000000005")
    );
    assert_eq!(d.source, Path::new("/home/user/src/example"));
}

// ── a document this build cannot read is never overwritten ────────

const MALFORMED: &str = "{\"agents\": {\"claude\": ";
const TOO_NEW: &str = "{\"version\": 2, \"auto_mode\": \"on\", \"auto\": \"on\"}";
/// Valid JSON at the current version, with a known setting no reader
/// accepts: the transaction must refuse it before its edit, not leave
/// it for the next read to trip on.
const BAD_SETTING: &str = "{\"version\": 1, \"finish\": {\"autosquash\": \"yes\"}}";
const BAD_AGENT_SETTING: &str = "{\"version\": 1, \"auto_mode\": \"maybe\"}";

fn refused(what: &str, body: &str, path: &Path, result: anyhow::Result<()>) {
    let err = result.expect_err(what);
    assert!(
        err.chain().any(|e| e
            .downcast_ref::<clank_user_config::versioned::Refusal>()
            .is_some()),
        "{what}: {err:#}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), body, "{what}");
}

fn library(home: &Path) {
    crate::cli::team::mutate_user_config(home, |cfg| {
        cfg.agents.insert(
            label("claude"),
            crate::cli::teams_config::AgentDescription {
                tool: Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
        );
        let mut dev = crate::cli::teams_config::TeamRoster::new();
        dev.insert(
            label("claude"),
            crate::cli::teams_config::TeamMember::Ref(crate::cli::teams_config::TeamRef {
                agent: None,
                role: crate::cli::teams_config::RosterRole::Master,
            }),
        );
        cfg.teams.insert("dev".into(), dev);
        Ok(())
    })
    .unwrap();
}

fn desc() -> crate::cli::teams_config::AgentDescription {
    crate::cli::teams_config::AgentDescription {
        tool: Tool::Codex,
        launch: None,
        initial_prompt: None,
    }
}

#[test]
fn a_repo_config_it_cannot_read_stops_every_writer() {
    for body in [MALFORMED, TOO_NEW, BAD_SETTING] {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        library(home.path());
        let path = place(&crate::agent_store::repo_config::path(repo.path()), body);
        refused(
            "config set",
            body,
            &path,
            crate::cli::config::set_repo_key(repo.path(), &["diff", "wait"], "true", "bool"),
        );
        refused(
            "init --team",
            body,
            &path,
            crate::cli::init::register_repo_team_guarded(home.path(), repo.path(), "dev", true),
        );
        refused(
            "bare init",
            body,
            &path,
            crate::cli::init::seed_bare(Some(home.path()), repo.path()),
        );
        refused(
            "agent add",
            body,
            &path,
            crate::cli::agent::add_repo_roster_agent(
                repo.path(),
                &label("codex"),
                desc(),
                crate::cli::teams_config::RosterRole::Commit,
            ),
        );
        refused(
            "record_web_port",
            body,
            &path,
            crate::agent_store::record_web_port(repo.path(), 1234),
        );
        refused(
            "load",
            body,
            &path,
            crate::cli::config::load_with_home(repo.path(), None).map(|_| ()),
        );
    }
}

#[test]
fn a_user_config_it_cannot_read_stops_every_writer() {
    for body in [MALFORMED, TOO_NEW, BAD_SETTING] {
        let home = tempfile::tempdir().unwrap();
        let path = place(&clank_user_config::path(home.path()), body);
        refused(
            "config set --global",
            body,
            &path,
            crate::cli::config::set_user_key(home.path(), &["diff", "wait"], "true", "bool"),
        );
        refused(
            "team agent add",
            body,
            &path,
            crate::cli::agent::declare_global_agent(home.path(), &label("codex"), desc()),
        );
        refused(
            "trust-key",
            body,
            &path,
            crate::cli::user_command::trust_key(home.path(), "site", LEGACY_SITE_KEY, false)
                .map(|_| ()),
        );
        refused(
            "auto mode",
            body,
            &path,
            crate::cli::team::resolve_effective_auto_mode(None, Some(home.path())).map(|_| ()),
        );
        refused(
            "setup's first-run seed",
            body,
            &path,
            crate::cli::config::set_user_key_if_absent(
                home.path(),
                &["finish", "autosquash"],
                "true",
                "bool",
            )
            .map(|_| ()),
        );
        let door = clank_web_cloudflare::door::Door::new(
            Some(home.path().to_path_buf()),
            crate::cli::team::check_user_config,
        );
        refused("the web door", body, &path, door.token().map(|_| ()));
    }
}

#[test]
fn an_edit_that_leaves_a_setting_no_reader_accepts_is_never_written() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let repo_cfg = place(
        &crate::agent_store::repo_config::path(repo.path()),
        "{\"version\": 1, \"diff\": {\"wait\": true}}",
    );
    let user_cfg = place(
        &clank_user_config::path(home.path()),
        "{\"version\": 1, \"diff\": {\"wait\": true}}",
    );
    let key = ["recovery", "stall_warn"];
    refused(
        "config set",
        "{\"version\": 1, \"diff\": {\"wait\": true}}",
        &repo_cfg,
        crate::cli::config::set_repo_key(repo.path(), &key, "soon", "duration|off"),
    );
    refused(
        "config set --global",
        "{\"version\": 1, \"diff\": {\"wait\": true}}",
        &user_cfg,
        crate::cli::config::set_user_key(home.path(), &key, "soon", "duration|off"),
    );
}

/// The typed transactions, which every roster, init, port and fork writer
/// uses, check the edited document as a whole: the roster type keeps the
/// settings it does not own in `extra`, unchecked.
#[test]
fn a_typed_edit_that_breaks_a_setting_it_does_not_own_is_never_written() {
    use crate::agent_store::repo_config::{self, Prior};
    let body = "{\"version\": 1, \"diff\": {\"wait\": true}}";
    let repo = tempfile::tempdir().unwrap();
    let path = place(&repo_config::path(repo.path()), body);
    let break_finish = |cfg: &mut crate::cli::teams_config::RepoConfigFile| {
        cfg.extra
            .insert("finish".to_owned(), "not-an-object".into());
    };
    refused(
        "transact",
        body,
        &path,
        repo_config::transact(repo.path(), |cfg| {
            break_finish(cfg);
            Ok(())
        }),
    );
    refused(
        "replace",
        body,
        &path,
        repo_config::replace(repo.path(), |prior| {
            let Prior::Valid(mut cfg) = prior else {
                panic!("a valid config")
            };
            break_finish(&mut cfg);
            Ok((Some(*cfg), ()))
        }),
    );
}

/// An edit can't publish a legacy marker either: the write stamps a
/// version, and a versioned document in the old shape is refused on read.
#[test]
fn an_edit_that_adds_a_legacy_marker_is_never_written() {
    use crate::agent_store::repo_config;
    let body = "{\"version\": 1, \"diff\": {\"wait\": true}}";
    let repo = tempfile::tempdir().unwrap();
    let path = place(&repo_config::path(repo.path()), body);
    refused(
        "typed transact adding team",
        body,
        &path,
        repo_config::transact(repo.path(), |cfg| {
            cfg.extra.insert("team".to_owned(), "dev".into());
            Ok(())
        }),
    );
    refused(
        "raw transact adding promoted",
        body,
        &path,
        repo_config::transact_document(repo.path(), |doc| {
            doc.insert("promoted".to_owned(), "claude".into());
            Ok(())
        }),
    );
}

/// A document carrying a version was written by a clank that writes the
/// roster schema, so a damaged one is refused, never recreated; and a
/// roster with one entry missing its role is a damaged roster, not the
/// old schema.
#[test]
fn a_damaged_current_repo_config_is_refused_not_recreated_by_init() {
    for body in [
        "{\"version\": 1, \"agents\": {\"codex\": {\"tool\": \"codex\"}}, \"review\": {\"plan_feedback\": false}}",
        "{\"version\": 1, \"agents\": {\"claude\": {\"tool\": \"claude\", \"role\": \"master\"}, \"codex\": {\"tool\": \"codex\"}}}",
        "{\"version\": 1, \"team\": \"dev\"}",
        "{\"agents\": {\"claude\": {\"tool\": \"claude\", \"role\": \"master\"}, \"codex\": {\"tool\": \"codex\"}}}",
    ] {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        library(home.path());
        let path = place(&crate::agent_store::repo_config::path(repo.path()), body);
        refused(
            "bare init",
            body,
            &path,
            crate::cli::init::seed_bare(Some(home.path()), repo.path()),
        );
        refused(
            "init --team",
            body,
            &path,
            crate::cli::init::register_repo_team_guarded(home.path(), repo.path(), "dev", true),
        );
    }
}

#[test]
fn an_unversioned_role_less_roster_is_the_old_schema_init_recreates() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    library(home.path());
    place(
        &crate::agent_store::repo_config::path(repo.path()),
        "{\"agents\": {\"claude\": {\"tool\": \"claude\"}}}",
    );
    crate::cli::init::register_repo_team_guarded(home.path(), repo.path(), "dev", false).unwrap();
    let cfg = crate::agent_store::repo_config::read(repo.path())
        .unwrap()
        .unwrap();
    assert_eq!(
        cfg.agents[&label("claude")].role,
        crate::cli::teams_config::RosterRole::Master
    );
}

#[test]
fn an_agent_config_it_cannot_read_stops_every_writer() {
    for body in [MALFORMED, TOO_NEW, BAD_AGENT_SETTING] {
        let repo = tempfile::tempdir().unwrap();
        let l = label("claude");
        let path = place(
            &crate::agent_store::agent_config_path(repo.path(), &l),
            body,
        );
        let session = SessionId::parse(SESSION).unwrap();
        refused(
            "update_agent_config",
            body,
            &path,
            crate::agent_store::update_agent_config(repo.path(), &l, |c| {
                c.auto_mode = Some(AutoMode::Off)
            }),
        );
        refused(
            "set_auto_mode",
            body,
            &path,
            crate::agent_store::set_auto_mode(repo.path(), &l, AutoMode::Off),
        );
        refused(
            "bind",
            body,
            &path,
            crate::agent_store::bind_session_to_agent(repo.path(), &l, Tool::Claude, &session)
                .map(|_| ()),
        );
        refused(
            "release",
            body,
            &path,
            crate::agent_store::release_binding(repo.path(), &l, &session).map(|_| ()),
        );
    }
}

#[test]
fn a_missing_document_still_initializes() {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    crate::cli::config::set_repo_key(repo.path(), &["diff", "wait"], "true", "bool").unwrap();
    assert_eq!(
        raw(&crate::agent_store::repo_config::path(repo.path())),
        json!({"version": 1, "diff": {"wait": true}})
    );
    let fresh = tempfile::tempdir().unwrap();
    crate::cli::init::seed_bare(Some(home.path()), fresh.path()).unwrap();
    assert_eq!(
        raw(&crate::agent_store::repo_config::path(fresh.path())),
        json!({"version": 1})
    );
    crate::cli::config::set_user_key(home.path(), &["diff", "wait"], "true", "bool").unwrap();
    assert_eq!(
        raw(&clank_user_config::path(home.path())),
        json!({"version": 1, "diff": {"wait": true}})
    );
    crate::agent_store::set_auto_mode(repo.path(), &label("claude"), AutoMode::On).unwrap();
    assert_eq!(
        raw(&crate::agent_store::agent_config_path(
            repo.path(),
            &label("claude")
        )),
        json!({"version": 1, "auto_mode": "on"})
    );
}

// ── loud readers ──────────────────────────────────────────────────

#[tokio::test]
async fn status_shows_a_config_it_cannot_read_as_a_failed_line() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let repo_cfg = place(
        &crate::agent_store::repo_config::path(repo.path()),
        "{ not json",
    );
    let snap = crate::cli::status::snapshot(repo.path(), Some(home.path()))
        .await
        .unwrap();
    assert_eq!(snap.config_errors.len(), 1, "{:?}", snap.config_errors);
    assert!(snap.config_errors[0].contains(&repo_cfg.display().to_string()));
    let human = snap.to_human();
    assert!(human.contains("config: FAILED"), "{human}");

    std::fs::remove_file(&repo_cfg).unwrap();
    let user_cfg = place(&clank_user_config::path(home.path()), "{\"version\": 9}");
    let snap = crate::cli::status::snapshot(repo.path(), Some(home.path()))
        .await
        .unwrap();
    assert!(
        snap.config_errors
            .iter()
            .any(|e| e.contains(&user_cfg.display().to_string()) && e.contains("newer clank")),
        "{:?}",
        snap.config_errors
    );
}

#[tokio::test]
async fn status_marks_an_agent_whose_config_it_cannot_read() {
    for body in ["{ not json", "{\"version\": 999, \"auto_mode\": \"on\"}"] {
        let repo = git_repo();
        let home = tempfile::tempdir().unwrap();
        crate::cli::agent::add_repo_roster_agent(
            repo.path(),
            &label("codex"),
            desc(),
            crate::cli::teams_config::RosterRole::Master,
        )
        .unwrap();
        let agent_cfg = place(
            &crate::agent_store::agent_config_path(repo.path(), &label("codex")),
            body,
        );
        let snap = crate::cli::status::snapshot(repo.path(), Some(home.path()))
            .await
            .unwrap();
        let shown = agent_cfg.display().to_string();
        assert!(
            snap.config_errors.iter().any(|e| e.contains(&shown)),
            "{body}: {:?}",
            snap.config_errors
        );
        let row = &snap.agents[0];
        assert!(
            row.unreadable
                .as_deref()
                .is_some_and(|why| why.contains(&shown)),
            "{body}: the row is marked, not shown as an agent that is off"
        );
        let sources = crate::cli::source_status::configured(repo.path());
        assert!(
            sources.is_err_and(|e| format!("{e:#}").contains(&shown)),
            "{body}: an unreadable agent is not one with no sources"
        );
        let json = snap.to_json();
        assert!(json["agents"][0]["unreadable"].is_string(), "{json}");
        assert!(json["config_errors"].is_array(), "{json}");
    }
}

#[test]
fn doctor_reports_each_config_document() {
    use crate::cli::doctor::{CheckStatus, config_checks};
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let status_of = |path: &Path| {
        let name = path.display().to_string();
        config_checks(Some(repo.path()), Some(home.path()))
            .into_iter()
            .find(|c| c.name == name)
            .map(|c| (c.status, c.message))
            .unwrap_or_else(|| panic!("no check for {name}"))
    };
    let repo_cfg = crate::agent_store::repo_config::path(repo.path());
    let user_cfg = clank_user_config::path(home.path());
    assert!(status_of(&repo_cfg).1.starts_with("missing"));
    assert!(status_of(&user_cfg).1.starts_with("missing"));

    place(&repo_cfg, "{ not json");
    place(&user_cfg, "{\"version\": 9}");
    let agent_cfg = place(
        &crate::agent_store::agent_config_path(repo.path(), &label("claude")),
        "{\"auto_mode\": \"on\"}",
    );
    let (status, message) = status_of(&repo_cfg);
    assert_eq!(status, CheckStatus::Fail);
    assert!(message.starts_with("malformed"), "{message}");
    let (status, message) = status_of(&user_cfg);
    assert_eq!(status, CheckStatus::Fail);
    assert!(message.starts_with("too new"), "{message}");
    assert_eq!(status_of(&agent_cfg), (CheckStatus::Ok, "ok".to_string()));
}
