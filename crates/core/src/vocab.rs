//! Closed-vocabulary enums shared across Clank.
//!
//! Every enum here is a small fixed set that callers branch on.
//! Wire form is snake_case (via `#[serde(rename_all = "snake_case")]`).
//! `as_str(self) -> &'static str` returns the wire string for log
//! / path / display use without round-tripping through serde.

use serde::{Deserialize, Serialize};

/// Implement `std::fmt::Display` for an enum by delegating to its
/// `as_str(self) -> &'static str` inherent method. The wire string
/// is the right Display form — log lines, format strings, and SPA
/// `{enum}` interpolation all see the same snake_case text serde
/// produces.
macro_rules! impl_display_via_as_str {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl ::core::fmt::Display for $ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    f.write_str(self.as_str())
                }
            }
        )+
    };
}

/// Plan lifecycle: `active` while no `Finalize` event has landed,
/// `finished` once one has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanLifecycle {
    Active,
    Finished,
}

impl PlanLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanLifecycle::Active => "active",
            PlanLifecycle::Finished => "finished",
        }
    }
}

/// Working-tree state of a plan's plan file relative to HEAD's
/// blob. Recomputed at read time — never stored on `Plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWorktreeStatus {
    /// Working-tree body hash matches HEAD's blob hash.
    #[default]
    Clean,
    /// File exists at the active path with a different body.
    BodyDirty,
    /// Plan file is missing from the working tree but still present
    /// in HEAD.
    PlanFileMissing,
}

impl PlanWorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanWorktreeStatus::Clean => "clean",
            PlanWorktreeStatus::BodyDirty => "body_dirty",
            PlanWorktreeStatus::PlanFileMissing => "plan_file_missing",
        }
    }
}

/// Verdict on a single feedback file. Parsed from the file's first
/// non-blank line: `CONTINUE` → `Continue`, `FINISHED` → `Finished`,
/// `REQUEST_CHANGES` → `RequestChanges`, anything else → `Unmarked`.
/// Legacy `APPROVE` is still accepted as `Continue` (the verdict was
/// renamed APPROVE→CONTINUE; on-disk feedback predating the rename must
/// keep gating).
///
/// `Finished` is the "ship it — finalize this plan" signal, distinct
/// from `Continue` which blesses the commit's work but says there is
/// more to do. Reviewers explicitly opt into `Finished`; clank never
/// infers it. The name CONTINUE (vs the old APPROVE) is deliberate:
/// "approve" reads as sign-off, pulling reviewers toward it exactly
/// when the work is done and the verdict should be FINISHED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum Verdict {
    Continue,
    Finished,
    RequestChanges,
    Unmarked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Continue => "continue",
            Verdict::Finished => "finished",
            Verdict::RequestChanges => "request_changes",
            Verdict::Unmarked => "unmarked",
        }
    }
}

/// Per-plan gate state.
///
/// Review-driven precedence (any single review pushes the gate to
/// the highest matching state):
/// - `ChangesRequested`: ≥1 reviewer voted REQUEST_CHANGES.
/// - `Finished`: ≥1 reviewer voted FINISHED, none requested changes.
/// - `Continued`: ≥1 reviewer voted CONTINUE, none FINISHED or
///   request-changes.
/// - `Unreviewed`: no recognized verdict yet on this commit.
///
/// **`Blocked` dominates all review-driven states.** A plan with at
/// least one open plan-scoped block is `Blocked` regardless of
/// review state on the latest reviewable commit (or in fact even
/// without any reviewable commit). The block creator clears the
/// block out-of-band; review-side work is not the path forward.
///
/// **Variant order is wincode-encoded** (this enum derives
/// `wincode::SchemaRead/Write` under `cache-encoding`). New variants
/// MUST be appended at the end so existing cached payloads remain
/// decodable. `Blocked` is at position 4. The cache invalidates via
/// `CACHE_FORMAT_VERSION` bump when needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum CommitGateState {
    Unreviewed,
    Continued,
    Finished,
    ChangesRequested,
    Blocked,
    /// All commit-reviewers voted continue/finished; at least
    /// one gate-reviewer hasn't voted yet (no Request-Changes
    /// from anyone). Master sleeps in this state — it's the
    /// gate-reviewers' window. Plan:
    /// `teams-based-agent-registration`. Appended at the end
    /// per the wincode position-encoding comment above.
    ContinuedPendingGate,
}

