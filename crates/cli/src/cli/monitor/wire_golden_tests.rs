//! The monitor wire's shape, pinned to the protocol that speaks it.
//!
//! Every type that crosses the monitor socket has one sample function
//! here. An enum's function matches each sample with no wildcard, and a
//! struct's (or struct variant's) destructures it with no `..`, so a new
//! variant or field does not compile until it has a sample. An integer's
//! sample is its type's `MAX`, so a change of width does not compile
//! either. The samples' JSON is hashed and compared with [`PINS`].

#![deny(unreachable_patterns)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use clank_core::SilentReason;
use clank_core::agent_config::{TimerGeneration, TimerRecurrence};
use clank_core::hook_io::BackgroundTask;
use clank_core::ids::{AgentLabel, CommitSha, PlanKey, SessionId};
use clank_core::turn_policy::{
    ChildReport, ChildSeen, Clearing, PresentationId, ReadTo, ShellProof, TaskSnapshot,
    TurnEndReport, TurnPlace,
};
use clank_core::vocab::{CommitGateState, Tool, WaitingReason};
use clank_core::wait::{
    AnswerRef, CommandReplyRoute, ContentRef, EventPresentation, HeadTagViolation, MasterNext,
    MessageReplyRoute, WaitItem,
};
use serde_json::{Value, json};

use super::{Control, Hello, Holder, Note, PROTO, Welcome, Who};
use crate::cli::question_state::{Key, Observation};
use crate::cli::recovery::{Reply, ReplyAnswer};
use crate::cli::turn_wire::{Frame, Report, TurnResult, WireVerdict};
use crate::cli::watch_store::{Producer, WatchStorage};

/// Every `(PROTO, hash)` the wire has been pinned at, oldest first.
///
/// Append a pair when the shape changes; never edit one in place. The
/// test cannot tell an in-place edit from a bump, so review holds it.
const PINS: &[(u32, &str)] = &[
    (
        24,
        "522c39c1769fe65c06da3128cf039906f35f1b45b2b16fc696d4b3d73baada11",
    ),
    (
        25,
        "547dd6eccf30d864bb65cfb4e89ae03e4e0bc6211406fb397496833bbbce3244",
    ),
];

fn session() -> SessionId {
    SessionId::parse("session-1").unwrap()
}

fn sha() -> CommitSha {
    CommitSha::parse("abc1234").unwrap()
}

fn plan() -> PlanKey {
    PlanKey::parse("some-plan").unwrap()
}

fn hellos() -> Vec<Hello> {
    let samples = vec![Hello {
        proto: PROTO,
        repo: "/w/repo".into(),
        who: whos().remove(0),
        build: Some("build-1".into()),
    }];
    for s in &samples {
        let Hello {
            proto: _,
            repo: _,
            who: _,
            build: _,
        } = s;
    }
    samples
}

fn holders() -> Vec<Holder> {
    let samples = vec![Holder {
        tool: Tool::Claude,
        session: "session-1".into(),
    }];
    for s in &samples {
        let Holder {
            tool: _,
            session: _,
        } = s;
    }
    samples
}

fn whos() -> Vec<Who> {
    let samples = vec![
        Who::Agent {
            author: "alice".into(),
            holder: Some(holders().remove(0)),
            has_cli_sources: true,
            turn_end: Some(reports().remove(0)),
        },
        Who::Observer,
        Who::RecoveryHelper {
            author: "alice".into(),
            holder: holders().remove(0),
        },
        Who::Control {
            request: controls().remove(0),
        },
    ];
    for s in &samples {
        match s {
            Who::Agent {
                author: _,
                holder: _,
                has_cli_sources: _,
                turn_end: _,
            } => {}
            Who::Observer => {}
            Who::RecoveryHelper {
                author: _,
                holder: _,
            } => {}
            Who::Control { request: _ } => {}
        }
    }
    samples
}

