//! The input state machine: raw stdin → [`Key`], the [`Mode`] that owns
//! the keyboard (and its sub-states), and the PURE routing/decision
//! functions that map a key in a mode to an action the loop executes.
//! No IO — the effectful appliers (roster mutations) live in the loop
//! (the IO shell), so this whole module is unit-tested headless.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Quit,
    /// Space — page-down while scrolling the log; toggle auto on the
    /// selected agent while the agent panel is focused. Context-free
    /// parse; the loop resolves the meaning from focus.
    Space,
    /// Tab — move keyboard focus between the log and the agent panel.
    ///
    /// `a` is an ALIAS for this at the sites that want it, but it is
    /// no longer folded in here: the parser used to map both bytes to
    /// this one variant, which left every page unable to tell a
    /// navigation keypress from a letter. Pages that bind `a` to an
    /// action (the event page's ack, the purge chooser's
    /// artifacts-only) could not see it at all, and routing this
    /// variant to such an action would have made TAB perform it
    /// (tui-event-page-hotkeys).
    Focus,
    /// Esc — back out one level (leave the panel / cancel a picker or
    /// confirm).
    Escape,
    /// Enter — activate the row under the cursor (open the picker on
    /// "+ add", choose a candidate) or, in a confirm, follow the
    /// default.
    Enter,
    /// ← (left arrow) — back out of the commit-detail overlay; cycle the
    /// selected toggle on the agent-detail page; a no-op elsewhere.
    Left,
    /// → (right arrow) — cycle the selected toggle on the agent-detail
    /// page; a no-op elsewhere.
    Right,
    /// Backspace / DEL — remove the selected reviewer.
    Delete,
    /// `y` — confirm.
    Yes,
    /// `n` — decline.
    No,
    /// `o` — open the current plan/commit detail overlay as its rendered
    /// HTML page in the browser (or a queue row's page from the panel).
    Html,
    /// `+` — nudge the selected queue row's priority number up (later).
    Plus,
    /// `-` — nudge the selected queue row's priority number down (sooner).
    Minus,
    /// Any other printable ASCII byte — mode-scoped hotkeys (the plan
    /// page's `s`/`f`/`c`/`p`/`b`) and, later, text input. Bound keys
    /// above parse FIRST; `Char` is strictly the fallback, so adding a
    /// binding never changes meaning under a mode that reads `Char`.
    Char(u8),
}

/// Which region (and sub-state) owns the keyboard. The backbone of key
/// routing: each key is interpreted in exactly ONE place per mode, so
/// an unbound key does nothing and a key can't mean two things at once.
/// `Confirm` as its own mode is what makes "Enter silently confirms a
/// destructive default" unwritable — Enter is resolved in one place.
///
/// `Copy` is preserved by storing INDICES (into `snapshot.agents` /
/// the freshly-read picker list), never owned labels; the label is
/// resolved at action time. A data-changing Refresh resets the picker/
/// confirm modes (see the Refresh arm) so an index can't act on a
/// reordered target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Default: keys scroll the log.
    LogScroll,
    /// The agent panel owns the keys; `sel` is the cursor row. Rows are
    /// `agents` followed by the "+ add" row at index `agents.len()`.
    AgentPanel { sel: usize },
    /// Choosing a library agent to add; `sel` indexes the freshly-read
    /// candidate list the loop holds.
    AddPicker {
        sel: usize,
        /// The tier the candidate will be added AT, cycled with
        /// ←/→ before Enter. Held in the mode rather than beside it so
        /// every construction site — the refresh rebind included —
        /// has to decide, and a redraw cannot silently reset a choice
        /// the user already made.
        tier: crate::cli::teams_config::ReviewKind,
    },
    /// Choosing the agent to swap IN for `out`. Same candidate list as
    /// `AddPicker` — library minus roster is exactly who may replace a
    /// member — with `out` indexing `snapshot.agents`.
    SwapPicker { out: usize, sel: usize },
    /// The per-agent detail/config page; `idx` is the agent in
    /// `snapshot.agents`, `sel` the cursor over its action menu.
    AgentDetail { idx: usize, sel: usize },
    /// A mutating decision is pending; the action carries the target by
    /// index.
    Confirm { action: ConfirmAction },
    /// The per-plan actions page (tui-plan-actions-page); `sel` is the
    /// cursor over its action menu. The plan's IDENTITY (stem) lives in
    /// the loop's `plan_page` — `Mode` stays `Copy`; the two are set and
    /// cleared together (invariant: `PlanDetail`/`PurgeChoice` ⟺
    /// `plan_page.is_some()`).
    PlanDetail { sel: usize },
    /// The purge second screen: artifacts-only vs drop-everything.
    PurgeChoice { sel: usize },
    /// Typing the repo pause question. Deliberately NOT a
    /// `PlanInput`: a pause is repo state, so this mode must work
    /// with no plan page open (and with no plans at all).
    PauseInput,
    /// Typing the answer to ONE agent-authored block, indexed into
    /// `snapshot.blocks`. Per-block by construction: a shared answer
    /// box would put one string under several distinct questions.
    BlockAnswer { block: usize },
    /// The github event page (tui-github-event-page); `sel` is the
    /// cursor over its action menu. The event's IDENTITY (retained
    /// member keys) lives in the loop's `event_page` — same
    /// set/clear-together invariant as `plan_page`.
    EventDetail { sel: usize },
    /// The open WAIT page: which agent's wait, and the cursor row on it.
    WaitDetail { agent: usize, sel: usize },
    /// A one-line text input on the plan page (force-finish subject,
    /// squash message, block reason, drop type-to-confirm). The BUFFER
    /// lives in the loop's `plan_input` (Mode stays `Copy`); same
    /// set/clear-together invariant as `plan_page`.
    PlanInput { kind: PlanInputKind },
}

/// What the open plan-page input is FOR — picks the screen chrome, the
/// submit action, and the validation (drop requires the exact stem).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanInputKind {
    /// The WHAT subject for `finish --force`; the WHY paragraph is
    /// auto-provenance (gate state at bypass).
    ForceFinishSubject,
    /// The squash message for a finished plan (prefilled with the
    /// finalize commit's subject).
    SquashMessage,
    /// Type-the-stem arming for `purge --drop` — the scariest screen;
    /// submit is a no-op until the buffer equals the stem exactly.
    DropStem,
}

/// Whether an agent has a pane with a running process, as the zellij
/// worker last reported it. `Unknown` is not `Missing`: outside zellij
/// or before a listing has answered there is nothing to say, and
/// nothing to reopen into (the-tui-knows-whether-a-pane-is-open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    Live,
    Missing,
    Unknown,
}

/// One row of an agent's detail-page action menu (the actions are data
/// the cursor moves over, not a keymap). Availability depends on role
/// and on whether the agent has a live pane — see [`detail_actions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailAction {
    /// Arm/disarm the agent's auto-mode.
    ToggleAuto,
    /// The `commit` review checkbox — EXCLUSIVE: a commit-tier reviewer
    /// reviews every commit, subsuming the gate points, so ticking it
    /// grays out `plan`/`final`.
    TierCommit,
    /// The `plan` review checkbox (independent of `final`).
    TierPlan,
    /// The `final` review checkbox. `plan`+`final` together is what the
    /// roster calls `Gate` internally.
    TierFinal,
    /// Promote a reviewer to master (the core also demotes the old one).
    PromoteToMaster,
    /// Replace this reviewer with another, keeping its role. Opens a
    /// picker: unlike every sibling action it needs a SECOND operand,
    /// since it acts on this agent AND an incoming one.
    Swap,
    /// Bring back this agent's pane — offered only when the worker's
    /// last listing found no live one.
    Reopen,
    /// Remove the agent from the team (behind the confirm).
    Remove,
    /// Leave the detail page.
    Back,
}

/// The detail-page actions for `role`, in display order. Master gets a
/// reduced set (no review checkboxes / promote / remove): the UI hide is
/// primary, and the cores refuse anyway (defense in depth). Reopen is
/// there only for an agent with no live pane.
pub(super) fn detail_actions(
    role: crate::cli::teams_config::RosterRole,
    presence: Presence,
) -> Vec<DetailAction> {
    use crate::cli::teams_config::RosterRole;
    use DetailAction::*;
    let mut actions = match role {
        RosterRole::Master => vec![ToggleAuto, Swap, Back],
        RosterRole::Commit | RosterRole::Plan | RosterRole::Final | RosterRole::Gate => {
            vec![
                ToggleAuto,
                TierCommit,
                TierPlan,
                TierFinal,
                PromoteToMaster,
                Swap,
                Remove,
                Back,
            ]
        }
    };
    if presence == Presence::Missing {
        let back = actions.len() - 1;
        let at = match role {
            RosterRole::Master => back,
            _ => back - 1,
        };
        actions.insert(at, Reopen);
    }
    actions
}

/// One row of the plan-actions page (tui-plan-actions-page). Rows are
/// data the cursor moves over; availability depends on the plan's
/// state — see [`plan_actions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanAction {
    /// Open the plan's rendered HTML page in the browser.
    OpenHtml,
    /// `stash push` the plan (active only) — behind a confirm.
    Stash,
    /// `finish --force` (active only): finalize past the review gate.
    ForceFinish,
    /// `purge --squash` a finished plan's range into one commit. Only
    /// offered when the range still has >1 commit.
    Squash,
    /// The danger door: opens the purge chooser (artifacts vs drop).
    Purge,
    /// Leave the page.
    Back,
}

/// Facts of the plan the page needs, derived by the loop from the
/// snapshot + log sequence (pure inputs, so row derivation is
/// unit-testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlanPageState {
    pub finished: bool,
    /// The plan's range still spans >1 commit (squash is pointless on
    /// an already-collapsed plan).
    pub multi_commit: bool,
    /// The repo is paused by an unanswered block. Repo state, not the
    /// plan's — shown on this page only to explain why an active plan
    /// is not progressing. Pausing is driven by the global `b` key.
    pub repo_paused: bool,
}

/// The page's rows for a plan state, in display order. Inapplicable
/// actions are ABSENT, not grayed: the states are different pages, not
/// one form (finished plans can't stash/force-finish; active plans
/// can't squash).
pub(super) fn plan_actions(st: PlanPageState) -> Vec<PlanAction> {
    use PlanAction::*;
    let mut v = vec![OpenHtml];
    if st.finished {
        if st.multi_commit {
            v.push(Squash);
        }
    } else {
        v.push(Stash);
        v.push(ForceFinish);
    }
    v.push(Purge);
    v.push(Back);
    v
}

/// The plan page's direct hotkey for a key, if that action is present
/// on this page. `s` stash · `f` force-finish · `c` squash (collapse) ·
/// `b` block/unblock · `p` purge · `o` html — `q` stays global quit.
pub(super) fn plan_hotkey(key: Key, actions: &[PlanAction]) -> Option<PlanAction> {
    use PlanAction::*;
    let want = match key {
        Key::Html => OpenHtml,
        Key::Char(b's') => Stash,
        Key::Char(b'f') => ForceFinish,
        Key::Char(b'c') => Squash,
        Key::Char(b'p') => Purge,
        _ => return None,
    };
    actions.contains(&want).then_some(want)
}

/// One keypress on the plan page → what the loop should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanNav {
    Sel(usize),
    Act(PlanAction),
    /// Scroll the plan-document body below the buttons (↑↓ stay on
    /// button selection; paging keys own the document).
    Scroll(i32),
    Back,
    None,
}

pub(super) fn plan_detail_nav(
    sel: usize,
    actions: &[PlanAction],
    key: Key,
    page: usize,
    scroll: usize,
) -> PlanNav {
    if let Some(a) = plan_hotkey(key, actions) {
        return PlanNav::Act(a);
    }
    let page = page as i32;
    let doc = document_focus(actions);
    match key {
        // The options→document crossing mirrors the panel↔log model:
        // ↓ walks the buttons, then moves FOCUS into the document —
        // where no button is highlighted and Enter has no target —
        // and only then scrolls it; ↑ climbs back out through the
        // document's top. Keeping the cursor on the last button while
        // the document scrolled left `esc back` lit for the whole
        // read, with Enter primed to fire it
        // (the-plan-page-cursor-enters-the-document).
        Key::Up if sel == doc && scroll > 0 => PlanNav::Scroll(-1),
        Key::Up => PlanNav::Sel(sel.saturating_sub(1)),
        Key::Down if sel < doc => PlanNav::Sel(sel + 1),
        Key::Down => PlanNav::Scroll(1),
        Key::PageUp => PlanNav::Scroll(-page),
        Key::Space | Key::PageDown => PlanNav::Scroll(page),
        Key::Enter => actions.get(sel).map_or(PlanNav::None, |a| PlanNav::Act(*a)),
        Key::Escape => PlanNav::Back,
        _ => PlanNav::None,
    }
}

/// The plan page's cursor position for the DOCUMENT: one past the
/// last button. A real focus position, like the log is for the panel,
/// not a button that happens to scroll.
pub(super) fn document_focus(actions: &[PlanAction]) -> usize {
    actions.len()
}

/// Carry the plan-page cursor across a refresh by the ACTION under
/// it, never its index: the list changes shape when a plan finishes
/// (`ForceFinish` leaves, `Squash` may arrive), and a clamped index
/// that was on `ForceFinish` lands on `Purge` — the next Enter would
/// run a destructive action the operator never saw selected (codex on
/// 40e5d34). Document focus stays document focus at the new length; a
/// vanished action falls back to it, the one position with no Enter
/// target.
pub(super) fn rebind_plan_sel(before: &[PlanAction], sel: usize, after: &[PlanAction]) -> usize {
    match before.get(sel) {
        Some(want) => after
            .iter()
            .position(|a| a == want)
            .unwrap_or_else(|| document_focus(after)),
        None => document_focus(after),
    }
}

