//! Snapshot-derived domain helpers: pure readings of a
//! [`StatusSnapshot`] into the values the view needs — the coarse
//! attention state, the frame's one hue, the per-agent status glyph,
//! and the per-`WaitingOn` glyph/verb. No rendering, no IO; these are
//! the single source of truth so the bar lamp, the tab indicator, and
//! the pane glyphs can never disagree.

use crate::cli::status::{StatusSnapshot, waiting_actor};
use clank_core::plan_view::WaitingOn;
use clank_core::wait::PlanWorkState;

/// Coarse, human-facing attention state for a worktree — the basis
/// for a zellij tab indicator (spike-zellij-tab-attention):
/// - `Blocked`: a human must act (an unanswered block, or a plan
///   parked on one).
/// - `NeedsCorrection`: HEAD's commit tag doesn't match the plan
///   files it touched (commit-tag-fixup-is-first-class-state) — a
///   self-correctable warning that dominates ordinary work but yields
///   to a human block.
/// - `Idle`: nothing in flight (no active plans, PR reviews, or
///   queued work) — the "asleep" state.
/// - `Active`: anything else (work progressing).
///
/// `state_color` derives ALL its hues from this, so the bar lamp and
/// any tab indicator can never disagree on what "blocked" /
/// "needs-correction" / "idle" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttentionState {
    Active,
    Idle,
    Blocked,
    NeedsCorrection,
}

pub(super) fn attention_state(snap: &StatusSnapshot) -> AttentionState {
    let blocked = snap
        .plans
        .iter()
        .any(|v| matches!(v.waiting_on, WaitingOn::Blocked { .. }))
        || snap.blocks.iter().any(|b| b.answer.is_none());
    if blocked {
        return AttentionState::Blocked;
    }
    // Below Blocked, above Active: a broken HEAD tag is a warning the
    // master must self-correct before work resumes. Read the
    // `head_correction` SOURCE (set on every real violation, including
    // the unknown-tag-only case that marks no plan row — codex 2bf46d9);
    // a per-plan `MasterToFixCommitTag` row is an additional signal.
    if snap.head_correction.is_some()
        || snap
            .plans
            .iter()
            .any(|v| matches!(v.waiting_on, WaitingOn::MasterToFixCommitTag))
    {
        return AttentionState::NeedsCorrection;
    }
    // Ad-hoc counts as in-flight only while its gate still ROUTES
    // work (codex 923a679) — a positively reviewed ad-hoc commit
    // (Continued/Finished) is terminal, not active.
    let nothing_in_flight = snap.plans.is_empty()
        && snap.pr_reviews.is_empty()
        && snap.queue.is_empty()
        && !snap.ad_hoc.iter().any(|a| a.is_open());
    if nothing_in_flight {
        AttentionState::Idle
    } else {
        AttentionState::Active
    }
}

/// Whether master is the agent being woken — the SINGLE predicate
/// behind both the bar's green/cyan hue and the per-pane `🔨`
/// (tui-agent-pane-status-emoji). Spelled with `state_color`'s exact
/// BRANCH PRECEDENCE (codex fbdb27b): plans dominate, so a
/// queue-ready / clean-PR state must NOT count as master-active while
/// a plan still awaits reviewers.
pub(super) fn master_is_active(snap: &StatusSnapshot) -> bool {
    match attention_state(snap) {
        // A broken HEAD tag is master's to fix — master is the active
        // agent (the per-pane 🔨), even though the bar paints orange.
        AttentionState::NeedsCorrection => return true,
        AttentionState::Blocked | AttentionState::Idle => return false,
        AttentionState::Active => {}
    }
    match snap.plans.as_slice() {
        // No plans: PR reviews take precedence over the queue.
        [] if !snap.pr_reviews.is_empty() => {
            // Master's turn iff no PR still owes a reviewer.
            !snap
                .pr_reviews
                .iter()
                .any(|p| !p.missing_reviewers.is_empty())
        }
        // Plan-less precedence mirrors wait's routing exactly (codex
        // 0c31c92): an ad-hoc ChangesRequested is an actionable
        // MASTER item (AdHocRevise) wait returns BEFORE scanning the
        // queue; an Unreviewed ad-hoc gives master nothing, so wait
        // DOES reach queue promotion — promote beats the
        // reviewers'-turn fallthrough.
        [] if snap
            .ad_hoc
            .iter()
            .any(|a| a.gate == clank_core::vocab::CommitGateState::ChangesRequested) =>
        {
            true
        }
        [] if !snap.queue.is_empty() => true, // promote
        [] if snap.ad_hoc.iter().any(|a| a.is_open()) => false, // reviewers' turn
        [] => false,                          // unreachable (Idle covers it)
        // Plans present: queue/PR ignored — master's turn iff some
        // plan is on a master action.
        plans => plans.iter().any(|v| {
            matches!(
                v.waiting_on,
                WaitingOn::MasterToRevise { .. }
                    | WaitingOn::MasterToContinue
                    | WaitingOn::MasterToCommit
                    | WaitingOn::MasterToFinalize
                    | WaitingOn::MasterToFixCommitTag
            )
        }),
    }
}