fn controls() -> Vec<Control> {
    let samples = vec![
        Control::Questions {
            author: "alice".into(),
            session: session(),
            observation: observations().remove(0),
        },
        Control::Restart {
            author: producers().remove(1),
            key: "source-key".into(),
            accepted: "fingerprint".into(),
        },
        Control::Run {
            author: "alice".into(),
            session: session(),
            desc: "build".into(),
            expect_ms: u64::MAX,
            proof: shell_proofs().remove(0),
        },
        Control::Attend {
            author: "alice".into(),
            session: session(),
            name: "subagent".into(),
            desc: Some("reviewing".into()),
            expect_ms: u64::MAX,
        },
        Control::Clear {
            author: "alice".into(),
            session: session(),
            which: clearings().remove(1),
        },
        Control::Observe {
            author: "alice".into(),
            tool: Tool::Codex,
            session: "session-1".into(),
            turn: json!({"opaque": [1, "two"]}),
        },
        Control::RecoveryReply {
            author: "alice".into(),
            tool: Tool::Claude,
            reply: replies().remove(0),
        },
    ];
    for s in &samples {
        match s {
            Control::Questions {
                author: _,
                session: _,
                observation: _,
            } => {}
            Control::Restart {
                author: _,
                key: _,
                accepted: _,
            } => {}
            Control::Run {
                author: _,
                session: _,
                desc: _,
                expect_ms: _,
                proof: _,
            } => {}
            Control::Attend {
                author: _,
                session: _,
                name: _,
                desc: _,
                expect_ms: _,
            } => {}
            Control::Clear {
                author: _,
                session: _,
                which: _,
            } => {}
            Control::Observe {
                author: _,
                tool: _,
                session: _,
                turn: _,
            } => {}
            Control::RecoveryReply {
                author: _,
                tool: _,
                reply: _,
            } => {}
        }
    }
    samples
}

fn welcomes() -> Vec<Welcome> {
    let samples = vec![
        Welcome::Ok { proto: PROTO },
        Welcome::Decline {
            proto: PROTO,
            why: "a newer client".into(),
        },
    ];
    for s in &samples {
        match s {
            Welcome::Ok { proto: _ } => {}
            Welcome::Decline { proto: _, why: _ } => {}
        }
    }
    samples
}

fn notes() -> Vec<Note> {
    let samples = vec![
        Note::Refused {
            why: "not admitted".into(),
        },
        Note::Superseded,
        Note::Items {
            items: wait_items(),
            source_cwd: Some(PathBuf::from("/w/repo")),
        },
        Note::Recover {
            opened_by: "opener".into(),
            n: u32::MAX,
            tag: "retry".into(),
            wake: "wake text".into(),
        },
        Note::Done {
            what: "restarted".into(),
        },
        Note::Verdict(frames().remove(0)),
        Note::Beat,
    ];
    for s in &samples {
        match s {
            Note::Refused { why: _ } => {}
            Note::Superseded => {}
            Note::Items {
                items: _,
                source_cwd: _,
            } => {}
            Note::Recover {
                opened_by: _,
                n: _,
                tag: _,
                wake: _,
            } => {}
            Note::Done { what: _ } => {}
            Note::Verdict(_) => {}
            Note::Beat => {}
        }
    }
    samples
}

fn reports() -> Vec<Report> {
    let samples = vec![
        Report::TurnEnd {
            stop: turn_end_reports().remove(0),
            deadline: u64::MAX,
            questions: observations(),
        },
        Report::Compacted {
            stop: turn_end_reports().remove(0),
            deadline: u64::MAX,
        },
        Report::SessionStart {
            tool: Tool::Claude,
            session: session(),
            deadline: u64::MAX,
        },
        Report::Handoff {
            session: session(),
            continued: true,
            tasks: task_snapshots().remove(0),
            hook_started: Some(u64::MAX),
            deadline: u64::MAX,
        },
        Report::Child {
            tool: Tool::Claude,
            seen: child_reports().remove(0),
            deadline: u64::MAX,
        },
    ];
    for s in &samples {
        match s {
            Report::TurnEnd {
                stop: _,
                deadline: _,
                questions: _,
            } => {}
            Report::Compacted {
                stop: _,
                deadline: _,
            } => {}
            Report::SessionStart {
                tool: _,
                session: _,
                deadline: _,
            } => {}
            Report::Handoff {
                session: _,
                continued: _,
                tasks: _,
                hook_started: _,
                deadline: _,
            } => {}
            Report::Child {
                tool: _,
                seen: _,
                deadline: _,
            } => {}
        }
    }
    samples
}