/// The purge chooser's rows: artifacts-only, drop-everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PurgeChoice {
    Artifacts,
    Drop,
    Back,
}

pub(super) fn purge_choice_nav(sel: usize, key: Key) -> PlanNavPurge {
    match key {
        Key::Char(b'a') => PlanNavPurge::Choose(PurgeChoice::Artifacts),
        Key::Char(b'd') => PlanNavPurge::Choose(PurgeChoice::Drop),
        Key::Up => PlanNavPurge::Sel(sel.saturating_sub(1)),
        Key::Down => PlanNavPurge::Sel((sel + 1).min(1)),
        Key::Enter => PlanNavPurge::Choose(if sel == 0 {
            PurgeChoice::Artifacts
        } else {
            PurgeChoice::Drop
        }),
        Key::Escape => PlanNavPurge::Choose(PurgeChoice::Back),
        _ => PlanNavPurge::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanNavPurge {
    Sel(usize),
    Choose(PurgeChoice),
    None,
}

/// A one-line ASCII text input: buffer + byte cursor. ASCII-only by
/// construction (the parser only emits printable ASCII `Char`s; the
/// 16-byte stdin reads can split multibyte sequences, so non-ASCII is
/// deliberately out of scope for now) — every byte index is a char
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TextInput {
    pub(super) buf: String,
    pub(super) cursor: usize,
}

impl TextInput {
    pub(super) fn prefilled(text: &str) -> Self {
        Self {
            buf: text.to_string(),
            cursor: text.len(),
        }
    }
}

/// What one text keystroke did to the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputNav {
    /// Enter — the caller validates + acts on `input.buf`.
    Submit,
    /// Esc — abandon the input.
    Cancel,
    None,
}

/// Compose the TUI squash message: typed subject + the plan's OWN WHY
/// (the finalize commit's body). finish's validator demands a
/// subject AND a `\n\n`-separated body (tui-squash-message-body: the
/// raw one-line input was rejected unconditionally — the flow was
/// dead on arrival). Legacy plans finished before mandatory messages
/// have no body — a provenance line passes validation and is honest
/// about why no richer WHY exists.
pub(crate) fn compose_squash_message(
    subject: &str,
    finalize_body: &str,
    provenance: &str,
) -> String {
    let body = finalize_body.trim();
    let why = if body.is_empty() { provenance } else { body };
    format!("{subject}\n\n{why}")
}

/// The TUI's provenance line for a squash whose finalize predates
/// mandatory finish messages (no WHY body to carry).
pub(super) const TUI_SQUASH_PROVENANCE: &str = "collapsed to one commit from clank status --tui; \
     the original finish predates mandatory finish messages.";

/// THE drop-arming predicate: the RAW buffer must equal the stem
/// exactly — no trim, no case-fold. One definition shared by the
/// armed/not-armed indicator AND the submit gate, so they cannot
/// disagree (codex 0daf087: submit trimmed while the indicator
/// didn't, so ` <stem> ` dropped a plan the screen called not armed).
pub(super) fn drop_armed(buf: &str, stem: &str) -> bool {
    buf == stem
}

/// Apply one [`TextKey`] to the input. Pure; the caller owns what
/// Submit/Cancel mean for its screen.
pub(super) fn text_input_nav(input: &mut TextInput, k: TextKey) -> InputNav {
    match k {
        TextKey::Enter => InputNav::Submit,
        TextKey::Esc => InputNav::Cancel,
        TextKey::Char(c) if (0x20..0x7f).contains(&c) => {
            input.buf.insert(input.cursor, c as char);
            input.cursor += 1;
            InputNav::None
        }
        TextKey::Backspace => {
            if input.cursor > 0 {
                input.cursor -= 1;
                input.buf.remove(input.cursor);
            }
            InputNav::None
        }
        TextKey::Left => {
            input.cursor = input.cursor.saturating_sub(1);
            InputNav::None
        }
        TextKey::Right => {
            input.cursor = (input.cursor + 1).min(input.buf.len());
            InputNav::None
        }
        TextKey::Char(_) | TextKey::Ignore => InputNav::None,
    }
}

/// A reviewer tier's checkbox state, `(commit, plan, final)`. The four
/// `ReviewKind`s map 1:1 onto the reachable states — `Gate` IS
/// `plan`+`final`; the checkboxes expose the model the roster already has.
pub(super) fn tier_boxes(kind: crate::cli::teams_config::ReviewKind) -> (bool, bool, bool) {
    use crate::cli::teams_config::ReviewKind::*;
    match kind {
        Commit => (true, false, false),
        Plan => (false, true, false),
        Final => (false, false, true),
        Gate => (false, true, true),
    }
}

/// Step the add-picker's tier. ←/→ already mean "cycle the selected
/// toggle" on the detail page, so they carry the same meaning here.
///
/// The ORDER is the review pipeline — commit, plan, final, then gate
/// (which is both plan and final) — so stepping reads as widening
/// scope rather than as an arbitrary rotation.
pub(super) fn tier_cycle(
    tier: crate::cli::teams_config::ReviewKind,
    forward: bool,
) -> crate::cli::teams_config::ReviewKind {
    use crate::cli::teams_config::ReviewKind::*;
    const ORDER: [crate::cli::teams_config::ReviewKind; 4] = [Commit, Plan, Final, Gate];
    let at = ORDER.iter().position(|k| *k == tier).unwrap_or(0);
    let next = if forward {
        (at + 1) % ORDER.len()
    } else {
        (at + ORDER.len() - 1) % ORDER.len()
    };
    ORDER[next]
}

/// The reviewer tier after toggling one checkbox — `None` = no-op.
/// Rules: `commit` is exclusive (ticking it clears `plan`/`final`;
/// ticking `plan`/`final` while in commit mode LEAVES commit mode); and
/// unticking the last remaining coverage is a no-op (a reviewer must
/// review something).
pub(super) fn tier_after_toggle(
    current: crate::cli::teams_config::ReviewKind,
    action: DetailAction,
) -> Option<crate::cli::teams_config::ReviewKind> {
    use crate::cli::teams_config::ReviewKind::*;
    let (commit, plan, final_) = tier_boxes(current);
    match action {
        DetailAction::TierCommit => (!commit).then_some(Commit),
        DetailAction::TierPlan => match (commit, plan, final_) {
            (true, _, _) => Some(Plan),
            (false, true, true) => Some(Final),
            (false, true, false) => None, // last coverage — keep it
            (false, false, _) => Some(if final_ { Gate } else { Plan }),
        },
        DetailAction::TierFinal => match (commit, final_, plan) {
            (true, _, _) => Some(Final),
            (false, true, true) => Some(Plan),
            (false, true, false) => None, // last coverage — keep it
            (false, false, _) => Some(if plan { Gate } else { Final }),
        },
        _ => None,
    }
}

/// A roster role's `ReviewKind` — `None` for master (no review tier).
pub(super) fn role_review_kind(
    role: crate::cli::teams_config::RosterRole,
) -> Option<crate::cli::teams_config::ReviewKind> {
    use crate::cli::teams_config::{ReviewKind, RosterRole};
    match role {
        RosterRole::Master => None,
        RosterRole::Commit => Some(ReviewKind::Commit),
        RosterRole::Plan => Some(ReviewKind::Plan),
        RosterRole::Final => Some(ReviewKind::Final),
        RosterRole::Gate => Some(ReviewKind::Gate),
    }
}

/// The roster role a `ReviewKind` persists as (for in-place snapshot
/// updates after a tier edit).
pub(super) fn review_kind_role(
    kind: crate::cli::teams_config::ReviewKind,
) -> crate::cli::teams_config::RosterRole {
    use crate::cli::teams_config::{ReviewKind, RosterRole};
    match kind {
        ReviewKind::Commit => RosterRole::Commit,
        ReviewKind::Plan => RosterRole::Plan,
        ReviewKind::Final => RosterRole::Final,
        ReviewKind::Gate => RosterRole::Gate,
    }
}

/// What a detail-page keystroke means — PURE, like `agent_panel_action`:
/// the loop executes the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailNav {
    None,
    Quit,
    /// Esc/Tab — return to the panel.
    Back,
    /// Move the action cursor to this row.
    MoveCursor(usize),
    /// Activate the action under the cursor.
    Activate(DetailAction),
    /// The wait page's kill row — routed to a confirm, never straight
    /// to a signal.
    ActivateKill,
}

/// What the panel cursor was ON, by IDENTITY rather than position.
///
/// A refresh rebuilds the row list, and positions move: a wait
/// appearing above the cursor shifts every row after it, and the old
/// arithmetic (`agents_len + 1 + i` for stash, the same for queue)
/// could not see waits at all. Captured before the rebuild, located
/// after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PanelAnchor {
    Agent(String),
    /// The wait belonging to that agent.
    /// A wait, by its full attendance INSTANCE. The owning agent's
    /// label alone would rebind onto a REPLACEMENT attendance for the
    /// same agent — a different wait wearing the same row.
    Wait {
        label: String,
        task: String,
        at: String,
    },
    Add,
    Stash(String),
    Queue(String),
}

/// What the cursor is on now, for re-locating after a refresh.
pub(super) fn capture_anchor(
    sel: usize,
    rows: &[PanelRow],
    agents: &[crate::cli::status::AgentAutoRow],
    stash_stems: &[String],
    queue_names: &[String],
) -> PanelAnchor {
    match rows.get(sel) {
        Some(PanelRow::Agent(i)) => agents
            .get(*i)
            .map(|a| PanelAnchor::Agent(a.label.clone()))
            .unwrap_or(PanelAnchor::Add),
        // The full instance: a replacement attendance under the same
        // agent is a different wait and must not inherit this cursor.
        Some(PanelRow::Wait(i)) => agents
            .get(*i)
            .and_then(|a| {
                a.attending.as_ref().map(|att| PanelAnchor::Wait {
                    label: a.label.clone(),
                    task: att.task.clone(),
                    at: att.at.clone(),
                })
            })
            .unwrap_or(PanelAnchor::Add),
        Some(PanelRow::Stash(i)) => stash_stems
            .get(*i)
            .map(|n| PanelAnchor::Stash(n.clone()))
            .unwrap_or(PanelAnchor::Add),
        Some(PanelRow::Queue(i)) => queue_names
            .get(*i)
            .map(|n| PanelAnchor::Queue(n.clone()))
            .unwrap_or(PanelAnchor::Add),
        _ => PanelAnchor::Add,
    }
}

/// Where that anchor sits in the rebuilt list.
///
/// A vanished wait falls back to its OWNING AGENT rather than to the
/// add row: the agent is still there and is what the wait hung under,
/// so that is where the user's attention already is. Everything else
/// that vanishes drops to "+ add".
pub(super) fn rebind_anchor(
    anchor: &PanelAnchor,
    rows: &[PanelRow],
    agents: &[crate::cli::status::AgentAutoRow],
    stash_stems: &[String],
    queue_names: &[String],
) -> usize {
    let find = |want: PanelRow| rows.iter().position(|r| *r == want);
    let by_label = |label: &str| agents.iter().position(|a| a.label == label);
    let at = match anchor {
        PanelAnchor::Agent(label) => by_label(label).and_then(|i| find(PanelRow::Agent(i))),
        // The SAME attendance, or else the agent it hung under. A
        // replacement is a DIFFERENT wait and must not inherit this
        // cursor.
        PanelAnchor::Wait { label, task, at } => by_label(label).and_then(|i| {
            let same = agents[i]
                .attending
                .as_ref()
                .is_some_and(|c| &c.task == task && &c.at == at);
            same.then(|| find(PanelRow::Wait(i)))
                .flatten()
                .or_else(|| find(PanelRow::Agent(i)))
        }),
        PanelAnchor::Stash(name) => stash_stems
            .iter()
            .position(|n| n == name)
            .and_then(|i| find(PanelRow::Stash(i))),
        PanelAnchor::Queue(name) => queue_names
            .iter()
            .position(|n| n == name)
            .and_then(|i| find(PanelRow::Queue(i))),
        PanelAnchor::Add => None,
    };
    at.unwrap_or_else(|| add_row_index(rows))
}

/// Re-locate an open detail page by LABEL after a roster rebuild:
/// the same agent's new index, or `None` if it's gone (close the page).
/// Identity is by label, never by a kept index — an external promote /
/// tier change reorders rows, so a kept index could retarget a
/// different agent.
pub(super) fn relocate_detail(
    label: &str,
    agents: &[crate::cli::status::AgentAutoRow],
) -> Option<usize> {
    agents.iter().position(|a| a.label == label)
}

/// A value-bearing row that ←/→ may flip in place (vs an action row that
/// only Enter/Space activates). All toggles are flips (auto on/off, a
/// checkbox tick/untick), so the flip is direction-agnostic.
pub(super) fn is_toggle(action: DetailAction) -> bool {
    matches!(
        action,
        DetailAction::ToggleAuto
            | DetailAction::TierCommit
            | DetailAction::TierPlan
            | DetailAction::TierFinal
    )
}

/// Pure key routing for the detail page. ↑↓ move the cursor; ←/→/␣ "change"
/// — they flip ONLY a toggle row (a no-op on an action row), so a stray
/// directional or space key can never fire the destructive `Remove` or
/// `PromoteToMaster`; only `Enter` "selects" (activates) an action. This
/// matches the hint exactly (←→ ␣ change · ⏎ select).
/// A row on the WAIT page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitAction {
    /// SIGTERM the recorded process — the one destructive action here,
    /// offered only when the process can still be identified.
    Kill,
    Back,
}

