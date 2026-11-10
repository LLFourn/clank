use super::*;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const MESSAGE: &str = "event messages@1 k:message#claude/00000000000000000000bb01";
const REVIEW: &str = "reviewer plan=example-plan sha=0000000000000000000000000000000000000001 next= reason= name= handle=";

fn message() -> WaitItem {
    WaitItem::Message {
        presentation: None,
        handle: "h".into(),
        from: Some("claude".into()),
        repo: None,
        subject: "s".into(),
        body: "b".into(),
        requires_reply: false,
        reply_route: None,
        selector: None,
    }
}

fn review() -> WaitItem {
    WaitItem::Reviewer {
        plan: clank_core::ids::PlanKey::parse("example-plan").unwrap(),
        sha: clank_core::ids::CommitSha::parse("0000000000000000000000000000000000000001").unwrap(),
        feedback_path: "f.md".into(),
    }
}

fn admitted(identity: &str, item: WaitItem, standing: Standing, arrived: Option<u64>) -> Admission {
    Admission {
        identity: identity.into(),
        pausable: matches!(item, WaitItem::Reviewer { .. }),
        item,
        standing,
        arrived,
    }
}

const BLOCK: &str = "claude/stork-retakes";

fn withheld() -> Standing {
    Standing::Withheld {
        by: vec![BLOCK.into()],
    }
}

fn none() -> Ended {
    Ended::new()
}

fn ended(at: u64) -> Ended {
    [(BLOCK.to_string(), at)].into()
}

fn obligation_of(ledger: &Ledger, identity: &str) -> Obligation {
    ledger
        .entries
        .iter()
        .find(|e| e.identity == identity)
        .map_or(Obligation::Discharged, |e| e.obligation.clone())
}

/// Review work a block withholds is not owed, and a message beside it is.
/// When the block lifts, the review is owed from the moment the block's
/// own record says it ended, not from its arrival and not from the fold
/// that noticed: time withheld is never charged, and time owed always is.
#[test]
fn withheld_work_is_not_owed_and_is_owed_from_the_blocks_end() {
    let blocked = [admitted(REVIEW, review(), withheld(), Some(100))];
    let under = fold(
        &Obligations::new(),
        &blocked,
        &Evidence::default(),
        &none(),
        5000,
    );
    assert!(under.owed().is_empty(), "withheld alone owes nothing");
    assert_eq!(under.owed_since(), None);
    assert_eq!(
        obligation_of(&under, REVIEW),
        Obligation::Withheld {
            by: vec![BLOCK.into()]
        }
    );

    let with_message = [
        admitted(MESSAGE, message(), Standing::Owed, Some(4000)),
        admitted(REVIEW, review(), withheld(), Some(100)),
    ];
    let both = fold(
        &under.obligations(),
        &with_message,
        &Evidence::default(),
        &none(),
        6000,
    );
    let owed = both.owed();
    assert_eq!(owed.len(), 1, "only the message: {owed:?}");
    assert_eq!(
        (owed[0].entry.what.as_str(), owed[0].owed_since),
        ("message from claude", Since::At(4000))
    );

    let released = [
        admitted(MESSAGE, message(), Standing::Owed, Some(4000)),
        admitted(REVIEW, review(), Standing::Owed, Some(100)),
    ];
    let after = fold(
        &both.obligations(),
        &released,
        &Evidence::default(),
        &ended(7000),
        9000,
    );
    assert_eq!(
        obligation_of(&after, REVIEW),
        Obligation::Owed {
            owed_since: Since::At(7000)
        },
        "owed from the block's end: not its arrival at 100, not the fold at 9000"
    );
    let later = fold(
        &after.obligations(),
        &released,
        &Evidence::default(),
        &ended(7000),
        9999,
    );
    assert_eq!(obligation_of(&later, REVIEW), obligation_of(&after, REVIEW));
    assert_eq!(later.owed_since(), Some(4000), "the message is the oldest");
    assert_eq!(
        obligation_of(&later, MESSAGE),
        Obligation::Owed {
            owed_since: Since::At(4000)
        },
        "a block's end moves nothing a block cannot withhold"
    );
}

