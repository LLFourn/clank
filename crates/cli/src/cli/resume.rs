//! Whether clank may resume an agent's harness on its own, and how.
//!
//! Resuming means starting the agent's next turn without interrupting
//! anything: never an interrupt key or RPC, only a submission that the
//! harness itself must queue behind a running turn rather than run over it.
//! Which harnesses do that is a fact about the harness, at its version, so
//! each candidate here is disabled until committed evidence says it holds
//! for the installed version. With no evidence, every harness is
//! status-only.
//!
//! What no candidate protects: a draft a person types after clank's last
//! look at the pane. A typed submission presses Enter, so in that race it
//! sends the draft early with clank's hint appended. Nothing is cancelled.

use clank_core::Tool;

/// How clank may start a harness's next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    /// Type a hint into the agent's pane and submit it.
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

/// One observation that a harness's candidate does not cancel work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub(crate) tool: Tool,
    /// The versions it was observed on, as `(first, last)` inclusive, by
    /// the version the harness prints.
    pub(crate) versions: (&'static str, &'static str),
    pub(crate) resume: Resume,
    /// What was seen, and how.
    pub(crate) observed: &'static str,
}

/// The committed evidence. Empty: no harness has been observed to queue
/// rather than cancel, so none is resumed.
pub(crate) const EVIDENCE: &[Evidence] = &[];

/// Why a harness is not resumed.
pub(crate) const NO_CANDIDATE: &str = "this harness has no way to resume that cancels nothing";
pub(crate) const NO_EVIDENCE: &str =
    "resuming this harness is not enabled: nothing shows it queues rather than cancels";

/// How clank may resume `tool` at `version`, by the evidence in
/// `evidence`; or why not.
pub(crate) fn resume_in(
    evidence: &[Evidence],
    tool: Tool,
    version: Option<&str>,
) -> Result<Resume, &'static str> {
    let candidate = candidate(tool).ok_or(NO_CANDIDATE)?;
    let version = version.ok_or(NO_EVIDENCE)?;
    evidence
        .iter()
        .any(|e| e.tool == tool && e.resume == candidate && within(version, e.versions))
        .then_some(candidate)
        .ok_or(NO_EVIDENCE)
}

/// How clank may resume the installed `tool`, or why not. The installed
/// version is asked for only when there is evidence to match it against.
pub(crate) fn resume_for(tool: Tool) -> Result<Resume, &'static str> {
    candidate(tool).ok_or(NO_CANDIDATE)?;
    if !EVIDENCE.iter().any(|e| e.tool == tool) {
        return Err(NO_EVIDENCE);
    }
    resume_in(EVIDENCE, tool, installed_version(tool).as_deref())
}

/// The version `tool --version` prints, once per process.
fn installed_version(tool: Tool) -> Option<String> {
    static SEEN: std::sync::Mutex<Vec<(Tool, Option<String>)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().ok()?;
    if let Some((_, v)) = seen.iter().find(|(t, _)| *t == tool) {
        return v.clone();
    }
    let version = std::process::Command::new(tool.as_str())
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .map(str::to_string)
        });
    seen.push((tool, version.clone()));
    version
}

/// Whether `version` lies in `range`, comparing dotted numbers.
fn within(version: &str, (first, last): (&str, &str)) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim()
            .trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().ok())
            .collect()
    };
    match (parse(version), parse(first), parse(last)) {
        (Some(v), Some(a), Some(b)) => a <= v && v <= b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: Evidence = Evidence {
        tool: Tool::Claude,
        versions: ("2.1.0", "2.1.300"),
        resume: Resume::TypedSubmit,
        observed: "test",
    };

    /// No evidence, no resume, for every harness; Grok never has one.
    #[test]
    fn nothing_is_resumed_without_evidence() {
        for tool in [Tool::Claude, Tool::Codex, Tool::OpenCode] {
            assert_eq!(resume_in(&[], tool, Some("2.1.280")), Err(NO_EVIDENCE));
        }
        assert_eq!(
            resume_in(&[CLAUDE], Tool::Grok, Some("1.0.46")),
            Err(NO_CANDIDATE)
        );
    }

    /// Evidence applies to its own harness, inside its own versions.
    #[test]
    fn evidence_applies_only_to_its_harness_and_versions() {
        let e = [CLAUDE];
        assert_eq!(
            resume_in(&e, Tool::Claude, Some("2.1.280")),
            Ok(Resume::TypedSubmit)
        );
        assert_eq!(resume_in(&e, Tool::Claude, Some("2.2.0")), Err(NO_EVIDENCE));
        assert_eq!(resume_in(&e, Tool::Claude, Some("2.0.9")), Err(NO_EVIDENCE));
        assert_eq!(resume_in(&e, Tool::Claude, None), Err(NO_EVIDENCE));
        assert_eq!(
            resume_in(&e, Tool::Codex, Some("2.1.280")),
            Err(NO_EVIDENCE)
        );
    }
}