/// The wait page's actions. `killable` is false whenever the process
/// cannot be identified — no pid, a dead one, a record predating the
/// identity token, or a token that no longer matches the pid. The row
/// is then ABSENT rather than shown-and-inert: a control that cannot
/// work teaches nothing, and the page says why in its body.
pub(super) fn wait_actions(killable: bool) -> Vec<WaitAction> {
    if killable {
        vec![WaitAction::Kill, WaitAction::Back]
    } else {
        vec![WaitAction::Back]
    }
}

pub(super) fn wait_page_nav(sel: usize, actions: &[WaitAction], key: Key) -> DetailNav {
    match key {
        Key::Quit => DetailNav::Quit,
        Key::Escape | Key::Focus | Key::Char(b'a') => DetailNav::Back,
        Key::Up => DetailNav::MoveCursor(move_selection(sel, actions.len(), false)),
        Key::Down => DetailNav::MoveCursor(move_selection(sel, actions.len(), true)),
        Key::Enter => match actions.get(sel) {
            Some(WaitAction::Kill) => DetailNav::ActivateKill,
            _ => DetailNav::Back,
        },
        _ => DetailNav::None,
    }
}

pub(super) fn agent_detail_nav(sel: usize, actions: &[DetailAction], key: Key) -> DetailNav {
    let current = || actions.get(sel).copied().unwrap_or(DetailAction::Back);
    match key {
        Key::Quit => DetailNav::Quit,
        Key::Escape | Key::Focus | Key::Char(b'a') => DetailNav::Back,
        Key::Up => DetailNav::MoveCursor(move_selection(sel, actions.len(), false)),
        Key::Down => DetailNav::MoveCursor(move_selection(sel, actions.len(), true)),
        Key::Enter => DetailNav::Activate(current()),
        Key::Left | Key::Right | Key::Space if is_toggle(current()) => {
            DetailNav::Activate(current())
        }
        _ => DetailNav::None,
    }
}

/// What a keystroke means in a full-window document overlay (commit
/// detail or plan markdown): dismiss it, scroll its body by a signed line
/// delta, or nothing. The overlay is READ-ONLY, so (like a pager)
/// `q`/Esc/Enter back out to the log rather than quitting the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DocNav {
    None,
    Back,
    Scroll(i32),
    /// `o` — open the overlay's plan/commit as its HTML page in the browser.
    OpenHtml,
}

/// Pure key routing for a document overlay. `page` is the viewport height
/// for PgUp/PgDn/Space. The loop clamps the resulting offset to the
/// content height.
pub(super) fn doc_nav(key: Key, page: usize) -> DocNav {
    let page = page as i32;
    match key {
        Key::Escape | Key::Enter | Key::Quit | Key::Focus | Key::Char(b'a') | Key::Left => {
            DocNav::Back
        }
        Key::Up => DocNav::Scroll(-1),
        Key::Down => DocNav::Scroll(1),
        Key::PageUp => DocNav::Scroll(-page),
        Key::Space | Key::PageDown => DocNav::Scroll(page),
        Key::Html => DocNav::OpenHtml,
        _ => DocNav::None,
    }
}

/// A pending roster mutation, by index (keeps [`Mode`] `Copy`).
/// Plan-page actions carry NO payload — the target stem is the loop's
/// `plan_page` (same Copy-preserving indirection as the indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmAction {
    /// Add the candidate at this index in the loop's picker list.
    AddCandidate {
        idx: usize,
        /// `ReviewKind` has no `Master` variant, so "add as master"
        /// is unrepresentable here rather than rejected downstream.
        /// Adding a master means demoting the incumbent, which is
        /// `clank agent promote`'s job.
        tier: crate::cli::teams_config::ReviewKind,
    },
    /// Remove the agent at this index in `snapshot.agents`.
    RemoveAgent { idx: usize },
    /// `stash push` the open plan page's plan.
    StashPlan,
    /// `purge` (artifacts only) the open plan page's plan.
    PurgeArtifacts,
    /// `purge --drop` the open plan page's plan — the scariest one.
    PurgeDrop,
    /// SIGTERM the open wait page's process. Carries no pid: the
    /// target is re-resolved from the page's identity AFTER the
    /// confirm, because everything checked before it is stale by the
    /// time the answer arrives.
    KillAttended,
}

impl ConfirmAction {
    /// Add defaults to Yes (non-destructive); everything else defaults
    /// to No — so Enter (which follows the default) never confirms a
    /// removal, stash, or purge.
    pub(super) fn default_yes(self) -> bool {
        matches!(self, ConfirmAction::AddCandidate { .. })
    }
    /// True for confirms that came FROM the plan page. Rendering is
    /// page-shaped for every confirm; this is only for navigation and
    /// execution routing.
    pub(super) fn is_plan_page(self) -> bool {
        matches!(
            self,
            ConfirmAction::StashPlan | ConfirmAction::PurgeArtifacts | ConfirmAction::PurgeDrop
        )
    }
}

impl Mode {
    /// True for every agent-region sub-state (panel, picker, confirm) —
    /// the log is the only non-agents mode. Drives the focus rail/header.
    pub(super) fn agents_focused(self) -> bool {
        !matches!(self, Mode::LogScroll)
    }
    pub(super) fn log_focused(self) -> bool {
        matches!(self, Mode::LogScroll)
    }
    /// The agent-panel cursor row, else `None` (picker/confirm/log have
    /// no agent-row cursor).
    pub(super) fn selected(self) -> Option<usize> {
        match self {
            Mode::AgentPanel { sel } => Some(sel),
            _ => None,
        }
    }

    /// The Tab/focus-key transition: from the log, enter the panel at row
    /// 0 (only if there's a roster to enter); from any agent-region mode,
    /// return to the log. The single focus-toggle rule, shared by every
    /// handler arm.
    pub(super) fn toggle_focus(self, agents_len: usize) -> Mode {
        match self {
            Mode::LogScroll if agents_len > 0 => Mode::AgentPanel { sel: 0 },
            // The plan page isn't part of the log↔agents focus cycle;
            // Tab is a no-op there (esc leaves the page).
            m @ (Mode::PlanDetail { .. } | Mode::PurgeChoice { .. }) => m,
            _ => Mode::LogScroll,
        }
    }
}

/// The open WAIT page's identity, held beside the `Copy` [`Mode`].
///
/// The ATTENDANCE INSTANCE, not just the agent and task: the same
/// agent can attend the same task again, and a replacement record
/// carries a new pid, description and `at`. Keying on label+task alone
/// would leave this page — or a confirm standing on top of it —
/// pointed at a different wait than the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaitPage {
    pub(super) label: String,
    pub(super) task: String,
    /// Per-record, so a replacement is a DIFFERENT instance.
    pub(super) at: String,
}

impl WaitPage {
    /// Is this still the same attendance? Compared in full — agent,
    /// task and instance.
    pub(super) fn matches(&self, label: &str, att: &crate::cli::stop_hook::Attended) -> bool {
        self.label == label && self.task == att.task && self.at == att.at
    }
}

/// The open EVENT page (tui-github-event-page): the target copy key,
/// the retained component member-key set (the retarget lookup — the
/// fresh snapshot cannot reconstruct membership from one key), and
/// the freshly-derived merged event. Set/cleared with
/// [`Mode::EventDetail`].
#[derive(Debug, Clone)]
pub(super) struct EventPage {
    /// The targeted copy — the FULL key.
    pub(super) target: crate::cli::github_timeline::MemberKey,
    /// The component's sorted member keys as of the last successful
    /// refresh; retargeting follows the first surviving key.
    pub(super) retained: Vec<crate::cli::github_timeline::MemberKey>,
    /// The component's current merged view.
    pub(super) event: crate::cli::github_timeline::MergedEvent,
    /// Standing watch prompts, deduplicated: `None` attribution when
    /// every member's prompt agrees, per-agent attribution when they
    /// differ (tui-github-event-page).
    pub(super) prompts: Vec<(Option<String>, String)>,
    /// Scroll offset into the details body (facts/members/prompts) —
    /// the plan page's document-scroll model, so long multiline
    /// prompts are always reachable.
    pub(super) scroll: usize,
    /// The GitHub body request, when this event names an object to
    /// read (tui-github-event-content). `None` for legacy rows and
    /// pushes, which name none.
    pub(super) content: Option<super::event_content::ContentSlot>,
}

/// One row of the event page's action menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventAction {
    /// Open the event URL via the platform opener.
    OpenBrowser,
    /// Ack every unhandled copy (the fanout through the shared core).
    Ack,
    /// Re-request the GitHub body. Offered ONLY while the content is
    /// in a retryable failure, so it never becomes a key that sits
    /// there doing nothing (tui-github-event-content, Contract 4).
    Retry,
    /// Close the page.
    Back,
}

/// An event action's key and the label the page shows for it —
/// defined together, in one arm per action, so the two cannot drift.
/// The render draws `.1`; [`event_hotkey`] matches `.0`. Adding an
/// action with a key that looks live and does nothing now requires
/// going out of your way (tui-event-page-hotkeys).
pub(super) fn event_action_key(a: EventAction) -> (Key, &'static str) {
    match a {
        EventAction::OpenBrowser => (Key::Html, "o"),
        // The physical letter, NOT `Key::Focus`: that variant is Tab
        // too, and acking every copy is destructive.
        EventAction::Ack => (Key::Char(b'a'), "a"),
        EventAction::Retry => (Key::Char(b'r'), "r"),
        EventAction::Back => (Key::Escape, "esc"),
    }
}

/// Resolve a keypress to an event-page action, gated on the action
/// being OFFERED right now — mirroring `plan_hotkey`. A no-URL event
/// has no browser row, so `o` is inert there rather than launching
/// nothing; a fully-handled event has no ack row, so `a` cannot
/// re-ack it.
pub(super) fn event_hotkey(key: Key, actions: &[EventAction]) -> Option<EventAction> {
    actions
        .iter()
        .copied()
        .find(|a| event_action_key(*a).0 == key)
}

/// The event page's actions in display order: unavailable actions are
/// OMITTED (no URL → no browser row; fully handled → no ack row),
/// matching how the other pages degrade.
pub(super) fn event_actions(has_url: bool, unhandled: bool, retryable: bool) -> Vec<EventAction> {
    let mut out = Vec::new();
    if has_url {
        out.push(EventAction::OpenBrowser);
    }
    if unhandled {
        out.push(EventAction::Ack);
    }
    if retryable {
        out.push(EventAction::Retry);
    }
    out.push(EventAction::Back);
    out
}

/// The actions a page currently offers, read off the page itself so
/// availability and the page's real state cannot disagree.
pub(super) fn actions_for(ep: &EventPage) -> Vec<EventAction> {
    event_actions(
        event_open_url(ep).is_some(),
        ep.event.unhandled,
        ep.content.as_ref().is_some_and(|c| c.state.is_retryable()),
    )
}

/// Keep the cursor on the SAME action across an availability change.
///
/// Selection is an INDEX, but the action list changes asynchronously:
/// a failed fetch inserts Retry ABOVE Back, so the index that meant
/// "close the page" silently comes to mean "retry". Rebinding by
/// identity is what stops a completion landing under the operator's
/// cursor from repurposing the key they were about to press.
///
/// If the selected action is gone entirely, fall back to the LAST
/// entry — always Back, and the only action that cannot do anything
/// the operator did not ask for.
pub(super) fn rebind_event_sel(before: &[EventAction], sel: usize, after: &[EventAction]) -> usize {
    let last = after.len().saturating_sub(1);
    match before.get(sel) {
        Some(want) => after.iter().position(|a| a == want).unwrap_or(last),
        None => last,
    }
}

/// Carry the detail-page cursor across a change in its action list —
/// the reopen row appearing or leaving as presence changes — by the
/// ACTION under it, not its index. Clamping the index alone repurposes
/// the key: a reviewer page with reopen selected at 6 whose pane comes
/// back has `remove` at 6 (codex on e310988). A vanished action falls
/// back to the last row, always Back, the one action that cannot do
/// anything the operator did not ask for.
pub(super) fn rebind_detail_sel(
    before: &[DetailAction],
    sel: usize,
    after: &[DetailAction],
) -> usize {
    let last = after.len().saturating_sub(1);
    match before.get(sel) {
        Some(want) => after.iter().position(|a| a == want).unwrap_or(last),
        None => last,
    }
}

/// Before a frame paints an agent's page: presence may have changed
/// the action list since the last painted one, so carry the cursor
/// across by its action and record what THIS frame will show. Off the
/// page nothing is shown. Pure, so the frame sequence a worker wake
/// produces — no key in between — is testable (codex on 44e937e).
///
/// `shown` is `None` when no page was painted last frame — entering
/// the page from the panel — and then the requested cursor stands
/// (clamped): there is no earlier list whose action it could mean, and
/// rebinding from nothing sent every first entry to Back (codex on
/// 18b70d3).
pub(super) fn settle_detail_cursor(
    mode: Mode,
    agents: &[crate::cli::status::AgentAutoRow],
    presence: &Option<std::collections::BTreeSet<String>>,
    shown: &mut Option<Vec<DetailAction>>,
) -> Mode {
    let Mode::AgentDetail { idx, sel } = mode else {
        *shown = None;
        return mode;
    };
    let Some(agent) = agents.get(idx) else {
        *shown = None;
        return mode;
    };
    let now = detail_actions(agent.role, presence_in(presence, &agent.label));
    let sel = match shown {
        Some(last) if *last != now => rebind_detail_sel(last, sel, &now),
        Some(_) => sel,
        None => sel.min(now.len().saturating_sub(1)),
    };
    *shown = Some(now);
    Mode::AgentDetail { idx, sel }
}