/// The transitions of one obligation, each from what the admission rule
/// said, what the ledger held before, and when the blocks ended.
#[test]
fn an_obligation_follows_the_admission_rule() {
    let owed = |at| Obligation::Owed {
        owed_since: Since::At(at),
    };
    let unknown = Obligation::Owed {
        owed_since: Since::Unknown { held: 50 },
    };
    let held = |by: &[&str]| Obligation::Withheld {
        by: by.iter().map(|b| b.to_string()).collect(),
    };
    let two: Ended = [("a/x".to_string(), 30), ("b/y".to_string(), 40)].into();
    let work = |standing, arrived| admitted(REVIEW, review(), standing, arrived);
    let event = |arrived| admitted(MESSAGE, message(), Standing::Owed, arrived);
    for (before, admission, ended, expect, why) in [
        (None, Some(event(Some(7))), none(), owed(7), "an arrival"),
        (
            Some(owed(7)),
            Some(event(Some(9))),
            two.clone(),
            owed(7),
            "an event keeps its clock, whatever blocks ended",
        ),
        (
            Some(held(&["a/x", "b/y"])),
            Some(work(Standing::Owed, Some(7))),
            two.clone(),
            owed(40),
            "released when the last of its blocks ended",
        ),
        (
            Some(held(&["a/x"])),
            Some(work(Standing::Owed, Some(35))),
            [("a/x".to_string(), 30)].into(),
            owed(35),
            "never before it arrived",
        ),
        (
            Some(held(&["a/x", "gone/z"])),
            Some(work(Standing::Owed, Some(7))),
            two.clone(),
            unknown.clone(),
            "a block whose end cannot be dated leaves the release undated",
        ),
        (
            None,
            Some(work(Standing::Owed, Some(7))),
            two.clone(),
            owed(40),
            "never seen withheld, and a block ended after it arrived",
        ),
        (
            None,
            Some(work(Standing::Owed, Some(45))),
            two.clone(),
            owed(45),
            "a block that ended before it arrived withheld none of it",
        ),
        (
            Some(owed(7)),
            Some(work(Standing::Owed, Some(7))),
            two.clone(),
            owed(40),
            "a block asked and ended between two folds",
        ),
        (
            Some(unknown.clone()),
            Some(work(Standing::Owed, Some(7))),
            two.clone(),
            unknown.clone(),
            "an undated release is not re-dated from the arrival it followed",
        ),
        (
            Some(owed(7)),
            Some(work(
                Standing::Withheld {
                    by: vec!["a/x".into()],
                },
                Some(7),
            )),
            none(),
            held(&["a/x"]),
            "withheld again",
        ),
        (Some(owed(7)), None, none(), Obligation::Discharged, "gone"),
        (
            None,
            Some(admitted(MESSAGE, message(), Standing::Context, None)),
            none(),
            Obligation::Discharged,
            "context is never owed",
        ),
        (
            None,
            Some(event(None)),
            none(),
            unknown.clone(),
            "no record dates it",
        ),
    ] {
        assert_eq!(
            obligation(before.as_ref(), admission.as_ref(), &ended, 50),
            expect,
            "{why}"
        );
    }
}

/// An item the fold no longer admits is discharged and leaves the ledger.
#[test]
fn an_item_the_fold_drops_is_discharged() {
    let had = fold(
        &Obligations::new(),
        &[admitted(MESSAGE, message(), Standing::Owed, Some(10))],
        &Evidence::default(),
        &none(),
        20,
    );
    assert_eq!(had.owed().len(), 1);
    let acked = fold(&had.obligations(), &[], &Evidence::default(), &none(), 30);
    assert_eq!(acked, Ledger::default());
}

/// An item no record dates is owed since an unknown moment. Neither a
/// later fold nor a restart gives it one: what is kept is when the ledger
/// first held it, which bounds its age and is never shown as it.
#[test]
fn an_undated_item_stays_undated_across_folds_and_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let undated = [admitted(MESSAGE, message(), Standing::Owed, None)];
    let first = fold_stored(dir.path(), &undated, &Evidence::default(), &none(), 1000);
    assert_eq!(first.owed()[0].owed_since, Since::Unknown { held: 1000 });
    assert_eq!(first.owed_since(), None, "nothing dates the agent's debt");
    let restarted = fold_stored(dir.path(), &undated, &Evidence::default(), &none(), 90_000);
    assert_eq!(
        restarted.owed()[0].owed_since,
        Since::Unknown { held: 1000 },
        "a monitor that starts later does not date it from its own start"
    );
    assert_eq!(Since::Unknown { held: 1000 }.known(), None);
}

