//! Whether clank may resume an agent's harness on its own, and how.
//!
//! Resuming means starting the agent's next turn without interrupting
//! anything: never an interrupt key or RPC, only a submission that the
//! harness itself queues behind a running turn rather than run over it.
//! Which harnesses do that is decided by the tool alone; [`EVIDENCE`]
//! records what was seen that makes each typed candidate safe.
//!
//! What no candidate protects: a draft a person types after clank's last
//! look at the pane. A typed submission presses its submit key, so in that
//! race it sends the draft early with clank's hint appended. Nothing is
//! cancelled.

use clank_core::Tool;

/// How clank may start a harness's next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Type a hint into the agent's pane and submit it, with the key the
    /// harness queues on (`pane_input::submit_async`).
    TypedSubmit,
    /// Hand a prompt to the harness's own plugin (OpenCode's
    /// `prompt_async`), through a recovery helper.
    Plugin,
}

/// How clank may resume a harness of `tool`, if at all.
pub(crate) fn candidate(tool: Tool) -> Option<Resume> {
    match tool {
        Tool::Claude | Tool::Codex => Some(Resume::TypedSubmit),
        Tool::OpenCode => Some(Resume::Plugin),
        // Typed input cancels a running Grok turn (D-grok:418).
        Tool::Grok => None,
    }
}

/// One observation of a harness's candidate, during a running turn: the
/// turn finished, and the submission ran after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) tool: Tool,
    pub(crate) resume: Resume,
    /// The version observed, as the harness reports it.
    pub(crate) from: &'static str,
    /// What was done and seen, including the race it does not cover.
    pub(crate) how: &'static str,
}

/// The committed evidence.
// The record a person reads to know why a candidate is enabled; nothing
// executes it, and only its tests read it as code.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const EVIDENCE: &[Evidence] = &[
    Evidence {
        tool: Tool::Claude,
        resume: Resume::TypedSubmit,
        from: "2.1.280",
        how: "Claude Code 2.1.280 in a scratch directory, observed 2026-10-13: a turn \
              writing a 1500-word story with no tools was running when `write-chars` and \
              a CR were sent to its pane, as submit_async does. Claude showed the line as \
              queued (\"Press up to edit queued messages\"), the story ran to its end, and \
              the queued line ran as the next turn. A person's draft typed after clank's \
              last look is submitted with the hint.",
    },
    Evidence {
        tool: Tool::Codex,
        resume: Resume::TypedSubmit,
        from: "0.161.0",
        how: "codex-cli 0.161.0 in a scratch directory, observed 2026-10-13: with a \
              1500-word story turn running, `write-chars` then Tab (0x09) showed \
              \"Queued follow-up inputs\"; the story ran to its end and the queued line \
              ran next. Idle, the same Tab submitted at once. Enter (the kitty submit) \
              in the same running turn instead steered the line into it, and the story \
              was never written: Codex is resumed with Tab, never Enter. A person's draft \
              typed after clank's last look is submitted with the hint.",
    },
];

/// Why a harness is not resumed.
pub(crate) const NO_CANDIDATE: &str = "this harness has no way to resume that cancels nothing";

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude and Codex are typed into, OpenCode is handed to its plugin,
    /// and Grok is never resumed.
    #[test]
    fn the_tool_decides_the_candidate() {
        assert_eq!(candidate(Tool::Claude), Some(Resume::TypedSubmit));
        assert_eq!(candidate(Tool::Codex), Some(Resume::TypedSubmit));
        assert_eq!(candidate(Tool::OpenCode), Some(Resume::Plugin));
        assert_eq!(candidate(Tool::Grok), None);
    }

    /// Every typed candidate is explained by an observation, and the
    /// evidence names only what the candidates use.
    #[test]
    fn the_evidence_explains_each_typed_candidate() {
        for tool in [Tool::Claude, Tool::Codex] {
            assert!(EVIDENCE.iter().any(|e| e.tool == tool), "{tool:?}");
        }
        for e in EVIDENCE {
            assert_eq!(candidate(e.tool), Some(e.resume), "{:?}", e.tool);
        }
    }
}
