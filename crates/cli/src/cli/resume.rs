//! Whether clank may resume an agent's harness on its own, and how.
//!
//! Resuming means starting the agent's next turn without interrupting
//! anything: never an interrupt key or RPC, only a submission that the
//! harness itself queues behind a running turn rather than run over it.
//! Which harnesses do that is a fact about the harness at its version, so
//! a candidate is enabled only by committed evidence for it.
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

/// The candidate each harness could use, before any evidence.
pub(crate) fn candidate(tool: Tool) -> Option<Resume> {
    match tool {
        Tool::Claude | Tool::Codex => Some(Resume::TypedSubmit),
        Tool::OpenCode => Some(Resume::Plugin),
        // Typed input cancels a running Grok turn (D-grok:418).
        Tool::Grok => None,
    }
}

/// What a submission during a running turn was seen to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The running turn finished, and the submission ran after it.
    Queues,
    /// The running turn was cut off or redirected.
    Aborts,
}

/// One observation of a harness's candidate, at one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) tool: Tool,
    pub(crate) resume: Resume,
    /// The version observed, as the harness reports it.
    pub(crate) from: &'static str,
    /// A policy, not an observation: later versions are assumed to behave
    /// the same until a later row says otherwise.
    pub(crate) later: bool,
    pub(crate) seen: Seen,
    /// What was done and seen, including the race it does not cover.
    pub(crate) how: &'static str,
}

/// The committed evidence.
pub(crate) const EVIDENCE: &[Evidence] = &[
    Evidence {
        tool: Tool::Claude,
        resume: Resume::TypedSubmit,
        from: "2.1.280",
        later: true,
        seen: Seen::Queues,
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
        later: true,
        seen: Seen::Queues,
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
pub(crate) const NO_EVIDENCE: &str =
    "resuming this harness is not enabled: nothing shows this version queues rather than cancels";
pub(crate) const ABORTS: &str = "typed input was seen to abort a running turn of this version";
pub(crate) const NO_VERSION: &str = "its version is not known yet";
pub(crate) const BAD_VERSION: &str = "its version could not be read";

/// What the evidence allows, and on what grounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) resume: Resume,
    /// `None` when observed at exactly this version; the version observed
    /// when allowed beyond it only by the later-versions policy.
    pub(crate) policy_beyond: Option<&'static str>,
}

/// How clank may resume `tool` at `version` by `evidence`, or why not.
///
/// Among the rows for its candidate whose version is at or below
/// `version`, the one observed latest decides, an `Aborts` beating a
/// `Queues` of the same version: a newer contradicting observation
/// revokes what an older one allowed. A `Queues` row covers its own
/// version, and later ones only by its policy.
pub(crate) fn resume_in(
    evidence: &[Evidence],
    tool: Tool,
    version: Option<&str>,
) -> Result<Grant, &'static str> {
    let resume = candidate(tool).ok_or(NO_CANDIDATE)?;
    let version = version.ok_or(NO_VERSION)?;
    let v = parse(version).ok_or(BAD_VERSION)?;
    let aborts = |e: &Evidence| e.seen == Seen::Aborts;
    let (from, decides) = evidence
        .iter()
        .filter(|e| e.tool == tool && e.resume == resume)
        .filter_map(|e| Some((parse(e.from)?, e)))
        .filter(|(from, _)| *from <= v)
        .max_by(|(a, x), (b, y)| a.cmp(b).then(aborts(x).cmp(&aborts(y))))
        .ok_or(NO_EVIDENCE)?;
    match decides.seen {
        Seen::Aborts => Err(ABORTS),
        Seen::Queues if from == v => Ok(Grant {
            resume,
            policy_beyond: None,
        }),
        Seen::Queues if decides.later => Ok(Grant {
            resume,
            policy_beyond: Some(decides.from),
        }),
        Seen::Queues => Err(NO_EVIDENCE),
    }
}

/// How clank may resume `tool`, running at `version`, by the committed
/// evidence.
pub(crate) fn resume_for(tool: Tool, version: Option<&str>) -> Result<Grant, &'static str> {
    resume_in(EVIDENCE, tool, version)
}

