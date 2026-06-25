//! The input state machine: raw stdin → [`Key`], the [`Mode`] that owns
//! the keyboard (and its sub-states), and the PURE routing/decision
//! functions that map a key in a mode to an action the loop executes.
//! No IO — the effectful appliers (roster mutations) live in the loop
//! (the IO shell), so this whole module is unit-tested headless.

#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// Backspace / DEL — remove the selected reviewer.
    Delete,
    /// `y` — confirm.
    Yes,
    /// `n` — decline.
    No,
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
    /// Flip a reviewer between the commit and gate tiers.
    SwitchTier,
    /// Promote a reviewer to master (the core also demotes the old one).
    PromoteToMaster,
    /// Remove the agent from the team (behind the confirm).
    Remove,
    /// Leave the detail page.
    Back,
}

/// The detail-page actions for `role`, in display order. Master gets a
/// reduced set (no tier-switch / promote / remove): the UI hide is
/// primary, and the cores refuse anyway (defense in depth).
pub(super) fn detail_actions(role: crate::cli::teams_config::RosterRole) -> Vec<DetailAction> {
    use crate::cli::teams_config::RosterRole;
    use DetailAction::*;
    match role {
        RosterRole::Master => vec![ToggleAuto, Back],
        RosterRole::Commit | RosterRole::Gate => {
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        }
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

/// Pure key routing for the detail page over its action menu.
pub(super) fn agent_detail_nav(sel: usize, actions: &[DetailAction], key: Key) -> DetailNav {
    match key {
        Key::Quit => DetailNav::Quit,
        Key::Escape | Key::Focus => DetailNav::Back,
        Key::Up => DetailNav::MoveCursor(move_selection(sel, actions.len(), false)),
        Key::Down => DetailNav::MoveCursor(move_selection(sel, actions.len(), true)),
        Key::Enter | Key::Space => {
            DetailNav::Activate(actions.get(sel).copied().unwrap_or(DetailAction::Back))
        }
        _ => DetailNav::None,
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
}

/// Pure key routing for the agent panel. `agents` is the roster; the
/// "+ add" row sits at index `agents.len()`. Removal/role/tier are no
/// longer panel actions — Enter opens the detail page where they live.
pub(super) fn agent_panel_action(
    sel: usize,
    agents: &[crate::cli::status::AgentAutoRow],
    key: Key,
) -> PanelAction {
    let add_row = agents.len();
    let on_add = sel == add_row;
    match key {
        Key::Quit => PanelAction::Quit,
        Key::Focus | Key::Escape => PanelAction::LeaveFocus,
        Key::Up => PanelAction::MoveCursor(move_selection(sel, add_row + 1, false)),
        // `Down` past the bottom row ("+ add") flows into the log —
        // continuous navigation across the panel↔log boundary.
        Key::Down if on_add => PanelAction::EnterLog,
        Key::Down => PanelAction::MoveCursor(move_selection(sel, add_row + 1, true)),
        // Enter activates the row: the picker on "+ add", the detail page
        // on an agent. Space is the quick inline auto-toggle (or the
        // picker on "+ add").
        Key::Enter if on_add => PanelAction::OpenPicker,
        Key::Enter => PanelAction::OpenDetail(sel),
        Key::Space if on_add => PanelAction::OpenPicker,
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
        } else if rest.starts_with(b"\x1b[") {
            // Unknown CSI (e.g. left/right arrows, F-keys): consume
            // through its final byte so a lone Esc isn't misread out of
            // the sequence's leading bytes.
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
        // An unknown CSI (left arrow) is consumed whole — NOT misread as a
        // lone Esc that would spuriously close the panel.
        assert!(
            parse_keys(b"\x1b[D").is_empty(),
            "left arrow consumed, no stray Escape"
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
        assert_eq!(
            detail_actions(RosterRole::Commit),
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        );
        assert_eq!(
            detail_actions(RosterRole::Gate),
            vec![ToggleAuto, SwitchTier, PromoteToMaster, Remove, Back]
        );
    }

    #[test]
    fn agent_detail_nav_routes_menu_keys() {
        use DetailAction::*;
        let actions = [ToggleAuto, SwitchTier, Remove, Back];
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
            DetailNav::Activate(SwitchTier)
        );
        assert_eq!(agent_detail_nav(0, &actions, Key::Escape), DetailNav::Back);
        assert_eq!(agent_detail_nav(0, &actions, Key::Quit), DetailNav::Quit);
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
            agent_panel_action(2, &agents, Key::Enter),
            PanelAction::OpenPicker
        );
        assert_eq!(
            agent_panel_action(2, &agents, Key::Space),
            PanelAction::OpenPicker
        );
        // Agent rows: Enter opens the detail page; Space toggles auto.
        assert_eq!(
            agent_panel_action(1, &agents, Key::Enter),
            PanelAction::OpenDetail(1)
        );
        assert_eq!(
            agent_panel_action(1, &agents, Key::Space),
            PanelAction::ToggleAuto(1)
        );
        // DEL is no longer a panel action (removal lives on the detail page).
        assert_eq!(
            agent_panel_action(1, &agents, Key::Delete),
            PanelAction::None
        );
        // Navigation: Down within the panel moves; Down at the +add row
        // (index 2 == agents.len()) crosses into the log; Tab/Esc leave;
        // q quits.
        assert_eq!(
            agent_panel_action(0, &agents, Key::Down),
            PanelAction::MoveCursor(1)
        );
        assert_eq!(
            agent_panel_action(2, &agents, Key::Down),
            PanelAction::EnterLog,
            "Down past +add flows into the log"
        );
        assert_eq!(
            agent_panel_action(0, &agents, Key::Up),
            PanelAction::MoveCursor(0),
            "Up at the top stays put"
        );
        assert_eq!(
            agent_panel_action(1, &agents, Key::Focus),
            PanelAction::LeaveFocus
        );
        assert_eq!(agent_panel_action(1, &agents, Key::Quit), PanelAction::Quit);
    }
}
