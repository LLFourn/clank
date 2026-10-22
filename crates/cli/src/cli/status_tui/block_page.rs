//! A pending block's page: the question in full, and the one answer it
//! takes, typed beneath it.

use super::input::{AnyKey, InputNav, Key, TextInput, move_selection, text_input_nav};
use crate::cli::block::BlockEntry;

/// The name of the TUI's own repo pause block.
pub(super) const PAUSE: &str = "pause";

/// The ask a page was opened on, captured whole from what the human was
/// reading. A refresh can insert, reorder, replace or remove entries in
/// the live list; none of that retargets the page, because the page
/// never looks an ask up by its place in that list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ask {
    pub(super) agent: String,
    pub(super) name: String,
    pub(super) occurrence: Option<String>,
    pub(super) question: String,
}

impl Ask {
    pub(super) fn of(b: &BlockEntry) -> Self {
        Self {
            agent: b.agent.clone(),
            name: b.name.clone(),
            occurrence: b.occurrence.clone(),
            question: b.question.clone(),
        }
    }

    fn shown(&self) -> crate::cli::block::Shown<'_> {
        crate::cli::block::Shown {
            occurrence: self.occurrence.as_deref(),
            question: &self.question,
        }
    }
}

/// Whether the opened ask is still the one waiting for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Standing {
    Open,
    Answered,
    /// The name was asked again, even with the same text: a different
    /// question, which this page never saw.
    Replaced,
    /// Withdrawn, or answered and already received.
    Gone,
}

impl Standing {
    pub(super) fn reason(self) -> Option<&'static str> {
        match self {
            Standing::Open => None,
            Standing::Answered => Some("this question was answered elsewhere"),
            Standing::Replaced => Some(
                "this question was asked again since the page opened; go back and open the new one",
            ),
            Standing::Gone => Some("this question is no longer pending"),
        }
    }
}

/// The opened ask's standing in `live`, judged by its occurrence, never
/// by its name alone or its position.
pub(super) fn standing(ask: &Ask, live: &[BlockEntry]) -> Standing {
    match live
        .iter()
        .find(|b| b.agent == ask.agent && b.name == ask.name)
    {
        None => Standing::Gone,
        Some(b) if b.occurrence != ask.occurrence || b.question != ask.question => {
            Standing::Replaced
        }
        Some(b) if b.answer.is_some() => Standing::Answered,
        Some(_) => Standing::Open,
    }
}

/// Where Back lands: the surface the page was opened from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Origin {
    Log,
    /// Resolved by label on the way back, since the panel can reorder
    /// while the page is open.
    Agent(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Button {
    Answer,
    Back,
}

pub(super) const BUTTONS: [Button; 2] = [Button::Answer, Button::Back];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BlockPage {
    pub(super) ask: Ask,
    pub(super) origin: Origin,
    /// Index into [`BUTTONS`].
    pub(super) sel: usize,
    pub(super) typing: bool,
    /// Kept across Esc and refusals, so leaving the input or failing to
    /// send never costs the human what they wrote.
    pub(super) draft: TextInput,
    pub(super) failure: Option<String>,
    pub(super) scroll: usize,
}

/// What one key on the page asks of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nav {
    Stay,
    Leave,
    Quit,
    Answered,
}

impl BlockPage {
    pub(super) fn open(ask: Ask, origin: Origin) -> Self {
        Self {
            ask,
            origin,
            sel: 0,
            typing: false,
            draft: TextInput::default(),
            failure: None,
            scroll: 0,
        }
    }

    /// One key. `max_scroll` and `page` bound the question's scroll as
    /// it is drawn; `answer` records the answer for exactly the ask it
    /// is handed.
    pub(super) fn key(
        &mut self,
        key: AnyKey,
        live: &[BlockEntry],
        max_scroll: usize,
        page: usize,
        answer: impl FnOnce(&Ask, &str) -> anyhow::Result<()>,
    ) -> Nav {
        match key {
            AnyKey::Text(tk) if self.typing => match text_input_nav(&mut self.draft, tk) {
                InputNav::Cancel => {
                    self.typing = false;
                    Nav::Stay
                }
                InputNav::Submit => self.submit(live, answer),
                InputNav::None => Nav::Stay,
            },
            AnyKey::Cmd(k) if !self.typing => self.button_key(k, live, max_scroll, page),
            AnyKey::Text(_) | AnyKey::Cmd(_) => Nav::Stay,
        }
    }