/// A block answered while no monitor ran. The monitor that starts forty
/// minutes later finds the stored ledger saying the work was withheld and
/// the fold saying it is owed: the release is the block's own end, forty
/// minutes ago, not the restart. Where the block's end cannot be read the
/// release is undated, and stays so; it is never the restart either.
#[test]
fn a_release_while_no_monitor_ran_is_dated_by_the_block_not_the_restart() {
    const ANSWERED: u64 = 100_000;
    const RESTART: u64 = ANSWERED + 40 * 60;
    let blocked = [
        admitted(MESSAGE, message(), Standing::Owed, Some(4000)),
        admitted(REVIEW, review(), withheld(), Some(100)),
    ];
    let released = [
        admitted(MESSAGE, message(), Standing::Owed, Some(4000)),
        admitted(REVIEW, review(), Standing::Owed, Some(100)),
    ];
    let dir = tempfile::tempdir().unwrap();
    let before = fold_stored(dir.path(), &blocked, &Evidence::default(), &none(), 5000);
    assert_eq!(
        load(dir.path()),
        before.obligations(),
        "kept for the next monitor"
    );

    let after = fold_stored(
        dir.path(),
        &released,
        &Evidence::default(),
        &ended(ANSWERED),
        RESTART,
    );
    assert_eq!(
        obligation_of(&after, REVIEW),
        Obligation::Owed {
            owed_since: Since::At(ANSWERED)
        },
        "forty minutes owed already, none of it forgiven by the restart"
    );
    assert_eq!(
        obligation_of(&after, MESSAGE),
        Obligation::Owed {
            owed_since: Since::At(4000)
        }
    );
    let again = fold_stored(
        dir.path(),
        &released,
        &Evidence::default(),
        &ended(ANSWERED),
        RESTART + 300,
    );
    assert_eq!(obligation_of(&again, REVIEW), obligation_of(&after, REVIEW));

    let cleaned = tempfile::tempdir().unwrap();
    fold_stored(
        cleaned.path(),
        &blocked,
        &Evidence::default(),
        &none(),
        5000,
    );
    let undated = fold_stored(
        cleaned.path(),
        &released,
        &Evidence::default(),
        &none(),
        RESTART,
    );
    assert_eq!(
        obligation_of(&undated, REVIEW),
        Obligation::Owed {
            owed_since: Since::Unknown { held: RESTART }
        },
        "a block cleaned away dates nothing: unknown, not the restart"
    );
    assert_eq!(undated.owed_since(), Some(4000), "and it ages no clock");
    let later = fold_stored(
        cleaned.path(),
        &released,
        &Evidence::default(),
        &ended(ANSWERED),
        RESTART + 9000,
    );
    assert_eq!(
        obligation_of(&later, REVIEW),
        obligation_of(&undated, REVIEW)
    );

    fold_stored(
        dir.path(),
        &[],
        &Evidence::default(),
        &none(),
        RESTART + 9000,
    );
    assert!(!path(dir.path()).exists(), "nothing owed keeps no file");
}

/// A ledger stored before each withholding block was named apart kept
/// them as one line, and still says which they were.
#[test]
fn a_ledger_that_kept_its_blocks_as_one_line_still_names_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        path(dir.path()),
        r#"[{"identity":"a","withheld":"claude/stork, codex/q (answered, not yet received)"},{"identity":"b","withheld":["claude/stork"]}]"#,
    )
    .unwrap();
    let stored = load(dir.path());
    assert_eq!(
        stored["a"],
        Obligation::Withheld {
            by: vec!["claude/stork".into(), "codex/q".into()]
        }
    );
    assert_eq!(
        stored["b"],
        Obligation::Withheld {
            by: vec!["claude/stork".into()]
        }
    );
}

/// Delivery is what the evidence proves and nothing else: a receipt is a
/// delivery, a presentation still inside its bound is in flight, and one
/// past it, failed or never made is no delivery at all.
#[test]
fn delivery_is_only_what_the_evidence_proves() {
    let none = Evidence::default();
    assert_eq!(delivery(MESSAGE, &none, 10), Delivery::NotDelivered);

    let flying = Evidence {
        delivered: Default::default(),
        in_flight: Some((vec![MESSAGE.into()], 70)),
    };
    assert_eq!(delivery(MESSAGE, &flying, 69), Delivery::InFlight(70));
    assert_eq!(
        delivery(MESSAGE, &flying, 70),
        Delivery::NotDelivered,
        "a carrier that never answered did not deliver"
    );
    assert_eq!(
        delivery(REVIEW, &flying, 10),
        Delivery::NotDelivered,
        "what the presentation does not carry is not on its way"
    );

    let received = Evidence {
        delivered: [(MESSAGE.to_string(), 50)].into(),
        in_flight: None,
    };
    assert_eq!(delivery(MESSAGE, &received, 10_000), Delivery::Delivered);

    let ledger = fold(
        &Obligations::new(),
        &[
            admitted(MESSAGE, message(), Standing::Owed, Some(5)),
            admitted(REVIEW, review(), Standing::Owed, Some(6)),
        ],
        &Evidence {
            delivered: [(MESSAGE.to_string(), 50)].into(),
            in_flight: Some((vec![REVIEW.into()], 70)),
        },
        &Ended::new(),
        60,
    );
    assert_eq!(ledger.last_delivery, Some(50));
    assert_eq!(ledger.in_flight_until(60), Some(70));
    assert_eq!(ledger.in_flight_until(70), None, "expired");
    assert_eq!(
        ledger.owed().len(),
        2,
        "delivered or not, both are still owed"
    );
}

