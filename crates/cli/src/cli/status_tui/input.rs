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
    /// Tab / `a` — move keyboard focus between the log and the agent
    /// panel.
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
    AddPicker { sel: usize },
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
    /// The reason for a pause block (the block's question text).
    BlockReason,
    /// Type-the-stem arming for `purge --drop` — the scariest screen;
    /// submit is a no-op until the buffer equals the stem exactly.
    DropStem,
}

/// One row of an agent's detail-page action menu (the actions are data
/// the cursor moves over, not a keymap). Availability depends on role —
/// see [`detail_actions`].
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
    /// Remove the agent from the team (behind the confirm).
    Remove,
    /// Leave the detail page.
    Back,
}

/// The detail-page actions for `role`, in display order. Master gets a
/// reduced set (no review checkboxes / promote / remove): the UI hide is
/// primary, and the cores refuse anyway (defense in depth).
pub(super) fn detail_actions(role: crate::cli::teams_config::RosterRole) -> Vec<DetailAction> {
    use crate::cli::teams_config::RosterRole;
    use DetailAction::*;
    match role {
        RosterRole::Master => vec![ToggleAuto, Back],
        RosterRole::Commit | RosterRole::Plan | RosterRole::Final | RosterRole::Gate => {
            vec![
                ToggleAuto,
                TierCommit,
                TierPlan,
                TierFinal,
                PromoteToMaster,
                Remove,
                Back,
            ]
        }
    }
}

/// One row of the plan-actions page (tui-plan-actions-page). Rows are
/// data the cursor moves over; availability depends on the plan's
/// state — see [`plan_actions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanAction {
    /// Open the plan document overlay (Enter-first row: the old direct
    /// drill-in, one keypress deeper for the menu's sake).
    ReadDoc,
    /// Open the plan's rendered HTML page in the browser.
    OpenHtml,
    /// `stash push` the plan (active only) — behind a confirm.
    Stash,
    /// `finish --force` (active only): finalize past the review gate.
    ForceFinish,
    /// `purge --squash` a finished plan's range into one commit. Only
    /// offered when the range still has >1 commit.
    Squash,
    /// Pause: create a plan-scoped block (active, unblocked only).
    Block,
    /// Resume: answer the pending block (active, blocked only).
    Unblock,
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
    /// An unanswered block is pending on the plan.
    pub blocked: bool,
}