fn wire_verdicts() -> Vec<WireVerdict> {
    let samples = vec![
        WireVerdict::Park,
        WireVerdict::Silent {
            why: SilentReason::WaitArmed,
        },
        WireVerdict::Deliver {
            presentation: presentation_ids().remove(0),
        },
        WireVerdict::Notify,
        WireVerdict::RenewWait,
        WireVerdict::Recorded,
        WireVerdict::Diagnostic {
            message: "a diagnostic".into(),
        },
        WireVerdict::Unanswered {
            why: "no verdict in time".into(),
        },
    ];
    for s in &samples {
        match s {
            WireVerdict::Park => {}
            WireVerdict::Silent { why: _ } => {}
            WireVerdict::Deliver { presentation: _ } => {}
            WireVerdict::Notify => {}
            WireVerdict::RenewWait => {}
            WireVerdict::Recorded => {}
            WireVerdict::Diagnostic { message: _ } => {}
            WireVerdict::Unanswered { why: _ } => {}
        }
    }
    samples
}

fn frames() -> Vec<Frame> {
    let samples = vec![Frame {
        verdict: wire_verdicts().remove(2),
        rendered: Some("rendered work".into()),
    }];
    for s in &samples {
        let Frame {
            verdict: _,
            rendered: _,
        } = s;
    }
    samples
}

fn turn_results() -> Vec<TurnResult> {
    let samples = vec![
        TurnResult::Delivered {
            presentation: presentation_ids().remove(0),
        },
        TurnResult::EmitFailed {
            presentation: presentation_ids().remove(0),
            why: "stdout closed".into(),
        },
    ];
    for s in &samples {
        match s {
            TurnResult::Delivered { presentation: _ } => {}
            TurnResult::EmitFailed {
                presentation: _,
                why: _,
            } => {}
        }
    }
    samples
}

fn observations() -> Vec<Observation> {
    let samples = vec![
        Observation::Asked {
            key: keys().remove(0),
            header: "Which one?".into(),
            input: "input-hash".into(),
        },
        Observation::Result {
            key: keys().remove(0),
        },
        Observation::TurnOver {
            session: "session-1".into(),
            prompt: "prompt-1".into(),
        },
        Observation::Behind {
            session: "session-1".into(),
            older: vec!["prompt-0".into()],
        },
    ];
    for s in &samples {
        match s {
            Observation::Asked {
                key: _,
                header: _,
                input: _,
            } => {}
            Observation::Result { key: _ } => {}
            Observation::TurnOver {
                session: _,
                prompt: _,
            } => {}
            Observation::Behind {
                session: _,
                older: _,
            } => {}
        }
    }
    samples
}

fn keys() -> Vec<Key> {
    let samples = vec![Key {
        session: "session-1".into(),
        prompt: "prompt-1".into(),
        tool_use: "toolu-1".into(),
    }];
    for s in &samples {
        let Key {
            session: _,
            prompt: _,
            tool_use: _,
        } = s;
    }
    samples
}

fn replies() -> Vec<Reply> {
    let samples = vec![Reply {
        session: "session-1".into(),
        opened_by: "opener".into(),
        n: u32::MAX,
        answer: ReplyAnswer::Submitted,
        started: Some("turn-2".into()),
    }];
    for s in &samples {
        let Reply {
            session: _,
            opened_by: _,
            n: _,
            answer: _,
            started: _,
        } = s;
    }
    samples
}

fn reply_answers() -> Vec<ReplyAnswer> {
    let samples = vec![
        ReplyAnswer::Submitted,
        ReplyAnswer::Refused,
        ReplyAnswer::Indeterminate,
    ];
    for s in &samples {
        match s {
            ReplyAnswer::Submitted => {}
            ReplyAnswer::Refused => {}
            ReplyAnswer::Indeterminate => {}
        }
    }
    samples
}

fn shell_proofs() -> Vec<ShellProof> {
    let samples = vec![ShellProof {
        pid: i32::MAX,
        token: "birth-token".into(),
    }];
    for s in &samples {
        let ShellProof { pid: _, token: _ } = s;
    }
    samples
}

fn clearings() -> Vec<Clearing> {
    let samples = vec![
        Clearing::All,
        Clearing::Named("subagent".into()),
        Clearing::Run(shell_proofs().remove(0)),
    ];
    for s in &samples {
        match s {
            Clearing::All => {}
            Clearing::Named(_) => {}
            Clearing::Run(_) => {}
        }
    }
    samples
}