impl CommitGateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitGateState::Unreviewed => "unreviewed",
            CommitGateState::Continued => "continued",
            CommitGateState::Finished => "finished",
            CommitGateState::ChangesRequested => "changes_requested",
            CommitGateState::Blocked => "blocked",
            CommitGateState::ContinuedPendingGate => "continued_pending_gate",
        }
    }
}

/// Declares the waiting-reason vocabulary ONCE.
///
/// Enum, `as_str`, `parse` and `ALL` from one list. They were three
/// tables, and omitting a variant from `ALL` compiled while making
/// its emitted reason unparseable — a wake this binary writes and
/// cannot read back, carrying no condition (codex bf22b5b).
macro_rules! waiting_reasons {
    ($($(#[$doc:meta])* $variant:ident => $tag:literal),+ $(,)?) => {
        /// Why the named role is currently the bottleneck.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum WaitingReason {
            $($(#[$doc])* $variant),+
        }

        impl WaitingReason {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(WaitingReason::$variant => $tag),+
                }
            }

            /// Parse a wire reason. `None` ONLY for one this build
            /// has no arm for — the second and last fail-soft at the
            /// hook's parse boundary (the-turn-end-is-one-decision).
            pub fn parse(s: &str) -> Option<Self> {
                Some(match s {
                    $($tag => WaitingReason::$variant,)+
                    _ => return None,
                })
            }

            /// Every reason. Generated, so it cannot omit one.
            pub const ALL: &'static [WaitingReason] = &[$(WaitingReason::$variant),+];
        }
    };
}

waiting_reasons! {
    /// Plan is frozen. No work for either role.
    SessionFinished => "session_finished",
    /// Plan body has uncommitted changes in the worktree. Master
    /// commits to release blocked reviews.
    CommitPlanRevision => "commit_plan_revision",
    /// REQUEST_CHANGES on the latest reviewable commit; master
    /// addresses and commits.
    AddressCommitChanges => "address_commit_changes",
    /// Gate is FINISHED + worktree clean — master should run
    /// `clank finish`.
    ReadyToFinalize => "ready_to_finalize",
    /// Latest reviewable commit is CONTINUE (not FINISHED). Master
    /// keeps working — more impl, more docs, more tests, or
    /// nudge a reviewer to upgrade to FINISHED.
    GateContinue => "gate_continue",
    /// Latest reviewable commit hasn't been reviewed yet.
    CommitNeedsReview => "commit_needs_review",
}

/// Which agent CLI an agent process is running inside. Used by
/// `clank stop-hook` (passed via `--tool`) and `clank as` (auto-
/// detected from which session-id env var is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Claude,
    Codex,
    /// xAI's Grok CLI ("Grok Build") — grok-first-class. Claude-style
    /// background-task wake, passive hooks (no stop-hook adapter), no
    /// session env var in tool subprocesses (identity resolves via the
    /// newest session dir for the cwd).
    Grok,
    /// opencode (anomalyco) — opencode-agent-tool M0. Session identity
    /// is the `OPENCODE_SESSION_ID` opencode 2.x exports into its bash
    /// tool's shells; the clank plugin drives the work loop. Serialized
    /// as `opencode` (one word, like the binary).
    #[serde(rename = "opencode")]
    OpenCode,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Grok => "grok",
            Tool::OpenCode => "opencode",
        }
    }

    /// The inverse of [`Tool::as_str`], for the places that store a
    /// tool as the string it prints as.
    pub fn from_name(s: &str) -> Option<Self> {
        [Tool::Claude, Tool::Codex, Tool::Grok, Tool::OpenCode]
            .into_iter()
            .find(|t| t.as_str() == s)
    }
}

/// Auto-mode setting for an agent's stop-hook behavior.
///
/// - `Off`: hook exits immediately.
/// - `On`: hook long-polls `clank wait` until work arrives or
///   the in-hook poll parks with no work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMode {
    #[default]
    Off,
    #[serde(alias = "hint", alias = "wait")]
    On,
}