/// A receipt is one session's. After a rebind the old session's receipts
/// prove nothing about the new one, and the new session's first receipt
/// replaces them rather than joining them.
#[test]
fn a_receipt_from_another_session_counts_for_nothing() {
    use crate::cli::presentation::{delivered_to, receive};
    use clank_core::Tool;
    let repo = tempfile::tempdir().unwrap();
    let agent = clank_core::AgentLabel::parse("agent").unwrap();
    let dir = crate::agent_store::agents_root(repo.path()).join("agent");
    let bind = |session: &str| {
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &agent,
            Tool::Codex,
            &clank_core::SessionId::parse(session).unwrap(),
        )
        .unwrap();
    };
    let record = |session: &str, identity: &str, at| {
        let guard = crate::agent_store::hold_binding(repo.path(), &agent).unwrap();
        receive(
            &guard,
            Tool::Codex,
            session,
            &[identity.to_string()],
            at,
            &crate::upgrade::process().settling(),
        )
        .unwrap()
    };
    bind("old-session");
    assert!(record("old-session", MESSAGE, 50));
    assert!(delivered_to(&dir, "old-session").contains_key(MESSAGE));
    bind("new-session");
    let rebound = Evidence {
        delivered: delivered_to(&dir, "new-session"),
        in_flight: None,
    };
    assert_eq!(
        delivery(MESSAGE, &rebound, 60),
        Delivery::NotDelivered,
        "the new binding was never shown it"
    );
    assert!(record("new-session", REVIEW, 70));
    assert_eq!(
        delivered_to(&dir, "new-session"),
        [(REVIEW.to_string(), 70)].into()
    );
    assert!(
        delivered_to(&dir, "old-session").is_empty(),
        "the old session's receipts are gone, not merged"
    );
    assert!(record("new-session", REVIEW, 99));
    assert_eq!(
        delivered_to(&dir, "new-session")[REVIEW],
        70,
        "a repeat keeps the first delivery's time"
    );
}

/// An event log written before deliveries named their session says a
/// message was presented, and to nobody in particular: with no receipt
/// for the bound session it is not delivered to it. The stored receipts
/// and ledger keep reading as they were written.
#[test]
fn old_presentation_bytes_prove_nothing_about_the_current_binding() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("events");
    std::fs::create_dir_all(&events).unwrap();
    std::fs::write(
        events.join("messages.jsonl"),
        include_str!("../../tests/fixtures/formats/events-presented.jsonl"),
    )
    .unwrap();
    let log = crate::cli::github::log::EventLog::open(&events, "messages").unwrap();
    let rows = log.read_rows().unwrap().unwrap().rows;
    assert_eq!(rows[0].woken_at, Some(1790648060), "the log says presented");
    let evidence = Evidence {
        delivered: crate::cli::presentation::delivered_to(dir.path(), SESSION),
        in_flight: None,
    };
    assert_eq!(
        delivery(MESSAGE, &evidence, 1790650000),
        Delivery::NotDelivered
    );

    std::fs::write(
        crate::cli::presentation::receipts_path(dir.path()),
        include_str!("../../tests/fixtures/formats/delivered.json"),
    )
    .unwrap();
    let delivered = crate::cli::presentation::delivered_to(dir.path(), SESSION);
    assert_eq!(delivered[MESSAGE], 1790648060);
    assert_eq!(delivered[REVIEW], 1790649000);

    std::fs::write(
        path(dir.path()),
        include_str!("../../tests/fixtures/formats/ledger.json"),
    )
    .unwrap();
    let stored = load(dir.path());
    assert_eq!(
        stored[MESSAGE],
        Obligation::Owed {
            owed_since: Since::At(1790648000)
        }
    );
    assert_eq!(
        stored[REVIEW],
        Obligation::Withheld {
            by: vec!["claude/stork-retakes".into()]
        }
    );
    assert_eq!(
        stored["promote_from_queue plan= sha= next= reason= name=next-plan handle="],
        Obligation::Owed {
            owed_since: Since::Unknown { held: 1790650000 }
        }
    );
}

/// Each kind of item says what it is in a row's words.
#[test]
fn every_item_names_itself() {
    assert_eq!(what(&message()).as_deref(), Some("message from claude"));
    assert_eq!(kind(&message()), "message");
    assert_eq!(what(&review()).as_deref(), Some("review of 0000000"));
    assert_eq!(kind(&review()), "review");
    let blocked = WaitItem::Blocked {
        agent: "claude".into(),
        name: "stork".into(),
        question: "?".into(),
    };
    assert_eq!(what(&blocked), None, "a notice names no work");
}