    fn button_key(&mut self, k: Key, live: &[BlockEntry], max_scroll: usize, page: usize) -> Nav {
        match k {
            Key::Quit => Nav::Quit,
            Key::Escape | Key::Left | Key::Focus | Key::Char(b'a') => Nav::Leave,
            Key::Up => {
                self.sel = move_selection(self.sel, BUTTONS.len(), false);
                Nav::Stay
            }
            Key::Down => {
                self.sel = move_selection(self.sel, BUTTONS.len(), true);
                Nav::Stay
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_sub(page).min(max_scroll);
                Nav::Stay
            }
            Key::Space | Key::PageDown => {
                self.scroll = (self.scroll + page).min(max_scroll);
                Nav::Stay
            }
            Key::Top => {
                self.scroll = 0;
                Nav::Stay
            }
            Key::Bottom => {
                self.scroll = max_scroll;
                Nav::Stay
            }
            Key::Enter => match BUTTONS.get(self.sel) {
                Some(Button::Answer) => {
                    self.failure = standing(&self.ask, live).reason().map(str::to_owned);
                    self.typing = self.failure.is_none();
                    Nav::Stay
                }
                Some(Button::Back) => Nav::Leave,
                None => Nav::Stay,
            },
            _ => Nav::Stay,
        }
    }

    fn submit(
        &mut self,
        live: &[BlockEntry],
        answer: impl FnOnce(&Ask, &str) -> anyhow::Result<()>,
    ) -> Nav {
        let text = self.draft.buf.trim();
        if text.is_empty() {
            return Nav::Stay;
        }
        if let Some(reason) = standing(&self.ask, live).reason() {
            self.failure = Some(reason.to_owned());
            return Nav::Stay;
        }
        match answer(&self.ask, text) {
            Ok(()) => Nav::Answered,
            Err(e) => {
                self.failure = Some(format!("{e:#}"));
                Nav::Stay
            }
        }
    }
}