impl AutoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoMode::Off => "off",
            AutoMode::On => "on",
        }
    }
}

/// Role an agent plays — their perspective for `wait`, `stop-hook`,
/// etc. — as resolved from the repo roster. `Standby` is on the roster
/// and reviews nothing: it is handed no repo work, and everything else
/// wakes it as it wakes anyone (a-standby-agent-wakes-only-for-messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Master,
    /// Singular per `role-reviewers-to-reviewer-rename`: one
    /// agent declaration entry has ONE role, so the noun should
    /// match. `#[serde(alias = "reviewers")]` preserves
    /// backwards compat for existing on-disk configs.
    #[serde(alias = "reviewers")]
    #[default]
    Reviewer,
    Standby,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Reviewer => "reviewer",
            Role::Standby => "standby",
        }
    }
}

/// Event that can trigger a configured shell hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    Blocked,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::Blocked => "blocked",
        }
    }
}

impl_display_via_as_str! {
    PlanLifecycle,
    PlanWorktreeStatus,
    Verdict,
    CommitGateState,
    WaitingReason,
    Tool,
    AutoMode,
    Role,
    HookEvent,
}

#[cfg(test)]
mod waiting_reason_tests {
    use super::WaitingReason;

    /// `as_str` says exactly what the `rename_all` derive writes.
    ///
    /// Two sources for one string: the macro's literal and serde's
    /// derive. A reason the hook compares against and the wire never
    /// carries classifies nothing, silently — the same drift that hit
    /// `WaitKind`'s ad-hoc variants (the-turn-end-is-one-decision).
    #[test]
    fn a_reasons_name_is_the_one_serde_writes() {
        for r in WaitingReason::ALL {
            let wire = serde_json::to_value(r).unwrap();
            assert_eq!(
                wire.as_str(),
                Some(r.as_str()),
                "{r:?} serializes as {wire} but names itself {}",
                r.as_str(),
            );
            assert_eq!(WaitingReason::parse(r.as_str()), Some(*r), "round trip");
        }
        assert_eq!(WaitingReason::parse("some_new_reason"), None);
    }
}

#[cfg(test)]
mod role_rename_tests {
    use super::Role;

    #[test]
    fn role_reviewers_string_deserializes_as_alias() {
        // Plan acceptance: existing on-disk configs with "reviewers"
        // must continue working via the serde alias.
        let parsed: Role = serde_json::from_str(r#""reviewers""#).unwrap();
        assert_eq!(parsed, Role::Reviewer);
    }

    #[test]
    fn role_reviewer_singular_string_deserializes() {
        let parsed: Role = serde_json::from_str(r#""reviewer""#).unwrap();
        assert_eq!(parsed, Role::Reviewer);
    }

    #[test]
    fn tool_opencode_serializes_one_word() {
        // opencode-agent-tool M0: the variant must serialize as
        // "opencode" (one word, like the binary), not snake_cased.
        assert_eq!(
            serde_json::to_string(&super::Tool::OpenCode).unwrap(),
            "\"opencode\""
        );
        let back: super::Tool = serde_json::from_str("\"opencode\"").unwrap();
        assert_eq!(back, super::Tool::OpenCode);
        assert_eq!(super::Tool::OpenCode.as_str(), "opencode");
    }

    #[test]
    fn role_reviewer_round_trips_as_singular() {
        // Acceptance: serialized output emits "reviewer", not
        // "reviewers". The alias is read-only.
        let out = serde_json::to_string(&Role::Reviewer).unwrap();
        assert_eq!(out, r#""reviewer""#);
    }

    #[test]
    fn role_master_unaffected_by_rename() {
        let out = serde_json::to_string(&Role::Master).unwrap();
        assert_eq!(out, r#""master""#);
        let parsed: Role = serde_json::from_str(r#""master""#).unwrap();
        assert_eq!(parsed, Role::Master);
    }

    #[test]
    fn role_default_is_reviewer() {
        assert_eq!(Role::default(), Role::Reviewer);
    }

    #[test]
    fn role_as_str_returns_singular() {
        assert_eq!(Role::Reviewer.as_str(), "reviewer");
        assert_eq!(Role::Master.as_str(), "master");
    }
}