/// The frame's one hue — a COLOUR, never an attribute.
///
/// `bar` paints it as a reverse-video BACKGROUND. An attribute (dim,
/// bold) sets no colour, so under reverse video it paints nothing and
/// the bar collapses to the terminal's plain reverse — which is
/// exactly the panel's selection band. Idle used to be `"2"` (dim)
/// for that reason, making an idle header indistinguishable from a
/// selected row.
///
/// A string return type made that mistake well-typed. This one cannot
/// represent it: the named variants are a closed set of foreground
/// colours, and `38;5;n` is a colour for every `n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hue {
    Red,
    Green,
    Yellow,
    Cyan,
    /// A 256-palette index, for hues no 16-colour code is close
    /// enough for: orange (208) and the idle blue (63).
    Indexed(u8),
}

impl Hue {
    /// The SGR parameters, for interpolation into an escape.
    pub(super) fn sgr(self) -> String {
        match self {
            Hue::Red => "31".to_string(),
            Hue::Green => "32".to_string(),
            Hue::Yellow => "33".to_string(),
            Hue::Cyan => "36".to_string(),
            Hue::Indexed(i) => format!("38;5;{i}"),
        }
    }
}

/// The frame's one hue: red = a human must act (blocked), orange =
/// HEAD tag needs correction, yellow = reviewers, green = master
/// working, cyan = promote, blue = idle.
///
/// Orange and the idle blue are 256-colour indices: no 16-colour code
/// is close enough for orange, and the idle blue is #5f5fff, chosen
/// because reverse video paints the terminal's default background AS
/// TEXT over the hue — so the bar must read with black text on a dark
/// theme AND white text on a light one. #5f5fff scores 4.57:1 and
/// 4.60:1; plain ANSI blue (`34`, #0000ee) is 2.23:1 on black. A
/// theme-mapped code would leave the real colour to the terminal, and
/// with it those numbers.
pub(super) fn state_color(snap: &StatusSnapshot) -> Hue {
    match attention_state(snap) {
        AttentionState::Blocked => Hue::Red,
        AttentionState::NeedsCorrection => Hue::Indexed(208), // orange
        AttentionState::Idle => Hue::Indexed(63),             // blue
        AttentionState::Active => {
            if master_is_active(snap) {
                // cyan for the queue-promote branch (no plans, no
                // PRs, no ad-hoc revision), green for master working
                // on a plan, PR, or ad-hoc revision (codex 4a4be39).
                if snap.plans.is_empty()
                    && snap.pr_reviews.is_empty()
                    && !snap
                        .ad_hoc
                        .iter()
                        .any(|a| a.gate == clank_core::vocab::CommitGateState::ChangesRequested)
                {
                    Hue::Cyan // promote
                } else {
                    Hue::Green // master
                }
            } else {
                Hue::Yellow // reviewers
            }
        }
    }
}

/// The `WorkStatus` projection the panel questions reduce — the SAME
/// shape `clank wait` reduces, with the fold's REAL ad-hoc state
/// (tui-adhoc-review-activity).
pub(super) fn work_projection(snap: &StatusSnapshot) -> clank_core::wait::WorkStatus {
    clank_core::wait::WorkStatus {
        plans: snap.plans.clone(),
        ad_hoc: snap.ad_hoc.clone(),
        pr_reviews: snap.pr_reviews.clone(),
        head_correction: snap.head_correction.clone(),
        // Irrelevant to the reviewer question: the multi-plan warning
        // preempts MASTER items only — reviewer routing is untouched
        // by design (soft-disallow-multiple-plans).
        multi_plan_open: None,
    }
}