/// Whether `label` has a live pane, given the worker's last report.
pub(super) fn presence_in(
    presence: &Option<std::collections::BTreeSet<String>>,
    label: &str,
) -> Presence {
    match presence {
        None => Presence::Unknown,
        Some(live) if live.contains(label) => Presence::Live,
        Some(_) => Presence::Missing,
    }
}

/// The URL the browser action opens: the FETCHED object's own link
/// when we have it — for a comment that is the anchored permalink the
/// event record never carried — else the record's issue/PR URL.
pub(super) fn event_open_url(ep: &EventPage) -> Option<String> {
    if let Some(slot) = &ep.content
        && let super::event_content::ContentState::Ready(b) = &slot.state
        && let Some(u) = &b.url
    {
        return Some(u.clone());
    }
    ep.event.url.clone()
}

/// The EXECUTED effect of an event-page action — the tested seam
/// codex 525cef1 asked for: the URL extraction and the fanout list
/// are resolved HERE (pure, recorded in tests with the exact URL);
/// the loop only routes `Open` to the shared platform opener and
/// `AckFanout` to the shared ack core.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EventEffect {
    /// Launch this exact URL via the shared opener.
    Open(String),
    /// Fan these member keys through the shared events-ack core.
    AckFanout(Vec<crate::cli::github_timeline::MemberKey>),
    /// Close the page.
    Close,
    /// Re-request the body under a fresh generation.
    RetryContent,
    /// Nothing to do (e.g. OpenBrowser with no URL — the menu omits
    /// it, this is the belt).
    None,
}

pub(super) fn event_action_effect(ep: &EventPage, a: EventAction) -> EventEffect {
    match a {
        EventAction::OpenBrowser => event_open_url(ep)
            .map(EventEffect::Open)
            .unwrap_or(EventEffect::None),
        EventAction::Ack => EventEffect::AckFanout(
            ep.event
                .members
                .iter()
                .filter(|m| !m.acked)
                .map(|m| m.key.clone())
                .collect(),
        ),
        EventAction::Retry => EventEffect::RetryContent,
        EventAction::Back => EventEffect::Close,
    }
}

/// Event-page navigation outcome — pure key routing, IO stays in
/// the loop (same shape as [`PlanNav`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EventNav {
    Sel(usize),
    /// Scroll the details body (the plan page's continuous
    /// buttons→document model).
    Scroll(i32),
    Back,
    Act(EventAction),
    None,
}

pub(super) fn event_detail_nav(
    sel: usize,
    actions: &[EventAction],
    k: Key,
    page: usize,
    scroll: usize,
) -> EventNav {
    let page = page as i32;
    let last = actions.len().saturating_sub(1);
    match k {
        // Esc AND q return to the log (the page never quits the TUI —
        // tui-github-event-page).
        Key::Escape | Key::Quit => EventNav::Back,
        Key::Up if sel == last && scroll > 0 => EventNav::Scroll(-1),
        Key::Up => EventNav::Sel(sel.saturating_sub(1)),
        Key::Down if sel == last => EventNav::Scroll(1),
        Key::Down => EventNav::Sel((sel + 1).min(last)),
        Key::PageUp => EventNav::Scroll(-page),
        Key::Space | Key::PageDown => EventNav::Scroll(page),
        Key::Enter => actions
            .get(sel)
            .map_or(EventNav::None, |a| EventNav::Act(*a)),
        _ => EventNav::None,
    }
}

/// The open plan page's identity + derived facts (tui-plan-actions-page).
/// Lives in the loop beside `mode` (which stays `Copy`); set and cleared
/// together with `Mode::PlanDetail`/`PurgeChoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanPage {
    pub(super) stem: String,
    pub(super) st: PlanPageState,
    /// The plan document's markdown, read once when the page opens
    /// (and on refresh) — rendered beneath the buttons. `None` when the
    /// file is unreadable.
    pub(super) body: Option<String>,
    /// Scroll offset into the rendered document body.
    pub(super) scroll: usize,
}

/// The interactive state `render_at` needs beyond the snapshot: which
/// mode owns the keyboard and the freshly-read add-picker candidates.
/// Bundled so the render signature stays small (and future interactive
/// bits land here, not as more args).
pub(super) struct PanelView<'a> {
    pub(super) mode: Mode,
    pub(super) plan_page: Option<&'a PlanPage>,
    pub(super) event_page: Option<&'a EventPage>,
    pub(super) wait_page: Option<&'a WaitPage>,
    pub(super) plan_input: Option<&'a TextInput>,
    pub(super) picker: &'a [crate::cli::status::AvailableAgent],
    /// The selected log ENTRY (index into the scroll sequence) — drawn
    /// with the unified selection band when the log is focused.
    pub(super) log_cursor: usize,
    /// Whole-pane pressure offset (tui-short-pane-whole-scroll): how
    /// many SCROLLABLE header rows (gauges, agents/stash/queue
    /// sections — never the bar) are scrolled off the top to give a
    /// focused log its minimum viewport in a short pane. 0 = today's
    /// rendering, byte for byte.
    pub(super) lift: usize,
    /// Whether zellij answers, as the reconcile worker last saw it.
    pub(super) reach: crate::cli::status_tui::zellij::ZellijReach,
    /// This repo's labels with a live pane, as the worker last listed
    /// them; `None` until a listing answers, or outside zellij.
    pub(super) presence: Option<std::collections::BTreeSet<String>>,
}

impl<'a> PanelView<'a> {
    pub(super) fn presence_of(&self, label: &str) -> Presence {
        presence_in(&self.presence, label)
    }

    /// A view with just a mode (no picker, cursor at 0) — the common
    /// case for tests and the log-scroll default.
    #[cfg(test)]
    pub(super) fn just(mode: Mode) -> Self {
        Self {
            mode,
            plan_page: None,
            event_page: None,
            wait_page: None,
            plan_input: None,
            picker: &[],
            log_cursor: 0,
            lift: 0,
            reach: crate::cli::status_tui::zellij::ZellijReach::NotInSession,
            presence: None,
        }
    }
}

/// What an `AgentPanel` keystroke means — PURE routing over the cursor
/// position and the selected row's role, so every case is unit-tested
/// and the loop only executes the result (the sole place IO happens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelAction {
    None,
    Quit,
    /// Tab/Esc — hand focus back to the log.
    LeaveFocus,
    /// Move the cursor to this row.
    MoveCursor(usize),
    /// `Down` past the last panel row ("+ add") — cross into the log.
    EnterLog,
    /// Toggle auto on the agent at this index (SPC, quick path).
    ToggleAuto(usize),
    /// Open the per-agent detail page (Enter on an agent row).
    OpenDetail(usize),
    /// Open the wait attended by that agent.
    OpenWait(usize),
    /// Open the add picker (Enter/Space on the "+ add" row).
    OpenPicker,
    /// Open a queued plan's read overlay (Enter on a queue row).
    OpenQueueItem(usize),
    /// Open a queued plan's HTML page in the browser (`o` on a queue row).
    OpenQueueHtml(usize),
    /// Open a stashed plan's read overlay (Enter on a STASH row) — the
    /// body comes from the record's protective ref.
    OpenStashItem(usize),
    /// Open a stashed plan's HTML page in the browser (`o` on a STASH row).
    OpenStashHtml(usize),
    /// Nudge a queue row's priority by `delta` (+/- on a queue row); the
    /// loop clamps to 0-999 and writes through `queue::set_priority`.
    NudgeQueue {
        idx: usize,
        delta: i16,
    },
}

/// Pure key routing for the agent panel. `agents` is the roster; the
/// "+ add" row sits at index `agents.len()`; `queue_len` QUEUE rows
/// follow at `agents.len()+1..` (Enter reads the queued plan, `o` opens
/// its HTML page, +/- nudge its priority). Removal/role/tier are no
/// longer panel actions — Enter opens the detail page where they live.
/// One CURSOR POSITION in the agent panel, knowing what it is.
///
/// The panel used to derive this arithmetically — `agents.len()` was
/// the add button, stash rows were `sel - add_row - 1`, and so on —
/// which works only while drawn rows and cursor positions correspond
/// one to one. They no longer do: an attending agent draws a wait line
/// beneath it. Positions are enumerated now, so a new row kind cannot
/// silently shift the arithmetic under every other segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelRow {
    Agent(usize),
    /// The wait attended by that agent, drawn beneath its row.
    Wait(usize),
    Add,
    Stash(usize),
    Queue(usize),
}

/// Every cursor position in the panel, in screen order. `waits` holds
/// the agent indices whose wait is BOTH live and drawable — a row the
/// cursor can reach must be a row the user can see.
pub(super) fn panel_rows(
    agents_len: usize,
    waits: &[usize],
    stash_len: usize,
    queue_len: usize,
) -> Vec<PanelRow> {
    let mut out = Vec::with_capacity(agents_len + waits.len() + 1 + stash_len + queue_len);
    for i in 0..agents_len {
        out.push(PanelRow::Agent(i));
        if waits.contains(&i) {
            out.push(PanelRow::Wait(i));
        }
    }
    out.push(PanelRow::Add);
    out.extend((0..stash_len).map(PanelRow::Stash));
    out.extend((0..queue_len).map(PanelRow::Queue));
    out
}

/// The cursor position of the "+ add" row — the fallback every action
/// that leaves a picker or removes an agent returns to.
pub(super) fn add_row_index(rows: &[PanelRow]) -> usize {
    rows.iter()
        .position(|r| matches!(r, PanelRow::Add))
        .unwrap_or(0)
}

pub(super) fn agent_panel_action(sel: usize, rows: &[PanelRow], key: Key) -> PanelAction {
    let total = rows.len();
    let here = rows.get(sel).copied();
    let on_add = matches!(here, Some(PanelRow::Add));
    let stash_idx = match here {
        Some(PanelRow::Stash(i)) => Some(i),
        _ => None,
    };
    let queue_idx = match here {
        Some(PanelRow::Queue(q)) => Some(q),
        _ => None,
    };
    let on_last = sel + 1 == total;
    match key {
        Key::Quit => PanelAction::Quit,
        Key::Focus | Key::Char(b'a') | Key::Escape => PanelAction::LeaveFocus,
        Key::Up => PanelAction::MoveCursor(move_selection(sel, total, false)),
        // `Down` past the bottom row flows into the log — continuous
        // navigation across the panel↔log boundary.
        Key::Down if on_last => PanelAction::EnterLog,
        Key::Down => PanelAction::MoveCursor(move_selection(sel, total, true)),
        // Enter activates the row: the picker on "+ add", a read overlay
        // on a queue row, the detail page on an agent. Space is the quick
        // inline auto-toggle (or the picker on "+ add"); it does nothing
        // on a queue row (no accidental overlay).
        Key::Enter if on_add => PanelAction::OpenPicker,
        Key::Enter => match here {
            Some(PanelRow::Stash(i)) => PanelAction::OpenStashItem(i),
            Some(PanelRow::Queue(q)) => PanelAction::OpenQueueItem(q),
            Some(PanelRow::Agent(i)) => PanelAction::OpenDetail(i),
            Some(PanelRow::Wait(i)) => PanelAction::OpenWait(i),
            _ => PanelAction::None,
        },
        Key::Html => match (stash_idx, queue_idx) {
            (Some(i), _) => PanelAction::OpenStashHtml(i),
            (_, Some(q)) => PanelAction::OpenQueueHtml(q),
            _ => PanelAction::None,
        },
        Key::Plus if queue_idx.is_some() => PanelAction::NudgeQueue {
            idx: queue_idx.unwrap(),
            delta: 50,
        },
        Key::Minus if queue_idx.is_some() => PanelAction::NudgeQueue {
            idx: queue_idx.unwrap(),
            delta: -50,
        },
        Key::Space if on_add => PanelAction::OpenPicker,
        // Space is the agent's inline auto-toggle and belongs to no
        // other row kind — a wait has no auto mode to flip.
        Key::Space => match here {
            Some(PanelRow::Agent(i)) => PanelAction::ToggleAuto(i),
            _ => PanelAction::None,
        },
        _ => PanelAction::None,
    }
}

/// Resolve a Confirm keystroke: `Some(true)` confirm, `Some(false)`
/// cancel, `None` ignore. In a confirm `q` CANCELS (it does not quit
/// the TUI), and Enter follows the action's default — so a destructive
/// default is never confirmed by Enter.
pub(super) fn confirm_decision(action: ConfirmAction, key: Key) -> Option<bool> {
    match key {
        Key::Yes => Some(true),
        Key::No | Key::Escape | Key::Quit => Some(false),
        Key::Enter => Some(action.default_yes()),
        _ => None,
    }
}

/// Parse a burst of stdin bytes into scroll keys — arrow keys,
/// PgUp/PgDn, plus vi-ish `j`/`k`/`g`/`G`, space (page down), `q` (quit).
/// One keystroke for a TEXT INPUT: printable bytes are literal text
/// (vi keys, y/n/o/q — everything — because "block reason" needs to
/// contain the letter j). Only the structural keys keep meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextKey {
    Char(u8),
    Backspace,
    Left,
    Right,
    Enter,
    Esc,
    /// A consumed-but-meaningless sequence (Up/Down, paging, unknown
    /// CSI, control bytes) — explicit so no arm smuggles a NUL into
    /// the buffer.
    Ignore,
}

/// One parsed input item: a command key (normal modes) or a text key
/// (a text input is open). The MODE picks the interpretation at the
/// loop — bytes → keys is interpretation, so it can't live in the
/// stdin thread (which doesn't know the mode): a context-free parse
/// would eat the letters j/k/g/b/y/n/o/q out of typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnyKey {
    Cmd(Key),
    Text(TextKey),
}