fn turn_end_reports() -> Vec<TurnEndReport> {
    let samples = vec![TurnEndReport {
        tool: Tool::Claude,
        session: session(),
        place: turn_places().remove(0),
        tasks: task_snapshots().remove(0),
        hook_started: Some(u64::MAX),
    }];
    for s in &samples {
        let TurnEndReport {
            tool: _,
            session: _,
            place: _,
            tasks: _,
            hook_started: _,
        } = s;
    }
    samples
}

fn turn_places() -> Vec<TurnPlace> {
    let samples = vec![TurnPlace {
        turn: Some("turn-1".into()),
        position: Some(u64::MAX),
        human_turn: true,
        read_to: Some(read_tos().remove(0)),
    }];
    for s in &samples {
        let TurnPlace {
            turn: _,
            position: _,
            human_turn: _,
            read_to: _,
        } = s;
    }
    samples
}

fn read_tos() -> Vec<ReadTo> {
    let samples = vec![ReadTo {
        file: u64::MAX,
        len: u64::MAX,
    }];
    for s in &samples {
        let ReadTo { file: _, len: _ } = s;
    }
    samples
}

fn task_snapshots() -> Vec<TaskSnapshot> {
    let samples = vec![
        TaskSnapshot::Known(background_tasks()),
        TaskSnapshot::Unavailable,
    ];
    for s in &samples {
        match s {
            TaskSnapshot::Known(_) => {}
            TaskSnapshot::Unavailable => {}
        }
    }
    samples
}

/// Only `raw` reaches the wire; the other fields are read from it.
fn background_tasks() -> Vec<BackgroundTask> {
    let samples = vec![BackgroundTask::from(json!({
        "id": "task-1",
        "status": "running",
        "type": "subagent",
        "description": "reviewer",
        "agent_type": "general",
    }))];
    for s in &samples {
        let BackgroundTask {
            id: _,
            status: _,
            kind: _,
            description: _,
            raw: _,
        } = s;
    }
    samples
}

fn child_reports() -> Vec<ChildReport> {
    let samples = vec![
        ChildReport {
            session: session(),
            child: "task-1".into(),
            hook_started: Some(u64::MAX),
            seen: child_seens().remove(1),
        },
        ChildReport {
            session: session(),
            child: "task-2".into(),
            hook_started: None,
            seen: child_seens().remove(0),
        },
    ];
    for s in &samples {
        let ChildReport {
            session: _,
            child: _,
            hook_started: _,
            seen: _,
        } = s;
    }
    samples
}

fn child_seens() -> Vec<ChildSeen> {
    let samples = vec![
        ChildSeen::Started,
        ChildSeen::Stopped {
            tasks: task_snapshots().remove(0),
            agent: turn_places().remove(0),
        },
    ];
    for s in &samples {
        match s {
            ChildSeen::Started => {}
            ChildSeen::Stopped { tasks: _, agent: _ } => {}
        }
    }
    samples
}

fn presentation_ids() -> Vec<PresentationId> {
    let samples = vec![PresentationId {
        session: session(),
        seq: u64::MAX,
    }];
    for s in &samples {
        let PresentationId { session: _, seq: _ } = s;
    }
    samples
}

fn silent_reasons() -> Vec<SilentReason> {
    let samples = vec![
        SilentReason::AutoOff,
        SilentReason::StaleBinding,
        SilentReason::OwnerGone,
        SilentReason::WaitArmed,
        SilentReason::UserCommand,
    ];
    for s in &samples {
        match s {
            SilentReason::AutoOff => {}
            SilentReason::StaleBinding => {}
            SilentReason::OwnerGone => {}
            SilentReason::WaitArmed => {}
            SilentReason::UserCommand => {}
        }
    }
    samples
}

fn producers() -> Vec<Producer> {
    let samples = vec![
        Producer::Agent(AgentLabel::parse("watch:1").unwrap()),
        Producer::Watch {
            id: u64::MAX,
            storage: watch_storages().remove(1),
        },
    ];
    for s in &samples {
        match s {
            Producer::Agent(_) => {}
            Producer::Watch { id: _, storage: _ } => {}
        }
    }
    samples
}

fn watch_storages() -> Vec<WatchStorage> {
    let samples = vec![
        WatchStorage::Repo,
        WatchStorage::Agent(AgentLabel::parse("alice").unwrap()),
    ];
    for s in &samples {
        match s {
            WatchStorage::Repo => {}
            WatchStorage::Agent(_) => {}
        }
    }
    samples
}

