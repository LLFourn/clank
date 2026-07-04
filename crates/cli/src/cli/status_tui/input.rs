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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmAction {
    /// Add the candidate at this index in the loop's picker list.
    AddCandidate { idx: usize },
    /// Remove the agent at this index in `snapshot.agents`.
    RemoveAgent { idx: usize },
}

impl ConfirmAction {
    /// Add defaults to Yes (non-destructive); remove defaults to No —
    /// so Enter (which follows the default) never confirms a removal.
    pub(super) fn default_yes(self) -> bool {
        matches!(self, ConfirmAction::AddCandidate { .. })
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
            _ => Mode::LogScroll,
        }
    }
}

/// The interactive state `render_at` needs beyond the snapshot: which
/// mode owns the keyboard and the freshly-read add-picker candidates.
/// Bundled so the render signature stays small (and future interactive
/// bits land here, not as more args).
pub(super) struct PanelView<'a> {
    pub(super) mode: Mode,
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
pub(super) fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"\x1b[A") {
            keys.push(Key::Up);
            i += 3;
        } else if rest.starts_with(b"\x1b[B") {
            keys.push(Key::Down);
            i += 3;
        } else if rest.starts_with(b"\x1b[5~") {
            keys.push(Key::PageUp);
            i += 4;
        } else if rest.starts_with(b"\x1b[6~") {
            keys.push(Key::PageDown);
            i += 4;
        } else if rest.starts_with(b"\x1b[D") {
            keys.push(Key::Left);
            i += 3;
        } else if rest.starts_with(b"\x1b[C") {
            keys.push(Key::Right);
            i += 3;
        } else if rest.starts_with(b"\x1b[") {
            // Unknown CSI (F-keys, etc.): consume through its final byte
            // so a lone Esc isn't misread out of the sequence's leading
            // bytes.
            let mut j = 2;
            while j < rest.len() && !(0x40..=0x7e).contains(&rest[j]) {
                j += 1;
            }
            i += (j + 1).min(rest.len());
        } else {
            match bytes[i] {
                b'k' => keys.push(Key::Up),
                b'j' => keys.push(Key::Down),
                b'g' => keys.push(Key::Top),
                b'G' => keys.push(Key::Bottom),
                b' ' => keys.push(Key::Space),
                b'b' => keys.push(Key::PageUp),
                b'\t' | b'a' => keys.push(Key::Focus),
                b'\r' | b'\n' => keys.push(Key::Enter),
                0x7f | 0x08 => keys.push(Key::Delete),
                b'y' => keys.push(Key::Yes),
                b'n' => keys.push(Key::No),
                b'o' => keys.push(Key::Html),
                b'+' => keys.push(Key::Plus),
                b'-' => keys.push(Key::Minus),
                0x1b => keys.push(Key::Escape),
                b'q' => keys.push(Key::Quit),
                _ => {}
            }
            i += 1;
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
        assert!(parse_keys(b"xz.").is_empty(), "unmapped bytes are ignored");
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