/// Parse ONE key from the head of `bytes` under the given
/// interpretation; returns the key and the bytes consumed, or `None`
/// for an unmapped head byte (caller skips 1). Incremental so the loop
/// can re-pick the interpretation between keys — a pasted burst like
/// `f<subject>\r` crosses INTO input mode mid-buffer and must not have
/// its text pre-parsed as commands.
pub(super) fn parse_one(bytes: &[u8], text_mode: bool) -> Option<(AnyKey, usize)> {
    use AnyKey::{Cmd, Text};
    if bytes.is_empty() {
        return None;
    }
    // CSI sequences: arrows keep structural meaning in both modes
    // (Left/Right move the input cursor); Up/Down and paging are
    // SWALLOWED in a one-line input rather than reinterpreted.
    let csi: &[(&[u8], Key, Option<TextKey>)] = &[
        (b"\x1b[A", Key::Up, None),
        (b"\x1b[B", Key::Down, None),
        (b"\x1b[D", Key::Left, Some(TextKey::Left)),
        (b"\x1b[C", Key::Right, Some(TextKey::Right)),
        (b"\x1b[5~", Key::PageUp, None),
        (b"\x1b[6~", Key::PageDown, None),
    ];
    for (seq, cmd, text) in csi {
        if bytes.starts_with(seq) {
            return Some(match (text_mode, text) {
                (false, _) => (Cmd(*cmd), seq.len()),
                (true, Some(t)) => (Text(*t), seq.len()),
                (true, None) => (Text(TextKey::Ignore), seq.len()),
            });
        }
    }
    if bytes.starts_with(b"\x1b[") {
        // Unknown CSI (F-keys, etc.): consume through its final byte
        // so a lone Esc isn't misread out of the sequence's leading
        // bytes.
        let mut j = 2;
        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
            j += 1;
        }
        let used = (j + 1).min(bytes.len());
        return Some((Text(TextKey::Ignore), used)); // swallowed in both modes
    }
    let b = bytes[0];
    if text_mode {
        let t = match b {
            b'\r' | b'\n' => TextKey::Enter,
            0x7f | 0x08 => TextKey::Backspace,
            0x1b => TextKey::Esc,
            c if (0x20..0x7f).contains(&c) => TextKey::Char(c),
            _ => TextKey::Ignore,
        };
        return Some((Text(t), 1));
    }
    let k = match b {
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'g' => Key::Top,
        b'G' => Key::Bottom,
        b' ' => Key::Space,
        b'b' => Key::PageUp,
        b'\t' => Key::Focus,
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Delete,
        b'y' => Key::Yes,
        b'n' => Key::No,
        b'o' => Key::Html,
        b'+' => Key::Plus,
        b'-' => Key::Minus,
        0x1b => Key::Escape,
        b'q' => Key::Quit,
        c if (0x20..0x7f).contains(&c) => Key::Char(c),
        _ => return Some((Text(TextKey::Ignore), 1)), // swallowed
    };
    Some((Cmd(k), 1))
}

pub(super) fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match parse_one(&bytes[i..], false) {
            Some((AnyKey::Cmd(k), used)) => {
                keys.push(k);
                i += used;
            }
            Some((_, used)) => i += used,
            None => i += 1,
        }
    }
    keys
}

/// The opposite armed state — what a SPC toggle writes.
pub(super) fn flip_auto(mode: clank_core::vocab::AutoMode) -> clank_core::vocab::AutoMode {
    use clank_core::vocab::AutoMode;
    match mode {
        AutoMode::On => AutoMode::Off,
        AutoMode::Off => AutoMode::On,
    }
}