fn tools() -> Vec<Tool> {
    let samples = vec![Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode];
    for s in &samples {
        match s {
            Tool::Claude => {}
            Tool::Codex => {}
            Tool::Grok => {}
            Tool::OpenCode => {}
        }
    }
    samples
}

fn wait_items() -> Vec<WaitItem> {
    let samples = vec![
        WaitItem::Master {
            plan: plan(),
            sha: sha(),
            next: MasterNext::Revise,
            reason: WaitingReason::AddressCommitChanges,
            gate: CommitGateState::ChangesRequested,
        },
        WaitItem::Reviewer {
            plan: plan(),
            sha: sha(),
            feedback_path: ".clank/feedback/x.md".into(),
        },
        WaitItem::Idle {
            prompt: "idle prompt".into(),
        },
        WaitItem::AdHocReview {
            sha: sha(),
            feedback_path: ".clank/feedback/y.md".into(),
        },
        WaitItem::AdHocRevise { sha: sha() },
        WaitItem::FixCommitTag {
            sha: sha(),
            violation: head_tag_violations().remove(0),
        },
        WaitItem::PromoteFromQueue {
            name: "queued".into(),
            priority: u16::MAX,
        },
        WaitItem::Message {
            presentation: Some(event_presentations().remove(0)),
            handle: "msg-1".into(),
            from: Some("bob".into()),
            repo: Some("/w/other".into()),
            subject: "hello".into(),
            body: "hello\nthere".into(),
            requires_reply: true,
            reply_route: Some(message_reply_routes().remove(0)),
            selector: Some("messages@1".into()),
        },
        WaitItem::GithubEvent {
            presentation: Some(event_presentations().remove(0)),
            repo: "owner/name".into(),
            event: "pr_comment".into(),
            detail: Some("review_comment".into()),
            number: Some(u64::MAX),
            title: Some("A PR".into()),
            actor: Some("carol".into()),
            url: Some("https://example.invalid/pr/12".into()),
            instructions: Some("look".into()),
            content: Some(content_refs().remove(3)),
        },
        WaitItem::TimerEvent {
            presentation: Some(event_presentations().remove(0)),
            name: "tick".into(),
            id: "tick-1".into(),
            due_at: u64::MAX,
            content: "time".into(),
            recurrence: Some(timer_recurrences().remove(0)),
            selector: Some("timer@4".into()),
        },
        WaitItem::CommandEvent {
            presentation: Some(event_presentations().remove(0)),
            source_cwd: Some(PathBuf::from("/w/repo")),
            name: "cmd".into(),
            id: "line-1".into(),
            event: "kind".into(),
            content: "output".into(),
            at: Some(u64::MAX),
            failure: true,
            requires_reply: true,
            reply_route: Some(command_reply_routes().remove(0)),
            selector: Some("cmd@7".into()),
        },
        WaitItem::ForCommit {
            sha: sha(),
            subject: "subject".into(),
        },
        WaitItem::ForFinished {
            plan: plan(),
            sha: sha(),
        },
        WaitItem::ForBlocked {
            name: "block".into(),
        },
        WaitItem::Blocked {
            name: "block".into(),
            question: "why?".into(),
        },
        WaitItem::Unblocked {
            answer: "because".into(),
            carries: answer_refs().remove(0),
        },
    ];
    for s in &samples {
        match s {
            WaitItem::Master {
                plan: _,
                sha: _,
                next: _,
                reason: _,
                gate: _,
            } => {}
            WaitItem::Reviewer {
                plan: _,
                sha: _,
                feedback_path: _,
            } => {}
            WaitItem::Idle { prompt: _ } => {}
            WaitItem::AdHocReview {
                sha: _,
                feedback_path: _,
            } => {}
            WaitItem::AdHocRevise { sha: _ } => {}
            WaitItem::FixCommitTag {
                sha: _,
                violation: _,
            } => {}
            WaitItem::PromoteFromQueue {
                name: _,
                priority: _,
            } => {}
            WaitItem::Message {
                presentation: _,
                handle: _,
                from: _,
                repo: _,
                subject: _,
                body: _,
                requires_reply: _,
                reply_route: _,
                selector: _,
            } => {}
            WaitItem::GithubEvent {
                presentation: _,
                repo: _,
                event: _,
                detail: _,
                number: _,
                title: _,
                actor: _,
                url: _,
                instructions: _,
                content: _,
            } => {}
            WaitItem::TimerEvent {
                presentation: _,
                name: _,
                id: _,
                due_at: _,
                content: _,
                recurrence: _,
                selector: _,
            } => {}
            WaitItem::CommandEvent {
                presentation: _,
                source_cwd: _,
                name: _,
                id: _,
                event: _,
                content: _,
                at: _,
                failure: _,
                requires_reply: _,
                reply_route: _,
                selector: _,
            } => {}
            WaitItem::ForCommit { sha: _, subject: _ } => {}
            WaitItem::ForFinished { plan: _, sha: _ } => {}
            WaitItem::ForBlocked { name: _ } => {}
            WaitItem::Blocked {
                name: _,
                question: _,
            } => {}
            WaitItem::Unblocked {
                answer: _,
                carries: _,
            } => {}
        }
    }
    samples
}