/// ROSTER-driven (tui-adhoc-review-activity, intro 956bed7):
/// candidates are the roster's non-master rows and the shared
/// `is_actionable` alone decides who is awaited — one routing path
/// covers plan, PR, ad-hoc, their unions, and global preemption.
/// Work-kind-specific candidate discovery is gone: an ad-hoc review
/// carries no labels, so no missing-set enumeration could ever
/// surface its reviewers.
pub(super) fn awaited_reviewers(snap: &StatusSnapshot) -> Vec<String> {
    use clank_core::vocab::Role;
    let work = work_projection(snap);
    snap.agents
        .iter()
        .filter(|a| a.role != crate::cli::teams_config::RosterRole::Master)
        .filter(|a| {
            clank_core::ids::AgentLabel::parse(&a.label)
                .is_ok_and(|l| work.is_actionable(&l, Role::Reviewer))
        })
        .map(|a| a.label.clone())
        .collect()
}

/// The status glyph for ONE agent's pane: `🔨` master working / `👀`
/// awaited reviewer / `💤` idle (tui-agent-pane-status-emoji). Coarse
/// by design — every master-work state shows `🔨`; the bar keeps the
/// fine-grained glyph.
pub(super) fn agent_status_emoji(
    snap: &StatusSnapshot,
    label: &str,
    role: clank_core::vocab::Role,
) -> &'static str {
    match role {
        clank_core::vocab::Role::Master => {
            if master_is_active(snap) {
                "🔨"
            } else {
                "💤"
            }
        }
        clank_core::vocab::Role::Reviewer => {
            if awaited_reviewers(snap).iter().any(|l| l.as_str() == label) {
                "👀"
            } else {
                "💤"
            }
        }
    }
}

pub(super) fn actor_of(v: &PlanWorkState) -> String {
    match &v.waiting_on {
        // Blocked = awaiting the human, not the block's creator.
        WaitingOn::Blocked { .. } => "human".to_string(),
        w => waiting_actor(w),
    }
}

pub(super) fn emoji_of(w: &WaitingOn) -> &'static str {
    match w {
        WaitingOn::ReviewerApprovalsMissing { .. } => "👀",
        WaitingOn::GateReviewersMissing { .. } => "🔍",
        WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToContinue
        | WaitingOn::MasterToCommit => "🔨",
        WaitingOn::MasterToFinalize => "🏁",
        WaitingOn::MasterToFixCommitTag => "⚠️",
        WaitingOn::Blocked { .. } => "🙋",
    }
}

