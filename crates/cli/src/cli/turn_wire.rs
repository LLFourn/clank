//! The frames a turn travels in: the report a provider hook's `clank wait`
//! child carries in its hello, the verdicts the monitor answers with, and
//! the one delivery result the child forwards on the same connection.

use clank_core::turn_policy::{ChildReport, Ms, PresentationId, TaskSnapshot, TurnEndReport};
use clank_core::{HookOutcome, SessionId, SilentReason, Tool};

/// What a hook reports to the monitor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "report", rename_all = "snake_case")]
pub(crate) enum Report {
    TurnEnd {
        stop: TurnEndReport,
        /// Unix ms past which the hook can no longer hold its wait.
        deadline: Ms,
        /// What the Stop proves about the questions the agent asked.
        #[serde(default)]
        questions: Vec<super::question_state::Observation>,
    },
    /// Claude's PostCompact: a compaction ends the park the last Stop
    /// armed, so the hook arms it again. It ends no turn and proves
    /// nothing about the agent's questions, so it is stepped as the park
    /// it replaces.
    Compacted { stop: TurnEndReport, deadline: Ms },
    SessionStart {
        tool: Tool,
        session: SessionId,
        deadline: Ms,
    },
    /// Grok's native Stop, answered at once and never parked. Whether its
    /// background wait is running is the monitor's own registration, not
    /// anything the hook saw.
    Handoff {
        session: SessionId,
        /// This Stop continues the turn that was asked to arm the wait.
        continued: bool,
        /// Grok's `backgroundTasks`, read only for declarations.
        #[serde(default)]
        tasks: TaskSnapshot,
        /// When the hook started, by its own clock.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hook_started: Option<Ms>,
        deadline: Ms,
    },
    /// Claude's SubagentStop or SubagentStart: evidence about what the
    /// agent declared, recorded and acknowledged at once. It is no Stop
    /// of the agent's, holds nothing for its session and is told nothing
    /// to pass on.
    Child {
        tool: Tool,
        seen: ChildReport,
        deadline: Ms,
    },
}

impl Report {
    pub(crate) fn tool(&self) -> Tool {
        match self {
            Report::TurnEnd { stop, .. } | Report::Compacted { stop, .. } => stop.tool,
            Report::SessionStart { tool, .. } | Report::Child { tool, .. } => *tool,
            Report::Handoff { .. } => Tool::Grok,
        }
    }

    pub(crate) fn session(&self) -> &SessionId {
        match self {
            Report::TurnEnd { stop, .. } | Report::Compacted { stop, .. } => &stop.session,
            Report::SessionStart { session, .. } => session,
            Report::Handoff { session, .. } => session,
            Report::Child { seen, .. } => &seen.session,
        }
    }

    pub(crate) fn deadline(&self) -> Ms {
        match self {
            Report::TurnEnd { deadline, .. }
            | Report::Compacted { deadline, .. }
            | Report::SessionStart { deadline, .. }
            | Report::Handoff { deadline, .. }
            | Report::Child { deadline, .. } => *deadline,
        }
    }

    /// A session start, a handoff or a child's report is answered once:
    /// none parks.
    pub(crate) fn parks(&self) -> bool {
        matches!(self, Report::TurnEnd { .. } | Report::Compacted { .. })
    }

    /// Whether its connection stands for its session's wait, taking the
    /// place of any older one. A handoff reports on that very wait, and a
    /// child's report is beside it: holding the session would end the
    /// park either one leaves alone.
    pub(crate) fn holds_session(&self) -> bool {
        !matches!(self, Report::Handoff { .. } | Report::Child { .. })
    }

    /// Whether its hook passes on what it is told to the agent. A child's
    /// hook does not, so nothing meant for the agent may be spent on it.
    pub(crate) fn reaches_agent(&self) -> bool {
        !matches!(self, Report::Child { .. })
    }
}

/// What the hook is told to do, already reduced to what its wire can say.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "v", rename_all = "snake_case")]
pub(crate) enum WireVerdict {
    /// Not terminal for a Stop: the wait holds, and another verdict
    /// follows on the same connection.
    Park,
    Silent {
        why: SilentReason,
    },
    /// Wake with the rendered work. Answered with one [`TurnResult`].
    Deliver {
        presentation: PresentationId,
    },
    /// Wake with rendered text that is not work: nothing is presented.
    Notify,
    /// OpenCode's plugin renews its long-poll without a model turn.
    RenewWait,
    /// A child's report was taken: there is nothing to do or say.
    Recorded,
    Diagnostic {
        message: String,
    },
}

/// One verdict and the text it carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Frame {
    pub(crate) verdict: WireVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rendered: Option<String>,
}

impl Frame {
    pub(crate) fn bare(verdict: WireVerdict) -> Self {
        Frame {
            verdict,
            rendered: None,
        }
    }

    pub(crate) fn text(verdict: WireVerdict, rendered: String) -> Self {
        Frame {
            verdict,
            rendered: Some(rendered),
        }
    }

    /// Whether this frame ends a report's exchange.
    pub(crate) fn terminal(&self, parks: bool) -> bool {
        !parks || self.verdict != WireVerdict::Park
    }

    /// The presentation this frame hands over, which its result answers.
    pub(crate) fn presentation(&self) -> Option<&PresentationId> {
        match &self.verdict {
            WireVerdict::Deliver { presentation } => Some(presentation),
            _ => None,
        }
    }

    /// The Stop outcome this frame is: the hook's whole mapping, before
    /// the per-tool framing.
    pub(crate) fn outcome(&self) -> HookOutcome {
        let text = || self.rendered.clone().unwrap_or_default();
        match &self.verdict {
            WireVerdict::Park => HookOutcome::Diagnostic {
                message: "hook: the wait ended while still parked".into(),
            },
            WireVerdict::Silent { why } => HookOutcome::Silent { why: *why },
            WireVerdict::Deliver { .. } | WireVerdict::Notify => {
                HookOutcome::Continue { reason: text() }
            }
            WireVerdict::RenewWait => HookOutcome::RenewWait,
            WireVerdict::Recorded => HookOutcome::Diagnostic {
                message: "hook: a child's acknowledgement reached a Stop".into(),
            },
            WireVerdict::Diagnostic { message } => HookOutcome::Diagnostic {
                message: message.clone(),
            },
        }
    }
}

/// How emitting a presentation went, from the connection that carried it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum TurnResult {
    Delivered {
        presentation: PresentationId,
    },
    EmitFailed {
        presentation: PresentationId,
        why: String,
    },
}

impl TurnResult {
    pub(crate) fn presentation(&self) -> &PresentationId {
        match self {
            TurnResult::Delivered { presentation }
            | TurnResult::EmitFailed { presentation, .. } => presentation,
        }
    }
}