fn master_nexts() -> Vec<MasterNext> {
    let samples = vec![
        MasterNext::Revise,
        MasterNext::Commit,
        MasterNext::Continue,
        MasterNext::Finalize,
    ];
    for s in &samples {
        match s {
            MasterNext::Revise => {}
            MasterNext::Commit => {}
            MasterNext::Continue => {}
            MasterNext::Finalize => {}
        }
    }
    samples
}

fn waiting_reasons() -> Vec<WaitingReason> {
    let samples = WaitingReason::ALL.to_vec();
    for s in &samples {
        match s {
            WaitingReason::SessionFinished => {}
            WaitingReason::CommitPlanRevision => {}
            WaitingReason::AddressCommitChanges => {}
            WaitingReason::ReadyToFinalize => {}
            WaitingReason::GateContinue => {}
            WaitingReason::CommitNeedsReview => {}
        }
    }
    samples
}

fn commit_gate_states() -> Vec<CommitGateState> {
    let samples = vec![
        CommitGateState::Unreviewed,
        CommitGateState::Continued,
        CommitGateState::Finished,
        CommitGateState::ChangesRequested,
        CommitGateState::Blocked,
        CommitGateState::ContinuedPendingGate,
    ];
    for s in &samples {
        match s {
            CommitGateState::Unreviewed => {}
            CommitGateState::Continued => {}
            CommitGateState::Finished => {}
            CommitGateState::ChangesRequested => {}
            CommitGateState::Blocked => {}
            CommitGateState::ContinuedPendingGate => {}
        }
    }
    samples
}

fn head_tag_violations() -> Vec<HeadTagViolation> {
    let samples = vec![HeadTagViolation {
        unknown: Some("nope".into()),
        untagged_touched: vec![plan()],
        extra_named: Some(PlanKey::parse("other-plan").unwrap()),
    }];
    for s in &samples {
        let HeadTagViolation {
            unknown: _,
            untagged_touched: _,
            extra_named: _,
        } = s;
    }
    samples
}

fn event_presentations() -> Vec<EventPresentation> {
    let samples = vec![EventPresentation {
        source: "source".into(),
        seq: u64::MAX,
        ident: "ident".into(),
    }];
    for s in &samples {
        let EventPresentation {
            source: _,
            seq: _,
            ident: _,
        } = s;
    }
    samples
}

fn message_reply_routes() -> Vec<MessageReplyRoute> {
    let samples = vec![MessageReplyRoute {
        repo: PathBuf::from("/w/other"),
        agent: AgentLabel::parse("bob").unwrap(),
    }];
    for s in &samples {
        let MessageReplyRoute { repo: _, agent: _ } = s;
    }
    samples
}

fn command_reply_routes() -> Vec<CommandReplyRoute> {
    let samples = vec![CommandReplyRoute {
        command: vec!["reply".into(), "--x".into()],
        cwd: PathBuf::from("/w/repo"),
        state_dir: Some(PathBuf::from("/w/state")),
    }];
    for s in &samples {
        let CommandReplyRoute {
            command: _,
            cwd: _,
            state_dir: _,
        } = s;
    }
    samples
}

