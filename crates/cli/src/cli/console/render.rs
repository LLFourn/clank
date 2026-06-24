//! Frame compositing for the console. Each repaint writes only the
//! delta to what is physically on screen, so a chatty agent doesn't
//! flicker:
//!
//! - the content region is a `vt100` `contents_diff` of the active
//!   grid against `painted`, an EXACT CLONE of the screen we last
//!   drew. Cloning (not feeding the diff back into a mirror parser)
//!   is what keeps the baseline truthful across a switch: the diff
//!   then transforms the OLD screen's grid straight into the new
//!   one, clearing every cell the new screen leaves blank. A
//!   reconstructed mirror drifts and leaves stale content behind.
//! - the chrome bar is rewritten only when its string changes;
//! - the real cursor is synced to the active grid's cursor last, so
//!   the agent's input caret lands in the right place.

use std::io::Write as _;

use super::mux::{self, Tab};

/// Tracks what is physically on the terminal so each frame writes
/// only the difference. `painted` is an exact clone of the content
/// region last drawn (`None` before the first paint / after a
/// resize, when the next draw repaints in full); `chrome` is the
/// last bar string.
pub(crate) struct Frame {
    painted: Option<vt100::Screen>,
    chrome: String,
    rows: u16,
    cols: u16,
}

impl Frame {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        clear_screen();
        Frame {
            painted: None,
            chrome: String::new(),
            rows,
            cols,
        }
    }

    /// Adopt a new terminal size: drop the mirror and clear the
    /// physical screen so the next `draw` repaints in full.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        self.painted = None;
        self.chrome.clear();
        self.rows = rows;
        self.cols = cols;
        clear_screen();
    }

    /// Compute the frame's bytes (content delta + chrome + cursor)
    /// and advance the mirror. Returns the bytes for the caller to
    /// write — keeping the terminal IO in the loop and this logic
    /// testable. The `hint` is the dim right-aligned keybinding
    /// reminder.
    pub(crate) fn draw(
        &mut self,
        active: &vt100::Screen,
        tabs: &[Tab],
        active_idx: usize,
        hint: &str,
    ) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();

        // 1. Content region. Against the exact previous frame, a diff
        // transforms it into `active` (clearing vacated cells); with
        // no previous frame, paint it in full. `contents_formatted`
        // self-clears, so the first paint after a resize is clean.
        match &self.painted {
            Some(prev) => buf.extend_from_slice(&active.contents_diff(prev)),
            None => buf.extend_from_slice(&active.contents_formatted()),
        }
        self.painted = Some(active.clone());

        // 2. Chrome bar at the bottom row — only when it changed.
        let chrome = mux::chrome_line(tabs, active_idx, hint, self.cols as usize);
        if chrome != self.chrome {
            buf.extend_from_slice(format!("\x1b[{};1H", self.rows).as_bytes());
            buf.extend_from_slice(chrome.as_bytes());
            self.chrome = chrome;
        }

        // 3. Sync the real cursor to the active grid's cursor LAST
        // (after the chrome write moved it). Grid coords are 0-based
        // from the content region's top-left, which is the terminal's
        // top-left, so +1 for the 1-based CUP. The grid is sized to
        // exclude the chrome row, so the caret can never land on it.
        if active.hide_cursor() {
            buf.extend_from_slice(b"\x1b[?25l");
        } else {
            let (r, c) = active.cursor_position();
            buf.extend_from_slice(format!("\x1b[{};{}H\x1b[?25h", r + 1, c + 1).as_bytes());
        }

        buf
    }

    /// Paint a dead screen: clear the content region and show a
    /// centered "press Enter to respawn" prompt. The respawn
    /// affordance lives IN the dead terminal (not a hidden hotkey),
    /// which is the whole point — a crashed agent is obvious and
    /// recoverable with one keystroke. Drops the mirror so the next
    /// live draw (after respawn) repaints in full.
    pub(crate) fn draw_dead(
        &mut self,
        label: &str,
        tabs: &[Tab],
        active_idx: usize,
        hint: &str,
    ) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"\x1b[2J");
        self.painted = None;

        let (crows, ccols) = mux::content_rect(self.rows, self.cols);
        let msg = format!("{label} exited · press Enter to respawn");
        let w = msg.chars().count().min(ccols as usize);
        let msg: String = msg.chars().take(w).collect();
        let row = (crows / 2).max(1);
        let col = ((ccols as usize - w) / 2) + 1;
        buf.extend_from_slice(format!("\x1b[{row};{col}H\x1b[2m{msg}\x1b[0m").as_bytes());

        // Chrome was wiped by the clear; force a redraw.
        let chrome = mux::chrome_line(tabs, active_idx, hint, self.cols as usize);
        buf.extend_from_slice(format!("\x1b[{};1H", self.rows).as_bytes());
        buf.extend_from_slice(chrome.as_bytes());
        self.chrome = chrome;

        buf.extend_from_slice(b"\x1b[?25l"); // no input caret on a dead screen
        buf
    }
}

fn clear_screen() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[2J");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a content-region screen of `rows`x`cols` from the bytes
    /// a child would have written.
    fn screen(rows: u16, cols: u16, feed: &[u8]) -> vt100::Screen {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(feed);
        p.screen().clone()
    }

    fn row_text(s: &vt100::Screen, row: u16, cols: u16) -> String {
        (0..cols)
            .map(|c| {
                s.cell(row, c)
                    .map(|cell| cell.contents())
                    .unwrap_or_default()
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The bug lloyd hit: switching tabs must clear the previous
    /// screen's content. We simulate the physical terminal with a
    /// vt100 parser, feed it each frame's bytes, and assert that
    /// after switching to a shorter screen no residue of the taller
    /// one survives.
    #[test]
    fn switching_screens_clears_the_previous_content() {
        let (rows, cols) = (4u16, 12u16); // 3 content rows + chrome
        let (crows, ccols) = mux::content_rect(rows, cols);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);

        let tab = |l: &str| Tab {
            label: l.into(),
            alive: true,
            working: false,
        };
        let tabs = [tab("a"), tab("b")];

        // Screen A fills two content rows.
        let a = screen(crows, ccols, b"AAA\r\nBBB");
        physical.process(&frame.draw(&a, &tabs, 0, ""));
        assert_eq!(row_text(physical.screen(), 0, cols), "AAA");
        assert_eq!(row_text(physical.screen(), 1, cols), "BBB");

        // Switch to screen B, which only writes row 0. Row 1's "BBB"
        // must be gone.
        let b = screen(crows, ccols, b"Z");
        physical.process(&frame.draw(&b, &tabs, 1, ""));
        assert_eq!(row_text(physical.screen(), 0, cols), "Z");
        assert_eq!(
            row_text(physical.screen(), 1, cols),
            "",
            "previous screen's content must be cleared on switch"
        );
    }

    #[test]
    fn dead_screen_shows_the_respawn_prompt() {
        let (rows, cols) = (6u16, 40u16);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);
        let tabs = [Tab {
            label: "codex".into(),
            alive: false,
            working: false,
        }];

        physical.process(&frame.draw_dead("codex", &tabs, 0, ""));

        let shown: String = (0..rows)
            .map(|r| row_text(physical.screen(), r, cols))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("press Enter to respawn"),
            "dead screen must invite respawn: {shown:?}"
        );
        assert!(shown.contains("codex"), "names the screen: {shown:?}");
    }
}