/// The page's rows for a plan state, in display order. Inapplicable
/// actions are ABSENT, not grayed: the states are different pages, not
/// one form (finished plans can't stash/force-finish/block; active
/// plans can't squash; block↔unblock flip on `blocked`).
pub(super) fn plan_actions(st: PlanPageState) -> Vec<PlanAction> {
    use PlanAction::*;
    let mut v = vec![ReadDoc, OpenHtml];
    if st.finished {
        if st.multi_commit {
            v.push(Squash);
        }
    } else {
        v.push(Stash);
        v.push(ForceFinish);
        v.push(if st.blocked { Unblock } else { Block });
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
        Key::Char(b'b') => {
            return actions
                .iter()
                .copied()
                .find(|a| matches!(a, Block | Unblock));
        }
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
    Back,
    None,
}

pub(super) fn plan_detail_nav(sel: usize, actions: &[PlanAction], key: Key) -> PlanNav {
    if let Some(a) = plan_hotkey(key, actions) {
        return PlanNav::Act(a);
    }
    match key {
        Key::Up => PlanNav::Sel(sel.saturating_sub(1)),
        Key::Down => PlanNav::Sel((sel + 1).min(actions.len().saturating_sub(1))),
        Key::Enter => actions.get(sel).map_or(PlanNav::None, |a| PlanNav::Act(*a)),
        Key::Escape => PlanNav::Back,
        _ => PlanNav::None,
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
pub(super) fn compose_squash_message(subject: &str, finalize_body: &str) -> String {
    let body = finalize_body.trim();
    let why = if body.is_empty() {
        "collapsed to one commit from clank status --tui; the original \
         finish predates mandatory finish messages."
    } else {
        body
    };
    format!("{subject}\n\n{why}")
}

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
}

/// Re-bind the panel cursor across a data refresh. An agent/"+ add"
/// cursor re-bounds by CLAMP to the new roster; a QUEUE-row cursor is
/// re-bound by NAME (`queue_name`, captured from the OLD snapshot) — the
/// refresh is often self-inflicted (a reprioritise renames the queue
/// file and re-sorts the list), and the cursor must FOLLOW the item, not
/// snap back to "+ add". A vanished item (promoted/removed under us)
/// drops the cursor to the "+ add" row.
pub(super) fn rebind_panel_sel(
    old_sel: usize,
    stash_name: Option<&str>,
    queue_name: Option<&str>,
    agents_len: usize,
    stash: &[crate::cli::status::StashItemView],
    queue: &[crate::cli::status::QueueItemView],
) -> usize {
    if let Some(name) = stash_name
        && let Some(pos) = stash.iter().position(|i| i.stem == name)
    {
        return agents_len + 1 + pos;
    }
    if let Some(name) = queue_name
        && let Some(pos) = queue.iter().position(|q| q.name == name)
    {
        return agents_len + 1 + stash.len() + pos;
    }
    old_sel.min(agents_len)
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
pub(super) fn agent_detail_nav(sel: usize, actions: &[DetailAction], key: Key) -> DetailNav {
    let current = || actions.get(sel).copied().unwrap_or(DetailAction::Back);
    match key {
        Key::Quit => DetailNav::Quit,
        Key::Escape | Key::Focus => DetailNav::Back,
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
        Key::Escape | Key::Enter | Key::Quit | Key::Focus | Key::Left => DocNav::Back,
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
    AddCandidate { idx: usize },
    /// Remove the agent at this index in `snapshot.agents`.
    RemoveAgent { idx: usize },
    /// `stash push` the open plan page's plan.
    StashPlan,
    /// `purge` (artifacts only) the open plan page's plan.
    PurgeArtifacts,
    /// `purge --drop` the open plan page's plan — the scariest one.
    PurgeDrop,
}

impl ConfirmAction {
    /// Add defaults to Yes (non-destructive); everything else defaults
    /// to No — so Enter (which follows the default) never confirms a
    /// removal, stash, or purge.
    pub(super) fn default_yes(self) -> bool {
        matches!(self, ConfirmAction::AddCandidate { .. })
    }
    /// Confirms that came FROM the plan page return TO it on decline /
    /// completion-with-error; roster confirms return to the panel.
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

/// The open plan page's identity + derived facts (tui-plan-actions-page).
/// Lives in the loop beside `mode` (which stays `Copy`); set and cleared
/// together with `Mode::PlanDetail`/`PurgeChoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlanPage {
    pub(super) stem: String,
    pub(super) st: PlanPageState,
}

/// The interactive state `render_at` needs beyond the snapshot: which
/// mode owns the keyboard and the freshly-read add-picker candidates.
/// Bundled so the render signature stays small (and future interactive
/// bits land here, not as more args).
pub(super) struct PanelView<'a> {
    pub(super) mode: Mode,
    pub(super) plan_page: Option<&'a PlanPage>,
    pub(super) plan_input: Option<&'a TextInput>,
    pub(super) picker: &'a [crate::cli::status::AvailableAgent],
    /// The selected log ENTRY (index into the scroll sequence) — drawn
    /// with the unified selection band when the log is focused.
    pub(super) log_cursor: usize,
}

impl<'a> PanelView<'a> {
    /// A view with just a mode (no picker, cursor at 0) — the common
    /// case for tests and the log-scroll default.
    #[cfg(test)]
    pub(super) fn just(mode: Mode) -> Self {
        Self {
            mode,
            plan_page: None,
            plan_input: None,
            picker: &[],
            log_cursor: 0,
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
pub(super) fn agent_panel_action(
    sel: usize,
    agents: &[crate::cli::status::AgentAutoRow],
    stash_len: usize,
    queue_len: usize,
    key: Key,
) -> PanelAction {
    let add_row = agents.len();
    let total = add_row + 1 + stash_len + queue_len;
    let on_add = sel == add_row;
    // Which segment the cursor sits in: STASH rows come first (they
    // render above the queue), then QUEUE rows.
    let stash_idx = (sel > add_row && sel <= add_row + stash_len).then(|| sel - add_row - 1);
    let queue_idx = (sel > add_row + stash_len).then(|| sel - add_row - 1 - stash_len);
    let on_last = sel + 1 == total;
    match key {
        Key::Quit => PanelAction::Quit,
        Key::Focus | Key::Escape => PanelAction::LeaveFocus,
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
        Key::Enter => match (stash_idx, queue_idx) {
            (Some(i), _) => PanelAction::OpenStashItem(i),
            (_, Some(q)) => PanelAction::OpenQueueItem(q),
            _ => PanelAction::OpenDetail(sel),
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
        Key::Space if stash_idx.is_some() || queue_idx.is_some() => PanelAction::None,
        Key::Space => PanelAction::ToggleAuto(sel),
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
        b'\t' | b'a' => Key::Focus,
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
            blocked: false,
        }
    }

    #[test]
    fn plan_actions_active_page_has_stash_force_finish_block_and_purge() {
        use PlanAction::*;
        assert_eq!(
            plan_actions(active_st()),
            vec![ReadDoc, OpenHtml, Stash, ForceFinish, Block, Purge, Back]
        );
    }

    #[test]
    fn plan_actions_blocked_page_flips_block_to_unblock() {
        use PlanAction::*;
        let st = PlanPageState {
            blocked: true,
            ..active_st()
        };
        let rows = plan_actions(st);
        assert!(rows.contains(&Unblock) && !rows.contains(&Block));
    }

    #[test]
    fn plan_actions_finished_page_swaps_the_middle_block_for_squash() {
        use PlanAction::*;
        let st = PlanPageState {
            finished: true,
            multi_commit: true,
            blocked: false,
        };
        assert_eq!(
            plan_actions(st),
            vec![ReadDoc, OpenHtml, Squash, Purge, Back]
        );
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
        assert_eq!(plan_hotkey(Key::Char(b'b'), &active), Some(Block));
        assert_eq!(plan_hotkey(Key::Char(b'p'), &active), Some(Purge));
        assert_eq!(plan_hotkey(Key::Html, &active), Some(OpenHtml));
        assert_eq!(
            plan_hotkey(Key::Char(b'c'), &active),
            None,
            "no squash on active"
        );
        let blocked = plan_actions(PlanPageState {
            blocked: true,
            ..active_st()
        });
        assert_eq!(plan_hotkey(Key::Char(b'b'), &blocked), Some(Unblock));
        let finished = plan_actions(PlanPageState {
            finished: true,
            multi_commit: true,
            blocked: false,
        });
        assert_eq!(plan_hotkey(Key::Char(b'c'), &finished), Some(Squash));
        assert_eq!(
            plan_hotkey(Key::Char(b's'), &finished),
            None,
            "no stash on finished"
        );
    }

    #[test]
    fn plan_detail_nav_moves_activates_and_backs_out() {
        let actions = plan_actions(active_st());
        assert_eq!(plan_detail_nav(0, &actions, Key::Down), PlanNav::Sel(1));
        assert_eq!(
            plan_detail_nav(actions.len() - 1, &actions, Key::Down),
            PlanNav::Sel(actions.len() - 1),
            "clamped at the last row"
        );
        assert_eq!(plan_detail_nav(0, &actions, Key::Up), PlanNav::Sel(0));
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Enter),
            PlanNav::Act(PlanAction::ReadDoc)
        );
        assert_eq!(plan_detail_nav(0, &actions, Key::Escape), PlanNav::Back);
        // A hotkey acts regardless of the cursor.
        assert_eq!(
            plan_detail_nav(0, &actions, Key::Char(b'p')),
            PlanNav::Act(PlanAction::Purge)
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
        );
        validate(Some(&real), "my-plan").expect("subject + finalize body");
        assert!(real.contains("\n\nthe plan landed"));
        // Legacy plan (no finalize body) → provenance fallback passes.
        let legacy = compose_squash_message("collapse it all", "  ");
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
        // Tab and `a` both focus the agent panel; lone Esc leaves it.
        let got: Vec<_> = parse_keys(b"\ta\x1b")
            .iter()
            .map(std::mem::discriminant)
            .collect();
        let want: Vec<_> = [Focus, Focus, Escape]
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(got, want, "tab/a focus, esc leaves");
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
        assert_eq!(Mode::AddPicker { sel: 0 }.toggle_focus(2), Mode::LogScroll);
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
        assert_eq!(detail_actions(RosterRole::Master), vec![ToggleAuto, Back]);
        let reviewer = vec![
            ToggleAuto,
            TierCommit,
            TierPlan,
            TierFinal,
            PromoteToMaster,
            Remove,
            Back,
        ];
        assert_eq!(detail_actions(RosterRole::Commit), reviewer);
        assert_eq!(detail_actions(RosterRole::Gate), reviewer);
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
        let add = ConfirmAction::AddCandidate { idx: 0 };
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
            ConfirmAction::AddCandidate { idx: 0 }.default_yes(),
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
            agent_panel_action(2, &agents, 0, 0, Key::Enter),
            PanelAction::OpenPicker
        );
        assert_eq!(
            agent_panel_action(2, &agents, 0, 0, Key::Space),
            PanelAction::OpenPicker
        );
        // Agent rows: Enter opens the detail page; Space toggles auto.
        assert_eq!(
            agent_panel_action(1, &agents, 0, 0, Key::Enter),
            PanelAction::OpenDetail(1)
        );
        assert_eq!(
            agent_panel_action(1, &agents, 0, 0, Key::Space),
            PanelAction::ToggleAuto(1)
        );
        // DEL is no longer a panel action (removal lives on the detail page).
        assert_eq!(
            agent_panel_action(1, &agents, 0, 0, Key::Delete),
            PanelAction::None
        );
        // Navigation: Down within the panel moves; Down at the +add row
        // (index 2 == agents.len()) crosses into the log; Tab/Esc leave;
        // q quits.
        assert_eq!(
            agent_panel_action(0, &agents, 0, 0, Key::Down),
            PanelAction::MoveCursor(1)
        );
        assert_eq!(
            agent_panel_action(2, &agents, 0, 0, Key::Down),
            PanelAction::EnterLog,
            "Down past +add flows into the log"
        );
        assert_eq!(
            agent_panel_action(0, &agents, 0, 0, Key::Up),
            PanelAction::MoveCursor(0),
            "Up at the top stays put"
        );
        assert_eq!(
            agent_panel_action(1, &agents, 0, 0, Key::Focus),
            PanelAction::LeaveFocus
        );
        assert_eq!(
            agent_panel_action(1, &agents, 0, 0, Key::Quit),
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
            agent_panel_action(3, &agents, 0, ql, Key::Enter),
            PanelAction::OpenQueueItem(0)
        );
        assert_eq!(
            agent_panel_action(4, &agents, 0, ql, Key::Html),
            PanelAction::OpenQueueHtml(1)
        );
        // +/- nudge the priority number by 50 (loop clamps + persists via
        // queue::set_priority).
        assert_eq!(
            agent_panel_action(3, &agents, 0, ql, Key::Plus),
            PanelAction::NudgeQueue { idx: 0, delta: 50 }
        );
        assert_eq!(
            agent_panel_action(3, &agents, 0, ql, Key::Minus),
            PanelAction::NudgeQueue { idx: 0, delta: -50 }
        );
        // Space on a queue row is inert (no accidental overlay); `o` on an
        // agent row is inert too.
        assert_eq!(
            agent_panel_action(3, &agents, 0, ql, Key::Space),
            PanelAction::None
        );
        assert_eq!(
            agent_panel_action(1, &agents, 0, ql, Key::Html),
            PanelAction::None
        );
        // Down from "+ add" now enters the queue, not the log; Down from
        // the LAST queue row crosses into the log.
        assert_eq!(
            agent_panel_action(2, &agents, 0, ql, Key::Down),
            PanelAction::MoveCursor(3)
        );
        assert_eq!(
            agent_panel_action(4, &agents, 0, ql, Key::Down),
            PanelAction::EnterLog
        );
        // With an empty queue, Down from "+ add" still enters the log.
        assert_eq!(
            agent_panel_action(2, &agents, 0, 0, Key::Down),
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
            agent_panel_action(3, &agents, sl, ql, Key::Enter),
            PanelAction::OpenStashItem(0)
        );
        assert_eq!(
            agent_panel_action(3, &agents, sl, ql, Key::Html),
            PanelAction::OpenStashHtml(0)
        );
        // The queue row sits AFTER the stash segment.
        assert_eq!(
            agent_panel_action(4, &agents, sl, ql, Key::Enter),
            PanelAction::OpenQueueItem(0)
        );
        // +/- are queue-only; inert on a stash row.
        assert_eq!(
            agent_panel_action(3, &agents, sl, ql, Key::Plus),
            PanelAction::None
        );
        // Down from the LAST row (the queue row) crosses into the log.
        assert_eq!(
            agent_panel_action(4, &agents, sl, ql, Key::Down),
            PanelAction::EnterLog
        );
    }

    #[test]
    fn rebind_panel_sel_follows_a_stash_row_by_name() {
        use crate::cli::status::{QueueItemView, StashItemView};
        let st = |stem: &str| StashItemView {
            stem: stem.to_string(),
            waiting_for: None,
            ready: false,
            commits: 1,
        };
        let q = |name: &str| QueueItemView {
            priority: 500,
            name: name.to_string(),
        };
        let stash = [st("a"), st("b")];
        let queue = [q("x")];
        // Cursor on stash "b" (sel 4 = 2 agents + add + idx 1) follows it.
        assert_eq!(rebind_panel_sel(4, Some("b"), None, 2, &stash, &queue), 4);
        // A queue selection offsets past the stash segment.
        assert_eq!(rebind_panel_sel(5, None, Some("x"), 2, &stash, &queue), 5);
        // Vanished stash item → "+ add".
        assert_eq!(
            rebind_panel_sel(4, Some("gone"), None, 2, &stash, &queue),
            2
        );
    }

    #[test]
    fn rebind_panel_sel_follows_a_queue_row_by_name_across_refresh() {
        use crate::cli::status::QueueItemView;
        let q = |prio: u16, name: &str| QueueItemView {
            priority: prio,
            name: name.to_string(),
        };
        // Cursor was on "b" (sel 4 = agents 2 + add 1 + queue idx 1); a
        // nudge re-sorted the queue so "b" is now FIRST — the cursor
        // follows it to sel 3, never snapping back to "+ add".
        let new_queue = [q(100, "b"), q(500, "a")];
        assert_eq!(rebind_panel_sel(4, None, Some("b"), 2, &[], &new_queue), 3);
        // The item left the queue (promoted/removed) → "+ add" row.
        let without_b = [q(500, "a")];
        assert_eq!(rebind_panel_sel(4, None, Some("b"), 2, &[], &without_b), 2);
        // A non-queue cursor keeps the old clamp semantics.
        assert_eq!(rebind_panel_sel(1, None, None, 2, &[], &new_queue), 1);
        assert_eq!(
            rebind_panel_sel(9, None, None, 2, &[], &new_queue),
            2,
            "clamped to +add"
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