fn content_refs() -> Vec<ContentRef> {
    let samples = vec![
        ContentRef::IssueComment { id: u64::MAX },
        ContentRef::PrIssueComment { id: u64::MAX },
        ContentRef::ReviewComment { id: u64::MAX },
        ContentRef::Review {
            number: u64::MAX,
            id: u64::MAX,
        },
        ContentRef::Pr { number: u64::MAX },
        ContentRef::Issue { number: u64::MAX },
    ];
    for s in &samples {
        match s {
            ContentRef::IssueComment { id: _ } => {}
            ContentRef::PrIssueComment { id: _ } => {}
            ContentRef::ReviewComment { id: _ } => {}
            ContentRef::Review { number: _, id: _ } => {}
            ContentRef::Pr { number: _ } => {}
            ContentRef::Issue { number: _ } => {}
        }
    }
    samples
}

fn timer_recurrences() -> Vec<TimerRecurrence> {
    let samples = vec![TimerRecurrence {
        generation: timer_generations().remove(0),
        generation_id: "gen-1".into(),
        first_slot: u64::MAX,
        last_slot: u64::MAX,
        count: u64::MAX,
    }];
    for s in &samples {
        let TimerRecurrence {
            generation: _,
            generation_id: _,
            first_slot: _,
            last_slot: _,
            count: _,
        } = s;
    }
    samples
}

fn timer_generations() -> Vec<TimerGeneration> {
    let samples = vec![TimerGeneration {
        name: "tick".into(),
        scope: Some("scope".into()),
        anchor: u64::MAX,
        interval: u64::MAX,
    }];
    for s in &samples {
        let TimerGeneration {
            name: _,
            scope: _,
            anchor: _,
            interval: _,
        } = s;
    }
    samples
}

fn answer_refs() -> Vec<AnswerRef> {
    let samples = vec![AnswerRef {
        name: "block".into(),
        occurrence: Some("occ-1".into()),
        question_digest: "qd".into(),
        answer_digest: "ad".into(),
    }];
    for s in &samples {
        let AnswerRef {
            name: _,
            occurrence: _,
            question_digest: _,
            answer_digest: _,
        } = s;
    }
    samples
}

fn samples<T: serde::Serialize>(of: Vec<T>) -> Vec<Value> {
    of.iter()
        .map(|s| serde_json::to_value(s).unwrap())
        .collect()
}

/// Every wire type's samples, by name. `PROTO` is in them, so a bump with
/// no change of shape is still a new pin.
fn document() -> Vec<(&'static str, Vec<Value>)> {
    vec![
        ("Hello", samples(hellos())),
        ("Holder", samples(holders())),
        ("Who", samples(whos())),
        ("Control", samples(controls())),
        ("Welcome", samples(welcomes())),
        ("Note", samples(notes())),
        ("Report", samples(reports())),
        ("WireVerdict", samples(wire_verdicts())),
        ("Frame", samples(frames())),
        ("TurnResult", samples(turn_results())),
        ("question_state::Observation", samples(observations())),
        ("question_state::Key", samples(keys())),
        ("recovery::Reply", samples(replies())),
        ("recovery::ReplyAnswer", samples(reply_answers())),
        ("ShellProof", samples(shell_proofs())),
        ("Clearing", samples(clearings())),
        ("TurnEndReport", samples(turn_end_reports())),
        ("TurnPlace", samples(turn_places())),
        ("ReadTo", samples(read_tos())),
        ("TaskSnapshot", samples(task_snapshots())),
        ("BackgroundTask", samples(background_tasks())),
        ("ChildReport", samples(child_reports())),
        ("ChildSeen", samples(child_seens())),
        ("PresentationId", samples(presentation_ids())),
        ("SilentReason", samples(silent_reasons())),
        ("Tool", samples(tools())),
        ("WaitItem", samples(wait_items())),
        ("MasterNext", samples(master_nexts())),
        ("WaitingReason", samples(waiting_reasons())),
        ("CommitGateState", samples(commit_gate_states())),
        ("HeadTagViolation", samples(head_tag_violations())),
        ("EventPresentation", samples(event_presentations())),
        ("MessageReplyRoute", samples(message_reply_routes())),
        ("CommandReplyRoute", samples(command_reply_routes())),
        ("ContentRef", samples(content_refs())),
        ("TimerRecurrence", samples(timer_recurrences())),
        ("TimerGeneration", samples(timer_generations())),
        ("AnswerRef", samples(answer_refs())),
        ("Producer", samples(producers())),
        ("WatchStorage", samples(watch_storages())),
    ]
}

/// Object keys sorted, so the hash follows what a peer decodes, not the
/// order fields are declared in.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), canonical(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

