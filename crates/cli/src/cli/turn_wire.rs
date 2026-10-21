//! The frames a turn travels in: the report a provider hook's `clank wait`
//! child carries in its hello, the verdicts the monitor answers with, and
//! the one delivery result the child forwards on the same connection.

use clank_core::turn_policy::{Handoff, Ms, PresentationId, TurnEndReport};
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
    SessionStart {
        tool: Tool,
        session: SessionId,
        deadline: Ms,
    },
    /// Grok's native Stop, answered at once and never parked.
    Handoff { handoff: Handoff, deadline: Ms },
}

impl Report {
    pub(crate) fn tool(&self) -> Tool {
        match self {
            Report::TurnEnd { stop, .. } => stop.tool,
            Report::SessionStart { tool, .. } => *tool,
            Report::Handoff { .. } => Tool::Grok,
        }
    }

    pub(crate) fn session(&self) -> &SessionId {
        match self {
            Report::TurnEnd { stop, .. } => &stop.session,
            Report::SessionStart { session, .. } => session,
            Report::Handoff { handoff, .. } => &handoff.session,
        }
    }

    pub(crate) fn deadline(&self) -> Ms {
        match self {
            Report::TurnEnd { deadline, .. }
            | Report::SessionStart { deadline, .. }
            | Report::Handoff { deadline, .. } => *deadline,
        }
    }

    /// A session start or a handoff is answered once: neither parks.
    pub(crate) fn parks(&self) -> bool {
        matches!(self, Report::TurnEnd { .. })
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
