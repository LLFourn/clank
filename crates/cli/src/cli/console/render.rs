//! Frame compositing for the console. Each repaint writes only the
//! delta to what is physically on screen, so a chatty agent doesn't
//! flicker:
//!
//! - the content region is a `vt100` `contents_diff` of the active
//!   grid against a `painted` mirror of what's already there — which
//!   stays correct even across a screen switch (the diff transforms
//!   the old screen straight into the new one);
//! - the chrome bar is rewritten only when its string changes;
//! - the real cursor is synced to the active grid's cursor last, so
//!   the agent's input caret lands in the right place.

use std::io::Write as _;

use super::mux::{self, Tab};

/// Tracks what is physically on the terminal so each frame writes
/// only the difference. `painted` mirrors the content region;
/// `chrome` is the last bar string.
pub(crate) struct Frame {
    painted: vt100::Parser,
    chrome: String,
    rows: u16,
    cols: u16,
}

impl Frame {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let frame = Self::blank(rows, cols);
        clear_screen();
        frame
    }

    fn blank(rows: u16, cols: u16) -> Self {
        let (crows, ccols) = mux::content_rect(rows, cols);
        Frame {
            painted: vt100::Parser::new(crows, ccols, 0),
            chrome: String::new(),
            rows,
            cols,
        }
    }

    /// Adopt a new terminal size: reset the mirror to a fresh blank
    /// grid of the new content size and clear the physical screen so
    /// the next `draw` repaints in full.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        *self = Self::blank(rows, cols);
        clear_screen();
    }

    /// Repaint from the active screen's grid plus the tab strip. The
    /// `hint` is the dim right-aligned keybinding reminder.
    pub(crate) fn draw(
        &mut self,
        active: &vt100::Screen,
        tabs: &[Tab],
        active_idx: usize,
        hint: &str,
    ) {
        let mut buf: Vec<u8> = Vec::new();

        // 1. Content region: the minimal diff from what's there now.
        // Feed the SAME diff back into the mirror so it tracks the
        // physical screen without cloning the whole grid each frame.
        let diff = active.contents_diff(self.painted.screen());
        buf.extend_from_slice(&diff);
        self.painted.process(&diff);

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

        let mut out = std::io::stdout();
        let _ = out.write_all(&buf);
        let _ = out.flush();
    }
}

fn clear_screen() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[2J");
    let _ = out.flush();
}