fn wire_hash() -> String {
    let doc: Vec<Value> = document()
        .into_iter()
        .map(|(name, samples)| json!([name, samples]))
        .collect();
    blake3::hash(canonical(&Value::Array(doc)).as_bytes())
        .to_hex()
        .to_string()
}

#[test]
fn the_wire_shape_is_pinned_to_the_protocol() {
    let hash = wire_hash();
    let &(pinned, pin) = PINS.last().expect("PINS is never empty");
    if pinned == PROTO {
        assert_eq!(
            pin, hash,
            "the monitor wire changed but PROTO is still {PROTO}: bump PROTO in monitor/wire.rs \
             and append the (PROTO, hash) pair this test then reports to PINS; never edit \
             an existing pair"
        );
    } else {
        panic!(
            "PROTO is {PROTO} but the last pin is for {pinned}: append \
             ({PROTO}, \"{hash}\") to PINS"
        );
    }
}

#[test]
fn pins_are_a_history() {
    for pair in PINS.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "PINS must be strictly increasing in PROTO: {pair:?}"
        );
    }
    let hashes: BTreeSet<&str> = PINS.iter().map(|(_, h)| *h).collect();
    assert_eq!(hashes.len(), PINS.len(), "a hash appears twice in PINS");
}

enum Tagging {
    External,
    Internal(&'static str),
}

/// The variant tags `T`'s own deserializer names, read from the error it
/// gives for one it does not know.
fn declared<T: serde::de::DeserializeOwned>(tagging: &Tagging) -> BTreeSet<String> {
    let probe = match tagging {
        Tagging::External => json!("\u{0}probe"),
        Tagging::Internal(tag) => json!({ *tag: "\u{0}probe" }),
    };
    let err = serde_json::from_value::<T>(probe)
        .err()
        .expect("a probe tag is no variant")
        .to_string();
    let (_, expected) = err
        .split_once("expected")
        .unwrap_or_else(|| panic!("not an unknown-variant error: {err}"));
    expected
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

fn sampled(samples: &[Value], tagging: &Tagging) -> BTreeSet<String> {
    samples
        .iter()
        .map(|v| match (tagging, v) {
            (Tagging::External, Value::String(s)) => s.clone(),
            (Tagging::External, Value::Object(m)) if m.len() == 1 => {
                m.keys().next().unwrap().clone()
            }
            (Tagging::Internal(tag), Value::Object(m)) => m[*tag].as_str().unwrap().to_owned(),
            (_, other) => panic!("untagged sample {other}"),
        })
        .collect()
}

/// The exhaustive matches make a new variant name a sample arm; this makes
/// the sample itself present, which the match alone cannot.
#[test]
fn every_variant_has_a_sample() {
    use Tagging::{External, Internal};
    fn check<T: serde::Serialize + serde::de::DeserializeOwned>(
        name: &str,
        of: Vec<T>,
        tagging: Tagging,
    ) -> Option<String> {
        let want = declared::<T>(&tagging);
        let have = sampled(&samples(of), &tagging);
        (want != have).then(|| format!("{name}: declares {want:?}, sampled {have:?}"))
    }
    let problems: Vec<String> = [
        check("Who", whos(), Internal("as")),
        check("Control", controls(), Internal("do")),
        check("Welcome", welcomes(), Internal("t")),
        check("Note", notes(), Internal("t")),
        check("Report", reports(), Internal("report")),
        check("WireVerdict", wire_verdicts(), Internal("v")),
        check("TurnResult", turn_results(), Internal("result")),
        check("Observation", observations(), Internal("seen")),
        check("ReplyAnswer", reply_answers(), External),
        check("Clearing", clearings(), External),
        check("TaskSnapshot", task_snapshots(), Internal("snapshot")),
        check("ChildSeen", child_seens(), External),
        check("SilentReason", silent_reasons(), External),
        check("Tool", tools(), External),
        check("WaitItem", wait_items(), Internal("kind")),
        check("MasterNext", master_nexts(), External),
        check("WaitingReason", waiting_reasons(), External),
        check("CommitGateState", commit_gate_states(), External),
        check("ContentRef", content_refs(), Internal("ref")),
        check("Producer", producers(), External),
        check("WatchStorage", watch_storages(), External),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(problems.is_empty(), "{problems:#?}");
}