/// The page Enter on scroll entry `cursor` opens, when that entry is a
/// line of a pending question: any of its wrapped lines opens it.
pub(super) fn from_log(seq: &[super::scroll::Seg<'_>], cursor: usize) -> Option<BlockPage> {
    match seq.get(cursor) {
        Some(super::scroll::Seg::Ask(line)) => line
            .block
            .as_ref()
            .map(|ask| BlockPage::open(Ask::clone(ask), Origin::Log)),
        _ => None,
    }
}

/// The page Enter on `label`'s agent row opens while that agent is
/// asking. The repo pause is the human's own question, authored under
/// the master only because blocks live in agent dirs, so it does not
/// make the master an asking agent.
pub(super) fn from_agent(label: &str, live: &[BlockEntry]) -> Option<BlockPage> {
    live.iter()
        .find(|b| b.agent == label && b.answer.is_none() && b.name != PAUSE)
        .map(|b| BlockPage::open(Ask::of(b), Origin::Agent(label.to_owned())))
}

/// Record `text` as the answer to exactly `ask`: the store refuses it
/// if the name has been asked again since, even with the same text.
pub(super) fn answer_in_store(repo: &std::path::Path, ask: &Ask, text: &str) -> anyhow::Result<()> {
    crate::cli::block::answer_block(repo, &ask.agent, &ask.name, text, Some(ask.shown()))
        .map(|_| ())
}

/// Whether the repo is paused: its pause block is asked and unanswered.
pub(super) fn repo_paused(live: &[BlockEntry]) -> bool {
    live.iter().any(|b| b.answer.is_none() && b.name == PAUSE)
}

#[cfg(test)]
mod tests {
    use super::super::input::{AnyKey, Key, Mode, PanelAction, TextKey, agent_panel_action};
    use super::super::{PageStep, agent_row_enter, drive_block_page, panel_row_list};
    use super::*;
    use crate::cli::status::StatusSnapshot;
    use crate::cli::status_tui::fixtures::two_agent_snap;
    use crate::cli::status_tui::render::block_ask_spans;
    use crate::cli::status_tui::scroll::build_scroll;

    fn entry(agent: &str, name: &str, occurrence: &str, question: &str) -> BlockEntry {
        BlockEntry {
            agent: agent.into(),
            name: name.into(),
            occurrence: Some(occurrence.into()),
            question: question.into(),
            answer: None,
        }
    }

    fn opened_from_log(s: &StatusSnapshot, cursor: usize) -> Option<BlockPage> {
        let lines = block_ask_spans(s, 20);
        from_log(&build_scroll(s, &lines), cursor)
    }

    fn typed(text: &str) -> Vec<AnyKey> {
        let mut keys = vec![AnyKey::Cmd(Key::Enter)];
        keys.extend(text.bytes().map(|b| AnyKey::Text(TextKey::Char(b))));
        keys
    }

    /// Each key through the page as the loop drives it, answering with
    /// `answer`; the last key's step.
    fn press(
        slot: &mut Option<BlockPage>,
        s: &StatusSnapshot,
        keys: &[AnyKey],
        answer: &mut dyn FnMut(&Ask, &str) -> anyhow::Result<()>,
    ) -> PageStep {
        let mut step = PageStep::To(Mode::BlockPage);
        for k in keys {
            step = drive_block_page(slot, *k, s, 24, 80, |a, t| answer(a, t));
        }
        step
    }

    fn recorder(
        calls: &mut Vec<(Ask, String)>,
    ) -> impl FnMut(&Ask, &str) -> anyhow::Result<()> + '_ {
        |a, t| {
            calls.push((a.clone(), t.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn enter_on_every_line_of_a_question_opens_that_question() {
        let mut s = two_agent_snap();
        s.blocks = vec![
            entry("claude", "short", "01", "Proceed?"),
            entry(
                "codex",
                "long",
                "02",
                "one two three four five six seven eight nine ten",
            ),
        ];
        let lines = block_ask_spans(&s, 20);
        assert!(lines.len() >= 4, "the long question wraps at 20 columns");
        assert_eq!(opened_from_log(&s, 0).unwrap().ask, Ask::of(&s.blocks[0]));
        for continuation in 1..lines.len() - 1 {
            let page = opened_from_log(&s, continuation).expect("every question line opens");
            assert_eq!(page.ask, Ask::of(&s.blocks[1]), "line {continuation}");
            assert_eq!(page.origin, Origin::Log);
        }
        assert_eq!(
            opened_from_log(&s, lines.len() - 1),
            None,
            "the hint is no question"
        );
    }

    #[test]
    fn enter_on_an_asking_agents_row_opens_its_question() {
        let mut s = two_agent_snap();
        s.blocks = vec![
            entry("claude", PAUSE, "01", "held for the release"),
            entry("codex", "scope", "02", "Ship it?"),
        ];
        let rows = panel_row_list(&s, 80);
        let enter = |label: &str| {
            let sel = rows
                .iter()
                .position(|r| matches!(r, super::super::input::PanelRow::Agent(i) if s.agents[*i].label == label))
                .unwrap();
            match agent_panel_action(sel, &rows, Key::Enter) {
                PanelAction::OpenDetail(i) => agent_row_enter(i, &s),
                other => panic!("{other:?}"),
            }
        };
        let (mode, page) = enter("codex");
        assert_eq!(mode, Mode::BlockPage);
        let page = page.unwrap();
        assert_eq!(page.ask, Ask::of(&s.blocks[1]));
        assert_eq!(page.origin, Origin::Agent("codex".into()));
        assert_eq!(
            enter("claude"),
            (Mode::AgentDetail { idx: 0, sel: 0 }, None),
            "the repo pause is the human's question, not the master's"
        );
    }

    #[test]
    fn answer_type_enter_records_that_question_and_returns_where_it_opened() {
        let mut s = two_agent_snap();
        s.blocks = vec![entry("codex", "scope", "02", "Ship it?")];
        let mut slot = opened_from_log(&s, 0);
        let mut calls = Vec::new();
        let mut keys = typed("yes, ship");
        keys.push(AnyKey::Text(TextKey::Enter));
        let step = press(&mut slot, &s, &keys, &mut recorder(&mut calls));
        assert_eq!(step, PageStep::Answered(Mode::LogScroll));
        assert_eq!(calls, [(Ask::of(&s.blocks[0]), "yes, ship".to_string())]);
        assert_eq!(slot, None, "the page closes with its mode");
    }

    #[test]
    fn a_block_inserted_before_the_open_one_does_not_take_its_answer() {
        let mut s = two_agent_snap();
        s.blocks = vec![entry("codex", "scope", "02", "Ship it?")];
        let mut slot = opened_from_log(&s, 0);
        let opened = slot.as_ref().unwrap().ask.clone();
        let mut calls = Vec::new();
        press(&mut slot, &s, &typed("yes"), &mut recorder(&mut calls));
        s.blocks
            .insert(0, entry("claude", "other", "01", "Ship it?"));
        let step = press(
            &mut slot,
            &s,
            &[AnyKey::Text(TextKey::Enter)],
            &mut recorder(&mut calls),
        );
        assert_eq!(step, PageStep::Answered(Mode::LogScroll));
        assert_eq!(calls, [(opened, "yes".to_string())]);
    }

    fn store_with(question: &str) -> (tempfile::TempDir, clank_core::AgentLabel) {
        let dir = tempfile::tempdir().unwrap();
        let codex = clank_core::AgentLabel::parse("codex").unwrap();
        crate::cli::block::ask(dir.path(), &codex, "scope", question).unwrap();
        (dir, codex)
    }

    /// The same name asked again with the same text is a different
    /// question. Whether or not a refresh has shown the page the new
    /// ask, sending refuses, keeps the draft, and answers nothing.
    #[test]
    fn a_re_ask_with_identical_text_is_refused_and_stays_unanswered() {
        for refreshed in [true, false] {
            let (dir, codex) = store_with("Ship it?");
            let repo = dir.path();
            let mut s = two_agent_snap();
            s.blocks = crate::cli::block::scan_blocks(repo);
            let mut slot = opened_from_log(&s, 0);
            let mut store = |a: &Ask, t: &str| answer_in_store(repo, a, t);
            press(&mut slot, &s, &typed("yes"), &mut store);
            crate::cli::block::ask(repo, &codex, "scope", "Ship it?").unwrap();
            if refreshed {
                s.blocks = crate::cli::block::scan_blocks(repo);
            }
            let step = press(&mut slot, &s, &[AnyKey::Text(TextKey::Enter)], &mut store);
            assert_eq!(step, PageStep::To(Mode::BlockPage), "refreshed {refreshed}");
            let page = slot.as_ref().unwrap();
            let failure = page.failure.as_deref().unwrap_or_default();
            let want = if refreshed { "asked again" } else { "changed" };
            assert!(failure.contains(want), "refreshed {refreshed}: {failure}");
            assert_eq!(page.draft.buf, "yes", "refreshed {refreshed}");
            let live = crate::cli::block::scan_blocks(repo);
            assert_eq!(live.len(), 1);
            assert_eq!(
                live[0].answer, None,
                "refreshed {refreshed}: the new ask is unanswered"
            );
        }
    }

    #[test]
    fn a_question_answered_elsewhere_refuses_with_its_reason_and_writes_nothing() {
        let (dir, _) = store_with("Ship it?");
        let repo = dir.path();
        let mut s = two_agent_snap();
        s.blocks = crate::cli::block::scan_blocks(repo);
        let mut slot = opened_from_log(&s, 0);
        let mut calls = Vec::new();
        press(&mut slot, &s, &typed("yes"), &mut recorder(&mut calls));
        crate::cli::block::answer_block(repo, "codex", "scope", "from the browser", None).unwrap();
        s.blocks = crate::cli::block::scan_blocks(repo);
        let step = press(
            &mut slot,
            &s,
            &[AnyKey::Text(TextKey::Enter)],
            &mut recorder(&mut calls),
        );
        assert_eq!(step, PageStep::To(Mode::BlockPage));
        assert!(calls.is_empty(), "nothing is written");
        let page = slot.as_ref().unwrap();
        assert_eq!(page.failure.as_deref(), Standing::Answered.reason());
        assert_eq!(page.draft.buf, "yes");
        let (lines, _) = crate::cli::status_tui::render::render_block_page(
            page,
            standing(&page.ask, &s.blocks),
            24,
            80,
        );
        let text = crate::cli::status_tui::fixtures::visible(&lines.join("\n"));
        assert!(text.contains("answered elsewhere"), "{text}");
        assert_eq!(
            crate::cli::block::scan_blocks(repo)[0].answer.as_deref(),
            Some("from the browser")
        );
    }

    #[test]
    fn a_withdrawn_question_refuses_the_answer_button() {
        let mut s = two_agent_snap();
        s.blocks = vec![entry("codex", "scope", "02", "Ship it?")];
        let mut slot = opened_from_log(&s, 0);
        s.blocks.clear();
        let mut calls = Vec::new();
        let step = press(&mut slot, &s, &typed("yes"), &mut recorder(&mut calls));
        assert_eq!(step, PageStep::To(Mode::BlockPage));
        let page = slot.as_ref().unwrap();
        assert!(!page.typing, "no input opens for a question that is gone");
        assert_eq!(page.failure.as_deref(), Standing::Gone.reason());
        assert!(calls.is_empty());
    }

    #[test]
    fn esc_returns_to_the_buttons_keeping_the_draft_then_leaves() {
        let mut s = two_agent_snap();
        s.blocks = vec![entry("codex", "scope", "02", "Ship it?")];
        let rows = panel_row_list(&s, 80);
        let codex_row = rows
            .iter()
            .position(|r| *r == super::super::input::PanelRow::Agent(1))
            .unwrap();
        let mut slot = agent_row_enter(1, &s).1;
        let mut calls = Vec::new();
        let mut keys = typed("draft");
        keys.push(AnyKey::Text(TextKey::Esc));
        assert_eq!(
            press(&mut slot, &s, &keys, &mut recorder(&mut calls)),
            PageStep::To(Mode::BlockPage)
        );
        let page = slot.as_ref().unwrap();
        assert!(!page.typing);
        assert_eq!(page.draft.buf, "draft");
        assert_eq!(
            press(
                &mut slot,
                &s,
                &[AnyKey::Cmd(Key::Escape)],
                &mut recorder(&mut calls)
            ),
            PageStep::To(Mode::AgentPanel { sel: codex_row }),
            "back to the row it was opened from"
        );
        assert_eq!(slot, None);
        assert!(calls.is_empty());
    }

    #[test]
    fn answering_the_pause_from_its_page_resumes_the_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let claude = clank_core::AgentLabel::parse("claude").unwrap();
        crate::cli::block::ask(repo, &claude, PAUSE, "held").unwrap();
        let mut s = two_agent_snap();
        s.blocks = crate::cli::block::scan_blocks(repo);
        assert!(repo_paused(&s.blocks));
        let mut slot = opened_from_log(&s, 0);
        let mut keys = typed("go");
        keys.push(AnyKey::Text(TextKey::Enter));
        let mut store = |a: &Ask, t: &str| answer_in_store(repo, a, t);
        assert_eq!(
            press(&mut slot, &s, &keys, &mut store),
            PageStep::Answered(Mode::LogScroll)
        );
        assert!(!repo_paused(&crate::cli::block::scan_blocks(repo)));
    }
}