pub(super) fn verb_of(w: &WaitingOn) -> &'static str {
    match w {
        WaitingOn::ReviewerApprovalsMissing { .. } => "reviewing",
        WaitingOn::GateReviewersMissing { .. } => "gate-reviewing",
        WaitingOn::MasterToRevise { .. } => "revising",
        WaitingOn::MasterToContinue => "working",
        WaitingOn::MasterToCommit => "committing",
        WaitingOn::MasterToFinalize => "finalizing",
        WaitingOn::MasterToFixCommitTag => "fixing tag",
        WaitingOn::Blocked { .. } => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{
        plan_state, pr_awaiting, reviewer_missing, snap, with_agents,
    };
    use crate::lifecycle::AgentLabel;

    #[test]
    fn master_is_active_follows_state_color_precedence() {
        // Idle → false.
        assert!(!master_is_active(&snap(vec![], vec![])));
        // Plan on a master action → true.
        assert!(master_is_active(&snap(
            vec![plan_state("p", WaitingOn::MasterToContinue)],
            vec![]
        )));
        // THE codex fbdb27b case: a plan awaiting reviewers dominates a
        // pending queue → master is NOT active (bar stays yellow).
        assert!(
            !master_is_active(&snap(
                vec![plan_state("p", reviewer_missing("codex"))],
                vec!["queued"]
            )),
            "plans take precedence over the queue"
        );
        // No plans + queue only → master promotes.
        assert!(master_is_active(&snap(vec![], vec!["queued"])));
        // No plans + PR: master's turn iff no reviewer is owed.
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(pr_awaiting(&["codex"]));
        assert!(!master_is_active(&s), "PR still owes a reviewer");
        s.pr_reviews[0].missing_reviewers.clear();
        assert!(master_is_active(&s), "PR fully reviewed → master's turn");
    }

    #[test]
    fn awaited_reviewers_idle_under_head_correction() {
        // unify-repo-state-watcher: a broken HEAD tag is a GLOBAL preempt
        // — `clank wait` wakes no reviewer while it stands. The panel
        // must agree (via the shared `is_actionable`), even for a reviewer
        // missing on a plan the tag never implicated. Without routing
        // through the single source, this reviewer showed 👀 while their
        // stop-hook returned nothing.
        let mut s = snap(vec![plan_state("other", reviewer_missing("codex"))], vec![]);
        s.head_correction = Some(clank_core::wait::HeadCorrection {
            sha: crate::lifecycle::CommitSha::parse(&"a".repeat(40)).unwrap(),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["ghost".to_string()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        });
        assert!(
            awaited_reviewers(&s).is_empty(),
            "a broken HEAD idles every reviewer, even on a non-implicated plan"
        );
        assert_eq!(
            agent_status_emoji(&s, "codex", clank_core::vocab::Role::Reviewer),
            "💤",
            "the panel must show the reviewer idle, matching the stop-hook"
        );
    }

    #[test]
    fn adhoc_review_activates_roster_reviewers() {
        // THE report (tui-adhoc-review-activity): a pending AD-HOC
        // commit review — no plans, no PRs — must show its reviewers
        // active. Candidates come from the ROSTER (AdHocWorkState
        // carries no labels); the shared is_actionable decides.
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::Role;
        let roster: &[(&str, RosterRole)] = &[
            ("claude", RosterRole::Master),
            ("codex", RosterRole::Commit),
            ("ruthless", RosterRole::Gate),
        ];
        let mut s = with_agents(snap(vec![], vec![]), roster);
        s.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: crate::lifecycle::CommitSha::parse(&"b".repeat(40)).unwrap(),
            gate: clank_core::vocab::CommitGateState::Unreviewed,
        }];
        let awaited = awaited_reviewers(&s);
        assert!(
            awaited.iter().any(|l| l == "codex"),
            "ad-hoc-only snapshot activates roster reviewers: {awaited:?}"
        );
        assert_eq!(agent_status_emoji(&s, "codex", Role::Reviewer), "👀");
        assert_eq!(
            agent_status_emoji(&s, "claude", Role::Master),
            "💤",
            "unreviewed ad-hoc work is the reviewers' turn"
        );

        // Global preemption still routes through is_actionable: a
        // broken HEAD idles everyone, ad-hoc work included.
        let mut broken = s;
        broken.head_correction = Some(clank_core::wait::HeadCorrection {
            sha: crate::lifecycle::CommitSha::parse(&"a".repeat(40)).unwrap(),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["ghost".to_string()],
                untagged_touched: vec![],
                extra_named: vec![],
            },
        });
        assert!(
            awaited_reviewers(&broken).is_empty(),
            "a broken HEAD idles ad-hoc reviewers too"
        );

        // ChangesRequested: the reviewers' part is done — they idle
        // (master's ad-hoc revise is out of this plan's scope).
        let mut done = with_agents(snap(vec![], vec![]), roster);
        done.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: crate::lifecycle::CommitSha::parse(&"b".repeat(40)).unwrap(),
            gate: clank_core::vocab::CommitGateState::ChangesRequested,
        }];
        assert!(awaited_reviewers(&done).is_empty());
    }

    #[test]
    fn awaited_reviewers_unions_plans_and_prs() {
        use crate::cli::teams_config::RosterRole;
        let mut s = with_agents(
            snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]),
            &[
                ("claude", RosterRole::Master),
                ("codex", RosterRole::Commit),
                ("ruthless", RosterRole::Gate),
            ],
        );
        s.pr_reviews.push(pr_awaiting(&["ruthless"]));
        let got = awaited_reviewers(&s);
        assert!(
            got.iter().any(|l| l == "codex") && got.iter().any(|l| l == "ruthless"),
            "got: {got:?}"
        );
        // Master's turn → nobody awaited.
        assert!(
            awaited_reviewers(&snap(
                vec![plan_state("p", WaitingOn::MasterToContinue)],
                vec![]
            ))
            .is_empty()
        );
    }

    #[test]
    fn agent_status_emoji_per_role() {
        use clank_core::vocab::Role;
        let working = snap(vec![plan_state("p", WaitingOn::MasterToContinue)], vec![]);
        assert_eq!(agent_status_emoji(&working, "claude", Role::Master), "🔨");
        assert_eq!(agent_status_emoji(&working, "codex", Role::Reviewer), "💤");

        // Roster-driven candidates (tui-adhoc-review-activity): the
        // fixture must declare who is on the team.
        let reviewing = with_agents(
            snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]),
            &[
                ("claude", crate::cli::teams_config::RosterRole::Master),
                ("codex", crate::cli::teams_config::RosterRole::Commit),
                ("ruthless", crate::cli::teams_config::RosterRole::Gate),
            ],
        );
        assert_eq!(
            agent_status_emoji(&reviewing, "codex", Role::Reviewer),
            "👀"
        );
        assert_eq!(
            agent_status_emoji(&reviewing, "ruthless", Role::Reviewer),
            "💤"
        );
        assert_eq!(agent_status_emoji(&reviewing, "claude", Role::Master), "💤");

        assert_eq!(
            agent_status_emoji(&snap(vec![], vec![]), "claude", Role::Master),
            "💤"
        );
    }

    #[test]
    fn attention_state_classifies_blocked_idle_active() {
        // Idle: nothing in flight.
        assert_eq!(attention_state(&snap(vec![], vec![])), AttentionState::Idle);

        // Active: an in-flight plan, OR queued work, OR a PR review.
        assert_eq!(
            attention_state(&snap(
                vec![plan_state("foo", reviewer_missing("codex"))],
                vec![]
            )),
            AttentionState::Active
        );
        assert_eq!(
            attention_state(&snap(vec![], vec!["queued"])),
            AttentionState::Active,
            "queued work is not idle — master owes a promote"
        );
        let mut s = snap(vec![], vec![]);
        s.pr_reviews.push(clank_core::wait::PrReviewWorkState {
            pr: 1,
            repo: "o/r".into(),
            round: 1,
            gate: clank_core::vocab::CommitGateState::Unreviewed,
            missing_reviewers: vec![AgentLabel::parse("codex").unwrap()],
        });
        assert_eq!(attention_state(&s), AttentionState::Active);

        // Blocked: an unanswered block dominates even with no plans.
        let mut s = snap(vec![], vec!["queued"]);
        s.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            question: "halt?".into(),
            answer: None,
        }];
        assert_eq!(
            attention_state(&s),
            AttentionState::Blocked,
            "an unanswered block outranks active work"
        );
    }

    /// Every branch `state_color` can return, built from a REAL
    /// snapshot rather than by naming `AttentionState` directly — the
    /// snapshot→hue mapping is what ships, and a branch reachable only
    /// in theory would not be worth pinning.
    fn rendered_branches() -> Vec<(&'static str, StatusSnapshot)> {
        let mut blocked = snap(vec![], vec!["queued"]);
        blocked.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            question: "halt?".into(),
            answer: None,
        }];
        vec![
            ("blocked", blocked),
            (
                "correction",
                snap(
                    vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
                    vec![],
                ),
            ),
            ("idle", snap(vec![], vec![])),
            (
                "master",
                snap(vec![plan_state("foo", WaitingOn::MasterToContinue)], vec![]),
            ),
            ("promote", snap(vec![], vec!["queued"])),
            (
                "reviewer",
                snap(vec![plan_state("foo", reviewer_missing("codex"))], vec![]),
            ),
        ]
    }

    /// A foreground COLOUR SGR: `30`-`37`, `90`-`97`, or a well-formed
    /// `38;5;n`. Never an attribute — `1` bold, `2` dim, `3` italic,
    /// `4` underline, `7` reverse — which sets no colour at all.
    fn is_foreground_colour(sgr: &str) -> bool {
        match sgr.strip_prefix("38;5;") {
            Some(n) => n.parse::<u8>().is_ok(),
            None => matches!(sgr.parse::<u16>(), Ok(30..=37 | 90..=97)),
        }
    }

    /// `bar` paints the hue as a reverse-video BACKGROUND, so an
    /// attribute paints nothing and the bar collapses to the
    /// terminal's plain reverse — the panel's selection band. `Hue`
    /// makes that unrepresentable; this pins the table it feeds, and
    /// that the six branches stay tellable apart.
    #[test]
    fn every_branch_is_a_distinct_colour_never_an_attribute() {
        let branches = rendered_branches();
        for (name, s) in &branches {
            let sgr = state_color(s).sgr();
            assert!(
                is_foreground_colour(&sgr),
                "{name}: `{sgr}` paints no colour"
            );
        }
        for (i, (a_name, a)) in branches.iter().enumerate() {
            for (b_name, b) in branches.iter().skip(i + 1) {
                assert_ne!(
                    state_color(a),
                    state_color(b),
                    "{a_name} and {b_name} share a hue — the bar cannot tell them apart"
                );
            }
        }
    }

    /// Comparing the bar's escape to the selection band's cannot catch
    /// the defect this replaced: `\x1b[1;7;2m` is textually distinct
    /// from `\x1b[7m` while rendering identically, so that assertion
    /// passes on the exact collision (codex on dbaa3e0). Assert the
    /// property that actually differs — a colour is present.
    #[test]
    fn every_bar_carries_a_colour_not_just_reverse_video() {
        for (name, s) in rendered_branches() {
            let line = crate::cli::status_tui::render::bar(
                &s,
                &state_color(&s).sgr(),
                20,
                crate::cli::status_tui::zellij::ZellijReach::NotInSession,
            );
            let sgr = line
                .strip_prefix("\x1b[")
                .and_then(|rest| rest.split_once('m'))
                .expect("the bar opens with an SGR")
                .0;
            let hue = sgr
                .strip_prefix("1;7;")
                .unwrap_or_else(|| panic!("{name}: expected bold+reverse then a hue, got `{sgr}`"));
            assert!(
                is_foreground_colour(hue),
                "{name}: bar paints no colour, so it renders as the selection band: {line:?}"
            );
        }
    }

    /// Pinned to the exact value, not merely "a colour that isn't the
    /// selection band". Reverse video paints the terminal's default
    /// background AS TEXT over the hue, so the bar must read with
    /// black text on a dark theme and white text on a light one.
    /// #5f5fff clears 4.5:1 both ways (4.57 / 4.60); the window where
    /// any single colour can is only L ∈ [0.175, 0.183]. A looser
    /// assertion would stay green while an edit swapped in a colour
    /// that fails on half the terminals in use.
    #[test]
    fn idle_is_the_blue_the_contrast_derivation_chose() {
        assert_eq!(state_color(&snap(vec![], vec![])), Hue::Indexed(63));
    }

    #[test]
    fn needs_correction_is_orange_above_active_below_blocked() {
        // commit-tag-fixup-is-first-class-state: a MasterToFixCommitTag
        // plan row → NeedsCorrection (orange), above ordinary Active.
        let s = snap(
            vec![plan_state("foo", WaitingOn::MasterToFixCommitTag)],
            vec![],
        );
        assert_eq!(attention_state(&s), AttentionState::NeedsCorrection);
        assert_eq!(state_color(&s), Hue::Indexed(208), "orange");
        // Master is the actor (per-pane 🔨), and the bar emoji is ⚠️ —
        // bar lamp + tab indicator both derived from attention_state,
        // so they can't disagree.
        assert!(master_is_active(&s));
        assert_eq!(emoji_of(&WaitingOn::MasterToFixCommitTag), "⚠️");

        // ...but a human block still outranks the correction.
        let mut blocked = s;
        blocked.blocks = vec![crate::cli::block::BlockEntry {
            agent: "claude".into(),
            name: "q".into(),
            question: "halt?".into(),
            answer: None,
        }];
        assert_eq!(
            attention_state(&blocked),
            AttentionState::Blocked,
            "a human block outranks the tag correction"
        );
    }
}