/// Move the agent-panel cursor within `[0, len)`, saturating at both
/// ends (no wrap). `len == 0` pins it at 0 (an empty roster has no
/// selectable rows; the panel can't be focused then anyway).
pub(super) fn move_selection(sel: usize, len: usize, down: bool) -> usize {
    let last = len.saturating_sub(1);
    if down {
        (sel + 1).min(last)
    } else {
        sel.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_displayed_event_key_activates_the_action_it_names() {
        // Driven from the action list itself, not a hand-written key
        // table: a future action gets this coverage for free, which
        // is the drift that made `o` and `a` inert to begin with.
        for a in [
            EventAction::OpenBrowser,
            EventAction::Ack,
            EventAction::Back,
        ] {
            let (key, label) = event_action_key(a);
            let offered = [a];
            assert_eq!(
                event_hotkey(key, &offered),
                Some(a),
                "the key displayed as `{label}` must activate {a:?}"
            );
        }
    }

    #[test]
    fn event_hotkeys_are_gated_on_the_action_being_offered() {
        // A no-URL event has no browser row and a handled event has no
        // ack row; their keys must be inert rather than firing at
        // something that is not there.
        let none: Vec<EventAction> = event_actions(false, false, false);
        assert_eq!(event_hotkey(Key::Html, &none), None, "no URL → `o` inert");
        assert_eq!(
            event_hotkey(Key::Char(b'a'), &none),
            None,
            "handled → ack inert"
        );

        let both = event_actions(true, true, false);
        assert_eq!(
            event_hotkey(Key::Html, &both),
            Some(EventAction::OpenBrowser)
        );
        assert_eq!(event_hotkey(Key::Char(b'a'), &both), Some(EventAction::Ack));
    }

    #[test]
    fn tab_never_acks_and_the_letter_a_does() {
        // MUST start from raw bytes. The whole hazard was that `\t`
        // and `a` became the same `Key`, so a test starting from a
        // `Key` value starts after the bug (codex on 6028cb0).
        let key_of = |b: &[u8]| {
            let keys = parse_keys(b);
            assert_eq!(keys.len(), 1, "one key per byte here: {keys:?}");
            keys[0]
        };
        let actions = event_actions(true, true, false);

        let tab = key_of(b"\t");
        assert_eq!(tab, Key::Focus, "Tab stays the focus key");
        assert_eq!(
            event_hotkey(tab, &actions),
            None,
            "Tab must NEVER ack — it is navigation, and ack is destructive"
        );

        let letter = key_of(b"a");
        assert_ne!(letter, Key::Focus, "`a` must survive the parse as itself");
        assert_eq!(
            event_hotkey(letter, &actions),
            Some(EventAction::Ack),
            "the advertised `a` must actually ack"
        );

        // And `o` likewise, through the parser.
        assert_eq!(
            event_hotkey(key_of(b"o"), &actions),
            Some(EventAction::OpenBrowser)
        );
    }

    #[test]
    fn the_letter_a_still_means_focus_where_it_always_did() {
        // Splitting the parse must not retire the alias: every site
        // that documented `a` as focus keeps it.
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agents = vec![agent_row("claude", RosterRole::Master, AutoMode::On)];
        assert_eq!(
            agent_panel_action(0, &panel_rows(agents.len(), &[], 0, 0), Key::Char(b'a')),
            agent_panel_action(0, &panel_rows(agents.len(), &[], 0, 0), Key::Focus),
            "panel: `a` and Tab both leave focus"
        );
        let acts = [DetailAction::Back];
        assert!(matches!(
            agent_detail_nav(0, &acts, Key::Char(b'a')),
            DetailNav::Back
        ));
        assert!(matches!(doc_nav(Key::Char(b'a'), 10), DocNav::Back));
    }

    #[test]
    fn the_purge_choosers_advertised_a_now_reaches_it() {
        // Collateral proof the parse was the defect: this arm existed
        // all along and was unreachable, because `a` never survived
        // the parser.
        let keys = parse_keys(b"a");
        assert!(matches!(
            purge_choice_nav(1, keys[0]),
            PlanNavPurge::Choose(PurgeChoice::Artifacts)
        ));
    }

    #[test]
    fn event_detail_nav_routes_keys_to_actions() {
        // The action seam (tui-github-event-page): Enter on a row
        // RETURNS the action — the loop performs the IO — so
        // open-in-browser and ack are pinned here without spawning
        // anything, exactly like the plan page's nav.
        use super::{EventAction, EventNav, event_actions, event_detail_nav};
        let actions = event_actions(true, true, false);
        assert_eq!(
            actions,
            vec![
                EventAction::OpenBrowser,
                EventAction::Ack,
                EventAction::Back
            ]
        );
        assert_eq!(
            event_detail_nav(0, &actions, Key::Enter, 5, 0),
            EventNav::Act(EventAction::OpenBrowser)
        );
        assert_eq!(
            event_detail_nav(1, &actions, Key::Enter, 5, 0),
            EventNav::Act(EventAction::Ack)
        );
        assert_eq!(
            event_detail_nav(0, &actions, Key::Down, 5, 0),
            EventNav::Sel(1)
        );
        // Down at the LAST button crosses into the details body —
        // the plan page's continuous buttons→document model.
        assert_eq!(
            event_detail_nav(2, &actions, Key::Down, 5, 0),
            EventNav::Scroll(1)
        );
        assert_eq!(
            event_detail_nav(2, &actions, Key::Up, 5, 3),
            EventNav::Scroll(-1),
            "Up climbs back out through the body top"
        );
        assert_eq!(
            event_detail_nav(0, &actions, Key::PageDown, 5, 0),
            EventNav::Scroll(5)
        );
        // q returns to the log — the page never quits the TUI.
        assert_eq!(
            event_detail_nav(0, &actions, Key::Quit, 5, 0),
            EventNav::Back
        );
        assert_eq!(
            event_detail_nav(0, &actions, Key::Up, 5, 0),
            EventNav::Sel(0)
        );
        assert_eq!(
            event_detail_nav(1, &actions, Key::Escape, 5, 0),
            EventNav::Back
        );
        // Degraded menu: no URL, handled → only Back, and Enter on it
        // closes.
        let only_back = event_actions(false, false, false);
        assert_eq!(
            event_detail_nav(0, &only_back, Key::Enter, 5, 0),
            EventNav::Act(EventAction::Back)
        );
    }

    use super::*;
    use crate::cli::status_tui::fixtures::agent_row;

    #[test]
    fn parse_keys_recognizes_arrows_paging_and_vi_keys() {
        use Key::*;
        let got: Vec<_> = parse_keys(b"jk gGq")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want: Vec<_> = [Down, Up, Space, Top, Bottom, Quit]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(got, want, "vi keys + space");
        // CSI escape sequences (arrows, PgUp/PgDn), even back-to-back.
        let csi: Vec<_> = parse_keys(b"\x1b[A\x1b[B\x1b[5~\x1b[6~")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want_csi: Vec<_> = [Up, Down, PageUp, PageDown]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(csi, want_csi, "arrows + page keys");
        // Printable bytes without a binding now parse as Char (the plan
        // page's mode-scoped hotkeys / future text input); NON-printable
        // unmapped bytes are still dropped.
        assert_eq!(
            parse_keys(b"xz."),
            vec![Char(b'x'), Char(b'z'), Char(b'.')],
            "printables fall through as Char"
        );
        assert!(parse_keys(b"\x01\x02").is_empty(), "control bytes ignored");
    }

    // ── tui-plan-actions-page: row derivation + routing ──

    fn active_st() -> PlanPageState {
        PlanPageState {
            finished: false,
            multi_commit: true,
            repo_paused: false,
        }
    }

    #[test]
    fn plan_actions_active_page_has_stash_force_finish_and_purge() {
        use PlanAction::*;
        assert_eq!(
            plan_actions(active_st()),
            vec![OpenHtml, Stash, ForceFinish, Purge, Back]
        );
    }

    #[test]
    fn a_paused_repo_adds_no_row_to_the_plan_page() {
        // Pause is repo state driven by the global `b`; the plan page
        // must not grow a per-plan block toggle again.
        let st = PlanPageState {
            repo_paused: true,
            ..active_st()
        };
        assert_eq!(plan_actions(st), plan_actions(active_st()));
    }

    #[test]
    fn plan_actions_finished_page_swaps_the_middle_block_for_squash() {
        use PlanAction::*;
        let st = PlanPageState {
            finished: true,
            multi_commit: true,
            repo_paused: false,
        };
        assert_eq!(plan_actions(st), vec![OpenHtml, Squash, Purge, Back]);
        // Already-collapsed plan: nothing to squash — the row is absent.
        let one = PlanPageState {
            multi_commit: false,
            ..st
        };
        assert!(!plan_actions(one).contains(&Squash));
    }

    #[test]
    fn plan_hotkeys_only_fire_for_rows_present_on_the_page() {
        use PlanAction::*;
        let active = plan_actions(active_st());
        assert_eq!(plan_hotkey(Key::Char(b's'), &active), Some(Stash));
        assert_eq!(plan_hotkey(Key::Char(b'f'), &active), Some(ForceFinish));
        assert_eq!(plan_hotkey(Key::Char(b'p'), &active), Some(Purge));
        assert_eq!(plan_hotkey(Key::Html, &active), Some(OpenHtml));
        assert_eq!(
            plan_hotkey(Key::Char(b'c'), &active),
            None,
            "no squash on active"
        );
        assert_eq!(
            plan_hotkey(Key::Char(b'b'), &active),
            None,
            "`b` is global pause, never a plan-page row"
        );
        let finished = plan_actions(PlanPageState {
            finished: true,
            multi_commit: true,
            repo_paused: false,
        });
        assert_eq!(plan_hotkey(Key::Char(b'c'), &finished), Some(Squash));
        assert_eq!(
            plan_hotkey(Key::Char(b's'), &finished),
            None,
            "no stash on finished"
        );
    }

    #[test]
    fn plan_detail_nav_moves_activates_scrolls_and_backs_out() {
        let actions = plan_actions(active_st());
        let last = actions.len() - 1;
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Down, 10, 0),
            PlanNav::Sel(1)
        );
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Up, 10, 0),
            PlanNav::Sel(0)
        );
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Enter, 10, 0),
            PlanNav::Act(PlanAction::OpenHtml)
        );
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Escape, 10, 0),
            PlanNav::Back
        );
        // A hotkey acts regardless of the cursor.
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Char(b'p'), 10, 0),
            PlanNav::Act(PlanAction::Purge)
        );
        // Paging keys scroll the document body from any row.
        assert_eq!(
            plan_detail_nav(0, &actions, Key::PageDown, 10, 0),
            PlanNav::Scroll(10)
        );
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Space, 10, 0),
            PlanNav::Scroll(10)
        );
        assert_eq!(
            plan_detail_nav(0, &actions, Key::PageUp, 10, 0),
            PlanNav::Scroll(-10)
        );
        // ── the options→document crossing (the-plan-page-cursor-enters-the-document) ──
        // ↓ on the LAST button moves focus into the document — the
        // panel's step into the log — and does not scroll yet.
        let doc = document_focus(&actions);
        assert_eq!(
            plan_detail_nav(last, &actions, Key::Down, 10, 0),
            PlanNav::Sel(doc)
        );
        // With the document focused, ↓ scrolls and keeps scrolling.
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Down, 10, 0),
            PlanNav::Scroll(1)
        );
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Down, 10, 5),
            PlanNav::Scroll(1)
        );
        // ↑ climbs back out THROUGH the document's top: unscroll first,
        // then return to the last button.
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Up, 10, 3),
            PlanNav::Scroll(-1)
        );
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Up, 10, 0),
            PlanNav::Sel(last),
            "at the document top, ↑ returns to the buttons"
        );
        // Nothing to select in prose: Enter is a no-op there, so a
        // stale keypress cannot fire `back`.
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Enter, 10, 4),
            PlanNav::None
        );
        assert_eq!(
            plan_detail_nav(doc, &actions, Key::Escape, 10, 4),
            PlanNav::Back,
            "esc still leaves from the document"
        );
        // A scrolled document never hijacks ↑ from a button.
        assert_eq!(
            plan_detail_nav(1, &actions, Key::Up, 10, 5),
            PlanNav::Sel(0)
        );
        assert_eq!(
            plan_detail_nav(last, &actions, Key::Up, 10, 5),
            PlanNav::Sel(last - 1),
            "the last button is a button, not the document"
        );
    }

    /// The refresh carries the cursor by identity. A plan finishing
    /// is the shape change that matters: `Stash` and `ForceFinish`
    /// leave, `Squash` may arrive, and every index after them shifts.
    #[test]
    fn plan_sel_rebinds_by_action_across_a_finish() {
        use PlanAction::*;
        let active = plan_actions(active_st());
        let finished = plan_actions(PlanPageState {
            finished: true,
            multi_commit: true,
            repo_paused: false,
        });
        assert_eq!(active, vec![OpenHtml, Stash, ForceFinish, Purge, Back]);
        assert_eq!(finished, vec![OpenHtml, Squash, Purge, Back]);
        let at = |list: &[PlanAction], a: PlanAction| list.iter().position(|x| *x == a).unwrap();

        // Retained actions keep their identity at the new index.
        for a in [OpenHtml, Purge, Back] {
            assert_eq!(
                rebind_plan_sel(&active, at(&active, a), &finished),
                at(&finished, a),
                "{a:?} stays {a:?}"
            );
        }
        // A clamp would have put ForceFinish (2) on Purge (2): the
        // destructive retargeting this exists to prevent.
        assert_eq!(
            rebind_plan_sel(&active, at(&active, ForceFinish), &finished),
            document_focus(&finished),
            "a vanished action falls back to the document, where Enter does nothing"
        );
        assert_eq!(
            rebind_plan_sel(&active, at(&active, Stash), &finished),
            document_focus(&finished)
        );
        // Document focus survives both shrink and growth, at the new
        // length each way.
        assert_eq!(
            rebind_plan_sel(&active, document_focus(&active), &finished),
            document_focus(&finished)
        );
        assert_eq!(
            rebind_plan_sel(&finished, document_focus(&finished), &active),
            document_focus(&active)
        );
        // An index past the document (never produced, but a refresh
        // must not trust one) lands on the document too.
        assert_eq!(
            rebind_plan_sel(&active, 99, &finished),
            document_focus(&finished)
        );
    }

    #[test]
    fn purge_choice_nav_routes_both_choices_and_escape() {
        assert_eq!(
            purge_choice_nav(0, Key::Char(b'a')),
            PlanNavPurge::Choose(PurgeChoice::Artifacts)
        );
        assert_eq!(
            purge_choice_nav(0, Key::Char(b'd')),
            PlanNavPurge::Choose(PurgeChoice::Drop)
        );
        assert_eq!(
            purge_choice_nav(1, Key::Enter),
            PlanNavPurge::Choose(PurgeChoice::Drop),
            "enter follows the cursor"
        );
        assert_eq!(
            purge_choice_nav(0, Key::Escape),
            PlanNavPurge::Choose(PurgeChoice::Back)
        );
    }

    #[test]
    fn plan_page_confirms_default_no_and_return_to_the_page() {
        for a in [
            ConfirmAction::StashPlan,
            ConfirmAction::PurgeArtifacts,
            ConfirmAction::PurgeDrop,
        ] {
            assert!(!a.default_yes(), "{a:?} must never Enter-confirm");
            assert!(a.is_plan_page());
        }
    }

    // ── tui-plan-actions-page M3: text input ──

    #[test]
    fn text_mode_reads_bound_letters_as_text() {
        // The whole reason parsing is mode-scoped: j/k/y/n/o/q are
        // commands in normal modes but LETTERS in an input.
        let bytes = b"jkyq nob";
        let mut i = 0;
        let mut typed = String::new();
        while i < bytes.len() {
            let (k, used) = parse_one(&bytes[i..], true).unwrap();
            if let AnyKey::Text(TextKey::Char(c)) = k {
                typed.push(c as char);
            }
            i += used;
        }
        assert_eq!(typed, "jkyq nob");
        // And the same bytes in command mode are commands, not text.
        assert!(matches!(
            parse_one(b"j", false),
            Some((AnyKey::Cmd(Key::Down), 1))
        ));
    }

    #[test]
    fn text_mode_arrows_move_and_updown_is_swallowed() {
        assert!(matches!(
            parse_one(b"\x1b[D", true),
            Some((AnyKey::Text(TextKey::Left), 3))
        ));
        assert!(matches!(
            parse_one(b"\x1b[C", true),
            Some((AnyKey::Text(TextKey::Right), 3))
        ));
        assert!(matches!(
            parse_one(b"\x1b[A", true),
            Some((AnyKey::Text(TextKey::Ignore), 3)),
        ));
        assert!(matches!(
            parse_one(b"\x1b", true),
            Some((AnyKey::Text(TextKey::Esc), 1))
        ));
    }

    #[test]
    fn text_input_edits_at_the_cursor() {
        let mut ti = TextInput::default();
        for c in b"helo" {
            text_input_nav(&mut ti, TextKey::Char(*c));
        }
        // Fix the typo: ← ← insert l
        text_input_nav(&mut ti, TextKey::Left);
        text_input_nav(&mut ti, TextKey::Left);
        text_input_nav(&mut ti, TextKey::Char(b'l'));
        assert_eq!(ti.buf, "helllo".replace("lll", "ll"), "insert at cursor");
        assert_eq!(ti.buf, "hello");
        // Backspace removes BEFORE the cursor.
        text_input_nav(&mut ti, TextKey::Backspace);
        assert_eq!(ti.buf, "helo");
        // Right clamps at the end; Left at 0.
        for _ in 0..20 {
            text_input_nav(&mut ti, TextKey::Right);
        }
        assert_eq!(ti.cursor, ti.buf.len());
        for _ in 0..20 {
            text_input_nav(&mut ti, TextKey::Left);
        }
        assert_eq!(ti.cursor, 0);
        text_input_nav(&mut ti, TextKey::Backspace); // no-op at 0
        assert_eq!(ti.buf, "helo");
        assert_eq!(text_input_nav(&mut ti, TextKey::Enter), InputNav::Submit);
        assert_eq!(text_input_nav(&mut ti, TextKey::Esc), InputNav::Cancel);
    }

    #[test]
    fn composed_squash_message_passes_finish_validation() {
        use crate::cli::finish::validate_finish_message as validate;
        // The raw one-line input — the shipped bug — is REJECTED.
        assert!(
            validate(Some("collapse it all"), "my-plan").is_err(),
            "subject-only must fail (tui-squash-message-body repro)"
        );
        // Typed subject + the plan's real finalize body passes.
        let real = compose_squash_message(
            "collapse it all",
            "the plan landed in five steps; one commit reads better",
            TUI_SQUASH_PROVENANCE,
        );
        validate(Some(&real), "my-plan").expect("subject + finalize body");
        assert!(real.contains("\n\nthe plan landed"));
        // Legacy plan (no finalize body) → provenance fallback passes.
        let legacy = compose_squash_message("collapse it all", "  ", TUI_SQUASH_PROVENANCE);
        validate(Some(&legacy), "my-plan").expect("provenance fallback");
        assert!(legacy.contains("predates mandatory finish messages"));
    }

    #[test]
    fn drop_arming_rejects_whitespace_padding() {
        // codex 0daf087 regression: submit used a TRIMMED compare while
        // the indicator used the raw buffer, so ` <stem> ` executed a
        // drop the screen called not armed. The predicate is raw-exact
        // and shared by both.
        assert!(drop_armed("my-plan", "my-plan"));
        assert!(!drop_armed(" my-plan", "my-plan"));
        assert!(!drop_armed("my-plan ", "my-plan"));
        assert!(!drop_armed(" my-plan ", "my-plan"));
        assert!(!drop_armed("My-plan", "my-plan"), "no case folding");
        assert!(!drop_armed("my-pla", "my-plan"));
        // The trap the trimmed compare fell into:
        assert_eq!(" my-plan ".trim(), "my-plan", "trim WOULD have matched");
    }

    #[test]
    fn prefilled_input_starts_with_cursor_at_the_end() {
        let ti = TextInput::prefilled("subject line");
        assert_eq!(ti.buf, "subject line");
        assert_eq!(ti.cursor, ti.buf.len());
    }

    #[test]
    fn parse_keys_recognizes_panel_focus_keys() {
        use Key::*;
        // Tab and `a` are now DISTINCT at the parse — both still act
        // as focus, but each site opts in, so a page can bind `a`
        // without Tab performing it (tui-event-page-hotkeys).
        let got: Vec<_> = parse_keys(b"\ta\x1b")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want: Vec<_> = [Focus, Char(b'a'), Escape]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(got, want, "tab focuses, `a` stays a letter, esc leaves");
        // Left arrow (← / CSI D) is its own key (the commit-detail back
        // key) — consumed whole, NOT misread as a lone Esc from the CSI's
        // leading bytes.
        assert_eq!(
            parse_keys(b"\x1b[D")
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>(),
            vec![std::mem::discriminant(&Key::Left)],
            "left arrow → one Key::Left, no stray Escape"
        );
        // Right arrow (→ / CSI C) is its own key (cycles a detail toggle),
        // consumed whole like the left arrow.
        assert_eq!(
            parse_keys(b"\x1b[C")
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>(),
            vec![std::mem::discriminant(&Key::Right)],
            "right arrow → one Key::Right, no stray Escape"
        );
    }

    #[test]
    fn flip_auto_inverts() {
        use clank_core::vocab::AutoMode;
        assert_eq!(flip_auto(AutoMode::On), AutoMode::Off);
        assert_eq!(flip_auto(AutoMode::Off), AutoMode::On);
    }

    #[test]
    fn toggle_focus_enters_panel_only_with_a_roster() {
        // From the log: enter the panel at row 0 — but only if there are
        // agents to focus; an empty roster stays on the log.
        assert_eq!(Mode::LogScroll.toggle_focus(2), Mode::AgentPanel { sel: 0 });
        assert_eq!(Mode::LogScroll.toggle_focus(0), Mode::LogScroll);
        // From any agent-region mode (panel, picker, confirm): back to log.
        assert_eq!(Mode::AgentPanel { sel: 1 }.toggle_focus(2), Mode::LogScroll);
        assert_eq!(
            Mode::AddPicker {
                sel: 0,
                tier: crate::cli::teams_config::ReviewKind::Commit
            }
            .toggle_focus(2),
            Mode::LogScroll
        );
        assert_eq!(
            Mode::Confirm {
                action: ConfirmAction::RemoveAgent { idx: 0 }
            }
            .toggle_focus(2),
            Mode::LogScroll
        );
    }

    #[test]
    fn move_selection_saturates_at_both_ends() {
        assert_eq!(move_selection(0, 3, false), 0, "up at top stays put");
        assert_eq!(move_selection(0, 3, true), 1, "down advances");
        assert_eq!(move_selection(2, 3, true), 2, "down at bottom stays put");
        assert_eq!(move_selection(2, 3, false), 1, "up retreats");
        assert_eq!(move_selection(0, 0, true), 0, "empty roster pins at 0");
    }

    #[test]
    fn detail_actions_are_reduced_for_master() {
        use crate::cli::teams_config::RosterRole;
        use DetailAction::*;
        assert_eq!(
            detail_actions(RosterRole::Master, Presence::Live),
            vec![ToggleAuto, Swap, Back]
        );
        let reviewer = vec![
            ToggleAuto,
            TierCommit,
            TierPlan,
            TierFinal,
            PromoteToMaster,
            Swap,
            Remove,
            Back,
        ];
        assert_eq!(detail_actions(RosterRole::Commit, Presence::Live), reviewer);
        assert_eq!(detail_actions(RosterRole::Gate, Presence::Live), reviewer);
    }

    /// Reopen is offered to EVERY role when the pane is missing —
    /// master included, a closed master pane is the one the
    /// reconciler stages — and to none when it is live or when nothing
    /// is known (not in zellij: nothing to reopen into)
    /// (the-tui-knows-whether-a-pane-is-open).
    #[test]
    fn the_detail_cursor_follows_its_action_when_reopen_comes_and_goes() {
        use crate::cli::teams_config::RosterRole;
        use DetailAction::*;
        let missing = detail_actions(RosterRole::Commit, Presence::Missing);
        let live = detail_actions(RosterRole::Commit, Presence::Live);
        // Reopen selected, the pane comes back: the row is gone, and
        // the cursor must NOT land on remove, which now sits at that
        // index. It lands on Back.
        let on_reopen = missing.iter().position(|a| *a == Reopen).unwrap();
        assert_eq!(
            live[on_reopen], Remove,
            "the index alone would select remove"
        );
        let rebound = rebind_detail_sel(&missing, on_reopen, &live);
        assert_eq!(live[rebound], Back);
        // Remove selected on the live list, the pane goes missing: the
        // cursor stays on remove, one row down.
        let on_remove = live.iter().position(|a| *a == Remove).unwrap();
        assert_eq!(
            missing[rebind_detail_sel(&live, on_remove, &missing)],
            Remove
        );
        // A cursor past the end of the old list lands on Back.
        assert_eq!(live[rebind_detail_sel(&missing, 99, &live)], Back);
    }

    #[test]
    fn a_worker_wake_rebinds_the_cursor_before_the_frame_paints() {
        // The sequence a hand-close then a reopen produces with no key
        // pressed: the frame that first shows the pane live must carry
        // a cursor that was on reopen to Back, not paint it on remove
        // (codex on 44e937e).
        use crate::cli::teams_config::RosterRole;
        use DetailAction::*;
        let agents = vec![crate::cli::status::AgentAutoRow {
            label: "codex".into(),
            role: RosterRole::Commit,
            auto_mode: clank_core::vocab::AutoMode::On,
            tool: "codex".into(),
            invocation: "codex".into(),
            session: None,
            attending: None,
        }];
        let mut shown = None;
        // Frame 1: entering the page from the panel, cursor on the
        // first row. Nothing was painted before, so the requested
        // cursor stands — rebinding from nothing sent it to Back
        // (codex on 18b70d3).
        let missing_set = Some(std::collections::BTreeSet::new());
        let mode = settle_detail_cursor(
            Mode::AgentDetail { idx: 0, sel: 0 },
            &agents,
            &missing_set,
            &mut shown,
        );
        assert_eq!(
            mode,
            Mode::AgentDetail { idx: 0, sel: 0 },
            "first entry keeps its cursor"
        );
        let list = shown.clone().expect("the page was painted");
        assert!(list.contains(&Reopen));
        let on_reopen = list.iter().position(|a| *a == Reopen).unwrap();
        let Mode::AgentDetail { idx, .. } = mode else {
            panic!()
        };
        // The operator moves onto reopen (a key), then a worker wake
        // reports the pane live — no key between that and the paint.
        let live_set = Some(["codex".to_string()].into_iter().collect());
        let painted = settle_detail_cursor(
            Mode::AgentDetail {
                idx,
                sel: on_reopen,
            },
            &agents,
            &live_set,
            &mut shown,
        );
        let Mode::AgentDetail { sel, .. } = painted else {
            panic!()
        };
        let list = shown.clone().expect("painted");
        assert!(!list.contains(&Reopen), "the frame shows the live list");
        assert_eq!(list[sel], Back, "not remove, which now holds that index");
        // Leaving the page forgets the list; coming back is a first
        // entry again.
        settle_detail_cursor(Mode::AgentPanel { sel: 0 }, &agents, &live_set, &mut shown);
        assert!(shown.is_none());
        let back_in = settle_detail_cursor(
            Mode::AgentDetail { idx: 0, sel: 0 },
            &agents,
            &live_set,
            &mut shown,
        );
        assert_eq!(back_in, Mode::AgentDetail { idx: 0, sel: 0 });
    }

    #[test]
    fn reopen_is_offered_exactly_when_the_pane_is_missing() {
        use crate::cli::teams_config::RosterRole;
        for role in [
            RosterRole::Master,
            RosterRole::Commit,
            RosterRole::Plan,
            RosterRole::Final,
            RosterRole::Gate,
        ] {
            let missing = detail_actions(role, Presence::Missing);
            assert!(missing.contains(&DetailAction::Reopen), "{role:?}");
            assert_eq!(
                missing.last(),
                Some(&DetailAction::Back),
                "{role:?}: back stays last"
            );
            if role != RosterRole::Master {
                let reopen = missing.iter().position(|a| *a == DetailAction::Reopen);
                let remove = missing.iter().position(|a| *a == DetailAction::Remove);
                assert!(
                    reopen < remove,
                    "{role:?}: reopen before the destructive row"
                );
            }
            for presence in [Presence::Live, Presence::Unknown] {
                assert!(
                    !detail_actions(role, presence).contains(&DetailAction::Reopen),
                    "{role:?} {presence:?}"
                );
            }
        }
    }

    /// Swap is a roster replacement action for every role. Promotion
    /// remains the separate operation for making an existing reviewer
    /// master while keeping the outgoing master on the roster.
    #[test]
    fn swap_is_offered_to_every_roster_role() {
        use crate::cli::teams_config::RosterRole;
        for tier in [
            RosterRole::Commit,
            RosterRole::Plan,
            RosterRole::Final,
            RosterRole::Gate,
        ] {
            assert!(
                detail_actions(tier, Presence::Live).contains(&DetailAction::Swap),
                "{tier:?} must be swappable"
            );
        }
        assert!(detail_actions(RosterRole::Master, Presence::Live).contains(&DetailAction::Swap));
    }

    /// ←/→ walk the review pipeline and wrap, in both directions.
    #[test]
    fn tier_cycle_walks_the_pipeline_and_wraps() {
        use crate::cli::teams_config::ReviewKind::*;
        let mut t = Commit;
        for want in [Plan, Final, Gate, Commit] {
            t = tier_cycle(t, true);
            assert_eq!(t, want, "forward");
        }
        for want in [Gate, Final, Plan, Commit] {
            t = tier_cycle(t, false);
            assert_eq!(t, want, "backward");
        }
    }

    #[test]
    fn tier_toggle_state_machine_covers_all_transitions() {
        use crate::cli::teams_config::ReviewKind::*;
        use DetailAction::*;
        // commit is exclusive; ticking plan/final leaves commit mode.
        assert_eq!(tier_after_toggle(Commit, TierCommit), None, "last coverage");
        assert_eq!(tier_after_toggle(Commit, TierPlan), Some(Plan));
        assert_eq!(tier_after_toggle(Commit, TierFinal), Some(Final));
        // plan-only.
        assert_eq!(tier_after_toggle(Plan, TierCommit), Some(Commit));
        assert_eq!(tier_after_toggle(Plan, TierPlan), None, "last coverage");
        assert_eq!(tier_after_toggle(Plan, TierFinal), Some(Gate));
        // final-only.
        assert_eq!(tier_after_toggle(Final, TierCommit), Some(Commit));
        assert_eq!(tier_after_toggle(Final, TierPlan), Some(Gate));
        assert_eq!(tier_after_toggle(Final, TierFinal), None, "last coverage");
        // gate == plan+final; unticking one leaves the other.
        assert_eq!(tier_after_toggle(Gate, TierCommit), Some(Commit));
        assert_eq!(tier_after_toggle(Gate, TierPlan), Some(Final));
        assert_eq!(tier_after_toggle(Gate, TierFinal), Some(Plan));
    }

    #[test]
    fn agent_detail_nav_routes_menu_keys() {
        use DetailAction::*;
        let actions = [ToggleAuto, TierCommit, Remove, Back];
        assert_eq!(
            agent_detail_nav(0, &actions, Key::Down),
            DetailNav::MoveCursor(1)
        );
        assert_eq!(
            agent_detail_nav(0, &actions, Key::Up),
            DetailNav::MoveCursor(0),
            "up at the top stays"
        );
        assert_eq!(
            agent_detail_nav(1, &actions, Key::Enter),
            DetailNav::Activate(TierCommit)
        );
        assert_eq!(agent_detail_nav(0, &actions, Key::Escape), DetailNav::Back);
        assert_eq!(agent_detail_nav(0, &actions, Key::Quit), DetailNav::Quit);

        // ←/→/␣ flip a TOGGLE row in place...
        for key in [Key::Left, Key::Right, Key::Space] {
            assert_eq!(
                agent_detail_nav(0, &actions, key),
                DetailNav::Activate(ToggleAuto),
                "{key:?} cycles the toggle row"
            );
        }
        // ...but the "change" keys (←/→/␣) must NEVER fire an action row
        // (the destructive guard) — they're a no-op on Remove; only Enter
        // "selects" (activates) it, which routes to the confirm modal.
        assert_eq!(agent_detail_nav(2, &actions, Key::Left), DetailNav::None);
        assert_eq!(agent_detail_nav(2, &actions, Key::Right), DetailNav::None);
        assert_eq!(agent_detail_nav(2, &actions, Key::Space), DetailNav::None);
        assert_eq!(
            agent_detail_nav(2, &actions, Key::Enter),
            DetailNav::Activate(Remove)
        );
    }

    #[test]
    fn confirm_decision_q_cancels_and_enter_follows_default() {
        let rm = ConfirmAction::RemoveAgent { idx: 0 };
        let add = ConfirmAction::AddCandidate {
            idx: 0,
            tier: crate::cli::teams_config::ReviewKind::Commit,
        };
        // In a confirm, q CANCELS — it must not quit the TUI.
        assert_eq!(confirm_decision(rm, Key::Quit), Some(false));
        assert_eq!(confirm_decision(rm, Key::Escape), Some(false));
        assert_eq!(confirm_decision(rm, Key::No), Some(false));
        assert_eq!(confirm_decision(rm, Key::Yes), Some(true));
        // Enter follows the default: remove cancels, add confirms.
        assert_eq!(confirm_decision(rm, Key::Enter), Some(false));
        assert_eq!(confirm_decision(add, Key::Enter), Some(true));
        assert_eq!(confirm_decision(rm, Key::Up), None);
    }

    #[test]
    fn confirm_action_default_is_safe_for_remove() {
        assert!(
            !ConfirmAction::RemoveAgent { idx: 0 }.default_yes(),
            "a destructive remove defaults to No"
        );
        assert!(
            ConfirmAction::AddCandidate {
                idx: 0,
                tier: crate::cli::teams_config::ReviewKind::Commit
            }
            .default_yes(),
            "a non-destructive add defaults to Yes"
        );
    }

    #[test]
    fn detail_page_relocates_by_label_across_a_reorder() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        // Detail is open on "codex", currently at index 1.
        let before = [
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        // After an external promote, codex moved to index 0 — relocating by
        // label finds it there, NOT whatever now sits at index 1.
        let after = [
            agent_row("codex", RosterRole::Master, AutoMode::Off),
            agent_row("claude", RosterRole::Commit, AutoMode::On),
            agent_row("ruthless", RosterRole::Gate, AutoMode::On),
        ];
        let label = before[1].label.clone();
        assert_eq!(relocate_detail(&label, &before), Some(1));
        assert_eq!(
            relocate_detail(&label, &after),
            Some(0),
            "follows the agent by label across a reorder"
        );
        // Removed entirely → None (the page should close).
        let removed = [agent_row("claude", RosterRole::Master, AutoMode::On)];
        assert_eq!(relocate_detail(&label, &removed), None);
    }

    /// A wait is a cursor position of its own, sitting between its
    /// agent and whatever follows. Positions are ENUMERATED rather
    /// than derived, which is what stops a new row kind shifting every
    /// segment after it — the queue rows used to be drawn at the stash
    /// rows' offsets for exactly that reason.
    #[test]
    fn a_drawable_wait_is_a_cursor_position_between_its_agent_and_the_next() {
        let rows = panel_rows(2, &[0], 1, 2);
        assert_eq!(
            rows,
            vec![
                PanelRow::Agent(0),
                PanelRow::Wait(0),
                PanelRow::Agent(1),
                PanelRow::Add,
                PanelRow::Stash(0),
                PanelRow::Queue(0),
                PanelRow::Queue(1),
            ]
        );
        assert_eq!(
            add_row_index(&rows),
            3,
            "the add row moved down by the wait"
        );

        // Stash and queue keep DISTINCT positions — the arithmetic
        // version gave them the same ones.
        assert_ne!(
            rows.iter().position(|r| *r == PanelRow::Stash(0)),
            rows.iter().position(|r| *r == PanelRow::Queue(0))
        );
    }

    /// A wait the pane cannot draw is not a place the cursor may go:
    /// landing on an invisible row is worse than not reaching it.
    #[test]
    fn an_undrawable_wait_is_not_a_cursor_position() {
        assert_eq!(
            panel_rows(2, &[], 0, 0),
            vec![PanelRow::Agent(0), PanelRow::Agent(1), PanelRow::Add]
        );
    }

    /// Enter opens the WAIT, not the agent it hangs under — the two
    /// rows are adjacent and an off-by-one here opens the wrong page.
    #[test]
    fn enter_on_a_wait_row_opens_the_wait_not_its_agent() {
        let rows = panel_rows(2, &[1], 0, 0);
        // [Agent(0), Agent(1), Wait(1), Add]
        assert_eq!(
            agent_panel_action(1, &rows, Key::Enter),
            PanelAction::OpenDetail(1)
        );
        assert_eq!(
            agent_panel_action(2, &rows, Key::Enter),
            PanelAction::OpenWait(1)
        );
        // And Space, the agent's inline auto-toggle, belongs to no
        // other row kind — a wait has no auto mode to flip.
        assert_eq!(
            agent_panel_action(1, &rows, Key::Space),
            PanelAction::ToggleAuto(1)
        );
        assert_eq!(agent_panel_action(2, &rows, Key::Space), PanelAction::None);
    }

    /// The agent index an action carries is the AGENT's, not the
    /// cursor's — they diverge as soon as a wait sits above.
    #[test]
    fn actions_carry_the_agent_index_not_the_cursor_position() {
        let rows = panel_rows(3, &[0], 0, 0);
        // [Agent(0), Wait(0), Agent(1), Agent(2), Add]
        assert_eq!(
            agent_panel_action(3, &rows, Key::Enter),
            PanelAction::OpenDetail(2),
            "cursor 3 is agent 2"
        );
    }

    /// The kill row is offered only when the process can be
    /// identified; otherwise it is ABSENT, and the page explains
    /// rather than showing a control that cannot work.
    #[test]
    fn the_kill_row_is_absent_when_the_process_cannot_be_identified() {
        assert_eq!(wait_actions(true), vec![WaitAction::Kill, WaitAction::Back]);
        assert_eq!(wait_actions(false), vec![WaitAction::Back]);
        // Enter on the only row of a non-killable page backs out; it
        // can never reach the kill.
        assert_eq!(
            wait_page_nav(0, &wait_actions(false), Key::Enter),
            DetailNav::Back
        );
        assert_eq!(
            wait_page_nav(0, &wait_actions(true), Key::Enter),
            DetailNav::ActivateKill
        );
    }

    /// A destructive default is never confirmed by Enter.
    #[test]
    fn the_kill_confirm_does_not_default_to_yes() {
        assert!(!ConfirmAction::KillAttended.default_yes());
        assert_eq!(
            confirm_decision(ConfirmAction::KillAttended, Key::Enter),
            Some(false)
        );
        assert_eq!(
            confirm_decision(ConfirmAction::KillAttended, Key::Escape),
            Some(false)
        );
    }

    #[test]
    fn agent_panel_action_routes_keys_by_row_and_role() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
        ];
        // "+ add" row is index 2 (== agents.len()): Enter AND Space open
        // the picker.
        assert_eq!(
            agent_panel_action(2, &panel_rows(agents.len(), &[], 0, 0), Key::Enter),
            PanelAction::OpenPicker
        );
        assert_eq!(
            agent_panel_action(2, &panel_rows(agents.len(), &[], 0, 0), Key::Space),
            PanelAction::OpenPicker
        );
        // Agent rows: Enter opens the detail page; Space toggles auto.
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, 0), Key::Enter),
            PanelAction::OpenDetail(1)
        );
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, 0), Key::Space),
            PanelAction::ToggleAuto(1)
        );
        // DEL is no longer a panel action (removal lives on the detail page).
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, 0), Key::Delete),
            PanelAction::None
        );
        // Navigation: Down within the panel moves; Down at the +add row
        // (index 2 == agents.len()) crosses into the log; Tab/Esc leave;
        // q quits.
        assert_eq!(
            agent_panel_action(0, &panel_rows(agents.len(), &[], 0, 0), Key::Down),
            PanelAction::MoveCursor(1)
        );
        assert_eq!(
            agent_panel_action(2, &panel_rows(agents.len(), &[], 0, 0), Key::Down),
            PanelAction::EnterLog,
            "Down past +add flows into the log"
        );
        assert_eq!(
            agent_panel_action(0, &panel_rows(agents.len(), &[], 0, 0), Key::Up),
            PanelAction::MoveCursor(0),
            "Up at the top stays put"
        );
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, 0), Key::Focus),
            PanelAction::LeaveFocus
        );
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, 0), Key::Quit),
            PanelAction::Quit
        );
    }

    #[test]
    fn agent_panel_action_routes_queue_rows() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
        ];
        // Rows: 0-1 agents, 2 "+ add", 3-4 the two queue rows.
        let ql = 2;
        // Enter on a queue row reads it; `o` opens its HTML page.
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], 0, ql), Key::Enter),
            PanelAction::OpenQueueItem(0)
        );
        assert_eq!(
            agent_panel_action(4, &panel_rows(agents.len(), &[], 0, ql), Key::Html),
            PanelAction::OpenQueueHtml(1)
        );
        // +/- nudge the priority number by 50 (loop clamps + persists via
        // queue::set_priority).
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], 0, ql), Key::Plus),
            PanelAction::NudgeQueue { idx: 0, delta: 50 }
        );
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], 0, ql), Key::Minus),
            PanelAction::NudgeQueue { idx: 0, delta: -50 }
        );
        // Space on a queue row is inert (no accidental overlay); `o` on an
        // agent row is inert too.
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], 0, ql), Key::Space),
            PanelAction::None
        );
        assert_eq!(
            agent_panel_action(1, &panel_rows(agents.len(), &[], 0, ql), Key::Html),
            PanelAction::None
        );
        // Down from "+ add" now enters the queue, not the log; Down from
        // the LAST queue row crosses into the log.
        assert_eq!(
            agent_panel_action(2, &panel_rows(agents.len(), &[], 0, ql), Key::Down),
            PanelAction::MoveCursor(3)
        );
        assert_eq!(
            agent_panel_action(4, &panel_rows(agents.len(), &[], 0, ql), Key::Down),
            PanelAction::EnterLog
        );
        // With an empty queue, Down from "+ add" still enters the log.
        assert_eq!(
            agent_panel_action(2, &panel_rows(agents.len(), &[], 0, 0), Key::Down),
            PanelAction::EnterLog
        );
    }

    #[test]
    fn agent_panel_action_routes_stash_rows_before_queue_rows() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let agents = vec![
            agent_row("claude", RosterRole::Master, AutoMode::On),
            agent_row("codex", RosterRole::Commit, AutoMode::Off),
        ];
        // Rows: 0-1 agents, 2 "+ add", 3 the stash row, 4 the queue row.
        let (sl, ql) = (1, 1);
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], sl, ql), Key::Enter),
            PanelAction::OpenStashItem(0)
        );
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], sl, ql), Key::Html),
            PanelAction::OpenStashHtml(0)
        );
        // The queue row sits AFTER the stash segment.
        assert_eq!(
            agent_panel_action(4, &panel_rows(agents.len(), &[], sl, ql), Key::Enter),
            PanelAction::OpenQueueItem(0)
        );
        // +/- are queue-only; inert on a stash row.
        assert_eq!(
            agent_panel_action(3, &panel_rows(agents.len(), &[], sl, ql), Key::Plus),
            PanelAction::None
        );
        // Down from the LAST row (the queue row) crosses into the log.
        assert_eq!(
            agent_panel_action(4, &panel_rows(agents.len(), &[], sl, ql), Key::Down),
            PanelAction::EnterLog
        );
    }

    /// Agent rows, with an attendance on each index in `attending`.
    fn rows_for(labels: &[&str], attending: &[usize]) -> Vec<crate::cli::status::AgentAutoRow> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| crate::cli::status::AgentAutoRow {
                label: l.to_string(),
                role: crate::cli::teams_config::RosterRole::Commit,
                auto_mode: clank_core::vocab::AutoMode::On,
                tool: "claude".to_string(),
                invocation: "claude".to_string(),
                session: None,
                attending: attending
                    .contains(&i)
                    .then(|| crate::cli::stop_hook::Attended {
                        expect: None,
                        task: format!("t{i}"),
                        desc: None,
                        pid: None,
                        token: None,
                        at: "2026-08-20T14:51:09Z".to_string(),
                    }),
            })
            .collect()
    }

    /// A replacement attendance under the same agent is a DIFFERENT
    /// wait. The cursor falls back to the agent rather than landing on
    /// something the user never selected — the anchor carries the full
    /// instance for exactly this.
    #[test]
    fn a_replaced_attendance_does_not_inherit_the_wait_cursor() {
        let before = rows_for(&["claude", "codex"], &[1]);
        let rows = panel_rows(2, &[1], 0, 0); // [A0, A1, W1, Add]
        let anchor = capture_anchor(2, &rows, &before, &[], &[]);

        // Same agent, still attending — but a new instance.
        let mut after = rows_for(&["claude", "codex"], &[1]);
        after[1].attending.as_mut().unwrap().at = "2026-08-24T09:00:00Z".to_string();
        assert_eq!(
            rebind_anchor(&anchor, &rows, &after, &[], &[]),
            1,
            "falls back to its agent, not onto the replacement"
        );

        // Unchanged instance keeps the wait row.
        assert_eq!(rebind_anchor(&anchor, &rows, &before, &[], &[]), 2);
    }

    /// A refresh moves rows. The cursor follows what it was ON, by
    /// identity — the old arithmetic could not see a wait row at all,
    /// so a wait appearing above a stash or queue selection silently
    /// retargeted it.
    #[test]
    fn the_cursor_follows_its_row_across_a_refresh_that_moves_everything() {
        let labels = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let agents = rows_for(&["claude", "codex"], &[]);
        let stash = labels(&["s1"]);
        let queue = labels(&["q1", "q2"]);

        // No waits: [A0, A1, Add, S0, Q0, Q1]
        let before = panel_rows(2, &[], 1, 2);
        let anchor = capture_anchor(4, &before, &agents, &stash, &queue);
        assert_eq!(anchor, PanelAnchor::Queue("q1".to_string()));

        // A wait appears above everything: [A0, W0, A1, Add, S0, Q0, Q1]
        let after = panel_rows(2, &[0], 1, 2);
        assert_eq!(
            rebind_anchor(&anchor, &after, &agents, &stash, &queue),
            5,
            "the queue row moved down by the wait; the cursor moved with it"
        );

        // The stash row too.
        let anchor = capture_anchor(3, &before, &agents, &stash, &queue);
        assert_eq!(anchor, PanelAnchor::Stash("s1".to_string()));
        assert_eq!(rebind_anchor(&anchor, &after, &agents, &stash, &queue), 4);

        // And an agent below the new wait.
        let anchor = capture_anchor(1, &before, &agents, &stash, &queue);
        assert_eq!(anchor, PanelAnchor::Agent("codex".to_string()));
        assert_eq!(rebind_anchor(&anchor, &after, &agents, &stash, &queue), 2);
    }

    /// A queue item that is renamed or re-sorted under the cursor is
    /// still followed — the refresh is often self-inflicted by a
    /// reprioritise.
    #[test]
    fn a_reordered_queue_item_keeps_the_cursor() {
        let agents = rows_for(&["claude", "codex"], &[]);
        let queue = vec!["a".to_string(), "b".to_string()];
        let rows = panel_rows(2, &[], 0, 2);
        let anchor = capture_anchor(4, &rows, &agents, &[], &queue);
        assert_eq!(anchor, PanelAnchor::Queue("b".to_string()));

        let resorted = vec!["b".to_string(), "a".to_string()];
        assert_eq!(
            rebind_anchor(&anchor, &rows, &agents, &[], &resorted),
            3,
            "follows the item, not the slot"
        );

        // Gone entirely → the add row.
        let without = vec!["a".to_string()];
        let shorter = panel_rows(2, &[], 0, 1);
        assert_eq!(
            rebind_anchor(&anchor, &shorter, &agents, &[], &without),
            add_row_index(&shorter)
        );
    }

    /// A wait that stops being drawable drops to the AGENT it hung
    /// under, not to "+ add": the agent is still there, and it is
    /// where the user was already looking.
    #[test]
    fn a_vanished_wait_falls_back_to_its_own_agent() {
        let agents = rows_for(&["claude", "codex"], &[1]);
        let with = panel_rows(2, &[1], 0, 0); // [A0, A1, W1, Add]
        let anchor = capture_anchor(2, &with, &agents, &[], &[]);
        assert_eq!(
            anchor,
            PanelAnchor::Wait {
                label: "codex".to_string(),
                task: "t1".to_string(),
                at: "2026-08-20T14:51:09Z".to_string(),
            }
        );

        let without = panel_rows(2, &[], 0, 0); // [A0, A1, Add]
        let no_wait = rows_for(&["claude", "codex"], &[]);
        assert_eq!(
            rebind_anchor(&anchor, &without, &no_wait, &[], &[]),
            1,
            "its agent, not the add row"
        );

        // The agent left too → the add row.
        let solo = rows_for(&["claude"], &[]);
        let rows = panel_rows(1, &[], 0, 0);
        assert_eq!(
            rebind_anchor(&anchor, &rows, &solo, &[], &[]),
            add_row_index(&rows)
        );
    }

    #[test]
    fn doc_nav_scrolls_or_backs_out() {
        assert_eq!(doc_nav(Key::Down, 10), DocNav::Scroll(1));
        assert_eq!(doc_nav(Key::Up, 10), DocNav::Scroll(-1));
        assert_eq!(doc_nav(Key::PageDown, 10), DocNav::Scroll(10));
        assert_eq!(doc_nav(Key::Space, 10), DocNav::Scroll(10));
        assert_eq!(doc_nav(Key::PageUp, 10), DocNav::Scroll(-10));
        // Read-only overlay: Esc / Enter / q / ← all dismiss to the log.
        assert_eq!(doc_nav(Key::Escape, 10), DocNav::Back);
        assert_eq!(doc_nav(Key::Enter, 10), DocNav::Back);
        assert_eq!(doc_nav(Key::Quit, 10), DocNav::Back);
        assert_eq!(doc_nav(Key::Left, 10), DocNav::Back);
        // `o` opens the overlay's page in the browser.
        assert_eq!(doc_nav(Key::Html, 10), DocNav::OpenHtml);
        // Unmapped keys do nothing.
        assert_eq!(doc_nav(Key::Yes, 10), DocNav::None);
    }

    #[test]
    fn parse_keys_maps_o_to_html() {
        assert_eq!(parse_keys(b"o"), vec![Key::Html]);
    }
}