/// `version`'s dotted numbers, ignoring a leading `v` and any suffix after
/// them.
fn parse(version: &str) -> Option<Vec<u64>> {
    let numbers: Vec<u64> = version
        .trim()
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()?
        .split('.')
        .map(|s| s.parse().ok())
        .collect::<Option<_>>()?;
    (!numbers.is_empty()).then_some(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEUES: Evidence = Evidence {
        tool: Tool::Claude,
        resume: Resume::TypedSubmit,
        from: "2.1.280",
        later: true,
        seen: Seen::Queues,
        how: "test",
    };

    /// No evidence, no resume, for every harness; Grok never has one.
    #[test]
    fn nothing_is_resumed_without_evidence() {
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
            assert_eq!(resume_in(&[], tool, Some("2.1.280")), Err(NO_EVIDENCE));
        }
        assert_eq!(
            resume_in(&[QUEUES], Tool::Grok, Some("1.0.46")),
            Err(NO_CANDIDATE)
        );
    }

    /// Below the observed version nothing is allowed; at it, by
    /// observation; beyond it, by the policy, which names itself.
    #[test]
    fn a_row_covers_its_version_and_later_ones_by_policy() {
        let e = [QUEUES];
        assert_eq!(
            resume_in(&e, Tool::Claude, Some("2.1.279")),
            Err(NO_EVIDENCE)
        );
        assert_eq!(
            resume_in(&e, Tool::Claude, Some("2.1.280")),
            Ok(Grant {
                resume: Resume::TypedSubmit,
                policy_beyond: None
            })
        );
        assert_eq!(
            resume_in(&e, Tool::Claude, Some("2.3.0")).map(|g| g.policy_beyond),
            Ok(Some("2.1.280"))
        );
        let pinned = [Evidence {
            later: false,
            ..QUEUES
        }];
        assert_eq!(
            resume_in(&pinned, Tool::Claude, Some("2.3.0")),
            Err(NO_EVIDENCE)
        );
        assert_eq!(
            resume_in(&e, Tool::Codex, Some("2.1.280")),
            Err(NO_EVIDENCE)
        );
    }

    /// A later `Aborts` revokes the earlier allowance from its version on,
    /// though the earlier row still matches; and it wins a tie.
    #[test]
    fn a_contradicting_observation_revokes() {
        let revoked = Evidence {
            from: "2.2.0",
            seen: Seen::Aborts,
            ..QUEUES
        };
        let e = [QUEUES, revoked];
        assert!(resume_in(&e, Tool::Claude, Some("2.1.999")).is_ok());
        assert_eq!(resume_in(&e, Tool::Claude, Some("2.2.0")), Err(ABORTS));
        assert_eq!(resume_in(&e, Tool::Claude, Some("2.9.0")), Err(ABORTS));
        let aborts = Evidence {
            seen: Seen::Aborts,
            ..QUEUES
        };
        for tie in [[QUEUES, aborts], [aborts, QUEUES]] {
            assert_eq!(resume_in(&tie, Tool::Claude, Some("2.1.280")), Err(ABORTS));
        }
    }

    /// An unknown or unreadable version is never resumed.
    #[test]
    fn an_unknown_version_is_not_resumed() {
        assert_eq!(resume_in(&[QUEUES], Tool::Claude, None), Err(NO_VERSION));
        assert_eq!(
            resume_in(&[QUEUES], Tool::Claude, Some("nightly")),
            Err(BAD_VERSION)
        );
        assert!(resume_in(&[QUEUES], Tool::Claude, Some("v2.1.280-beta")).is_ok());
    }

    /// The committed table: both typed harnesses queue at their observed
    /// versions, and an older Codex is not covered.
    #[test]
    fn the_committed_evidence_enables_claude_and_codex() {
        assert!(resume_for(Tool::Claude, Some("2.1.280")).is_ok());
        assert!(resume_for(Tool::Codex, Some("0.161.0")).is_ok());
        assert_eq!(resume_for(Tool::Codex, Some("0.159.3")), Err(NO_EVIDENCE));
        assert_eq!(resume_for(Tool::OpenCode, Some("1.0.0")), Err(NO_EVIDENCE));
    }
}
