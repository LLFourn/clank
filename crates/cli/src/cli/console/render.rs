//! Frame compositing for the console. The terminal is split into two
//! live panes — the active agent and the always-on status pane (see
//! [`mux::layout`]) — plus the chrome bar. Each pane is a `vt100`
//! grid composited into its own sub-rectangle.
//!
//! We can't use vt100's own `contents_formatted` / `rows_formatted`
//! for a sub-rect: they emit `\x1b[K` (clears to the PHYSICAL end of
//! line, clobbering a right-hand neighbour) and absolute grid-coord
//! cursor moves. So we build each pane row cell-by-cell ourselves —
//! one glyph per column, spaces for blanks (no `\x1b[K`), positioned
//! absolutely at the rect — and diff per row so an idle pane costs
//! nothing. The cursor is synced last to whichever pane has focus.

use std::io::Write as _;

use super::mux::{self, Rect, Tab};

/// What occupies the main (agent) pane this frame.
pub(crate) enum MainPane<'a> {
    /// A live agent grid.
    Live(&'a vt100::Screen),
    /// A crashed agent — show a centered respawn prompt instead.
    Dead(&'a str),
}

/// The bottom bar's inputs: the agent tabs, which one is active, the
/// focus + follow state, and the dim keybinding hint.
pub(crate) struct ChromeBar<'a> {
    pub(crate) tabs: &'a [Tab],
    pub(crate) active: usize,
    /// Status pane (not an agent) has focus — so no agent tab bands.
    pub(crate) status_focused: bool,
    /// Follow-active mode is on.
    pub(crate) follow: bool,
    pub(crate) hint: &'a str,
}

/// Tracks what is physically on the terminal so each frame writes
/// only the delta. The `*_rows` vecs are the formatted rows last
/// drawn into each pane (cleared on resize, forcing a full repaint).
pub(crate) struct Frame {
    main_rows: Vec<String>,
    status_rows: Vec<String>,
    divider_rows: Vec<String>,
    chrome: String,
    cols: u16,
}

impl Frame {
    pub(crate) fn new(_rows: u16, cols: u16) -> Self {
        clear_screen();
        Frame {
            main_rows: Vec::new(),
            status_rows: Vec::new(),
            divider_rows: Vec::new(),
            chrome: String::new(),
            cols,
        }
    }

    /// Adopt a new terminal size: drop the per-pane caches and clear
    /// the physical screen so the next `draw` repaints in full.
    pub(crate) fn resize(&mut self, _rows: u16, cols: u16) {
        self.main_rows.clear();
        self.status_rows.clear();
        self.divider_rows.clear();
        self.chrome.clear();
        self.cols = cols;
        clear_screen();
    }

    /// Composite the agent pane, the status pane, and the chrome bar,
    /// then sync the cursor into the focused pane. Returns the bytes
    /// for the caller to write (IO stays in the loop).
    pub(crate) fn draw(
        &mut self,
        main: MainPane,
        status: &vt100::Screen,
        layout: mux::Layout,
        cursor_in_status: bool,
        chrome: ChromeBar,
    ) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();

        // Agent pane.
        match main {
            MainPane::Live(grid) => blit(&mut buf, &mut self.main_rows, grid, layout.main),
            MainPane::Dead(label) => blit_rows(
                &mut buf,
                &mut self.main_rows,
                dead_pane(label, layout.main),
                layout.main,
            ),
        }
        // Status pane.
        blit(&mut buf, &mut self.status_rows, status, layout.status);

        // Divider between the panes — its colour is the focus cue:
        // accent when status is focused, dim otherwise.
        let div = divider_rows(layout, cursor_in_status);
        blit_rows(&mut buf, &mut self.divider_rows, div, layout.divider);

        // Chrome bar — only when it changed. The agent tab band shows
        // only when an agent (not status) is focused.
        let chrome = mux::chrome_line(
            chrome.tabs,
            chrome.active,
            chrome.status_focused,
            chrome.follow,
            chrome.hint,
            self.cols as usize,
        );
        if chrome != self.chrome {
            buf.extend_from_slice(format!("\x1b[{};1H", layout.chrome_row + 1).as_bytes());
            buf.extend_from_slice(chrome.as_bytes());
            self.chrome = chrome;
        }

        // Cursor into the focused pane, mapped to that rect's origin.
        let (grid, rect) = match (cursor_in_status, &main) {
            (false, MainPane::Live(g)) => (Some(*g), layout.main),
            (true, _) => (Some(status), layout.status),
            (false, MainPane::Dead(_)) => (None, layout.main),
        };
        match grid {
            Some(g) if !g.hide_cursor() => {
                let (r, c) = g.cursor_position();
                buf.extend_from_slice(
                    format!("\x1b[{};{}H\x1b[?25h", rect.y + r + 1, rect.x + c + 1).as_bytes(),
                );
            }
            _ => buf.extend_from_slice(b"\x1b[?25l"),
        }
        buf
    }
}

/// Composite a grid into `rect`, writing only the rows that changed
/// since the last frame.
fn blit(buf: &mut Vec<u8>, last: &mut Vec<String>, grid: &vt100::Screen, rect: Rect) {
    let rows = (0..rect.rows)
        .map(|r| region_row(grid, r, rect.cols))
        .collect::<Vec<_>>();
    blit_rows(buf, last, rows, rect);
}

/// Write `rows` into `rect`, diffing per row against `last`. Each row
/// is positioned absolutely and is exactly `rect.cols` wide (spaces
/// for blanks), so it overwrites stale content without `\x1b[K`.
fn blit_rows(buf: &mut Vec<u8>, last: &mut Vec<String>, rows: Vec<String>, rect: Rect) {
    for (i, row) in rows.iter().enumerate() {
        if last.get(i).map(String::as_str) != Some(row.as_str()) {
            buf.extend_from_slice(
                format!("\x1b[{};{}H", rect.y as usize + i + 1, rect.x + 1).as_bytes(),
            );
            buf.extend_from_slice(row.as_bytes());
        }
    }
    *last = rows;
}

/// Build one pane row as a self-contained string: a glyph per column
/// (space for a blank), SGR emitted only when it changes, reset at
/// the end. Exactly `cols` visible columns wide. No `\x1b[K`, no
/// absolute moves — safe to drop at any (x, y).
fn region_row(grid: &vt100::Screen, row: u16, cols: u16) -> String {
    let mut out = String::new();
    let mut cur_sgr = String::new();
    let mut col = 0u16;
    while col < cols {
        let cell = grid.cell(row, col);
        if let Some(c) = cell {
            if c.is_wide_continuation() {
                col += 1;
                continue;
            }
            let sgr = cell_sgr(c);
            if sgr != cur_sgr {
                out.push('\x1b');
                out.push('[');
                out.push_str(&sgr);
                out.push('m');
                cur_sgr = sgr;
            }
            if c.has_contents() {
                out.push_str(c.contents());
            } else {
                out.push(' ');
            }
        } else {
            if !cur_sgr.is_empty() && cur_sgr != "0" {
                out.push_str("\x1b[0m");
                cur_sgr = "0".to_string();
            }
            out.push(' ');
        }
        col += 1;
    }
    out.push_str("\x1b[0m");
    out
}

/// SGR parameter string for a cell (always reset-prefixed, so each
/// emit fully sets the style).
fn cell_sgr(c: &vt100::Cell) -> String {
    let mut p = vec!["0".to_string()];
    if c.bold() {
        p.push("1".into());
    }
    if c.dim() {
        p.push("2".into());
    }
    if c.italic() {
        p.push("3".into());
    }
    if c.underline() {
        p.push("4".into());
    }
    if c.inverse() {
        p.push("7".into());
    }
    push_color(&mut p, c.fgcolor(), true);
    push_color(&mut p, c.bgcolor(), false);
    p.join(";")
}

fn push_color(p: &mut Vec<String>, color: vt100::Color, fg: bool) {
    let base: u16 = if fg { 30 } else { 40 };
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => p.push((base + u16::from(i)).to_string()),
        vt100::Color::Idx(i) if i < 16 => p.push((base + 60 + u16::from(i - 8)).to_string()),
        vt100::Color::Idx(i) => {
            p.push(if fg { "38" } else { "48" }.into());
            p.push("5".into());
            p.push(i.to_string());
        }
        vt100::Color::Rgb(r, g, b) => {
            p.push(if fg { "38" } else { "48" }.into());
            p.push("2".into());
            p.push(r.to_string());
            p.push(g.to_string());
            p.push(b.to_string());
        }
    }
}

/// The divider rule between the panes, as blit rows. Its colour is
/// the focus cue: an accent (cyan) when the status pane is focused,
/// dim otherwise. Landscape draws a vertical `│` per row; portrait a
/// horizontal `─` rule with a centered `STATUS` label.
fn divider_rows(layout: mux::Layout, status_focused: bool) -> Vec<String> {
    let style = if status_focused {
        "\x1b[36m"
    } else {
        "\x1b[2m"
    };
    let d = layout.divider;
    if layout.landscape {
        (0..d.rows).map(|_| format!("{style}│\x1b[0m")).collect()
    } else {
        let cols = d.cols as usize;
        let label = " STATUS ";
        let line = if cols >= label.chars().count() + 2 {
            let dash = cols - label.chars().count();
            let left = dash / 2;
            format!(
                "{style}{}{label}{}\x1b[0m",
                "─".repeat(left),
                "─".repeat(dash - left)
            )
        } else {
            format!("{style}{}\x1b[0m", "─".repeat(cols))
        };
        vec![line]
    }
}

/// The crashed-agent pane: a centered, dim "press Enter to respawn"
/// prompt, the rest blank. Same row-string shape as a composited
/// grid so it diffs/blits identically.
fn dead_pane(label: &str, rect: Rect) -> Vec<String> {
    let cols = rect.cols as usize;
    let msg = format!("{label} exited · press Enter to respawn");
    let msg: String = msg.chars().take(cols).collect();
    let mid = (rect.rows / 2) as usize;
    (0..rect.rows as usize)
        .map(|r| {
            if r == mid {
                let pad = (cols - msg.chars().count()) / 2;
                let mut line = " ".repeat(pad);
                line.push_str("\x1b[2m");
                line.push_str(&msg);
                line.push_str("\x1b[0m");
                let tail = cols - pad - msg.chars().count();
                line.push_str(&" ".repeat(tail));
                line
            } else {
                " ".repeat(cols)
            }
        })
        .collect()
}

fn clear_screen() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[2J");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Screen {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(bytes);
        p.screen().clone()
    }

    fn tab(label: &str, working: bool) -> Tab {
        Tab {
            label: label.into(),
            alive: true,
            working,
        }
    }

    fn bar(tabs: &[Tab], active: usize) -> ChromeBar<'_> {
        ChromeBar {
            tabs,
            active,
            status_focused: false,
            follow: false,
            hint: "",
        }
    }

    fn row_text(s: &vt100::Screen, row: u16, cols: u16) -> String {
        (0..cols)
            .map(|c| s.cell(row, c).map(|c| c.contents()).unwrap_or_default())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The divider rule renders between the panes, in the divider's
    /// own column — so the status pane's edge is visible.
    #[test]
    fn divider_renders_between_the_panes() {
        let (rows, cols) = (24u16, 80u16);
        let layout = mux::layout(rows, cols);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);
        let agent = feed(layout.main.rows, layout.main.cols, b"x");
        let status = feed(layout.status.rows, layout.status.cols, b"y");
        let tabs = [tab("a", false)];

        physical.process(&frame.draw(
            MainPane::Live(&agent),
            &status,
            layout,
            false,
            bar(&tabs, 0),
        ));

        // A `│` sits in the divider column on a content row.
        let cell = physical
            .screen()
            .cell(0, layout.divider.x)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "│", "divider at x={}", layout.divider.x);
    }

    /// The load-bearing property: two panes side by side must not
    /// bleed into each other. Render an agent on the left and status
    /// on the right, feed the frame to a physical vt100 parser, and
    /// assert each pane shows its OWN content at its own columns.
    #[test]
    fn side_by_side_panes_do_not_bleed() {
        let (rows, cols) = (24u16, 80u16);
        let layout = mux::layout(rows, cols);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);

        let agent = feed(layout.main.rows, layout.main.cols, b"AGENT-LEFT");
        let status = feed(layout.status.rows, layout.status.cols, b"STATUS-RIGHT");
        let tabs = [tab("claude", false)];

        let bytes = frame.draw(
            MainPane::Live(&agent),
            &status,
            layout,
            false,
            bar(&tabs, 0),
        );
        physical.process(&bytes);

        // Agent text is in the main pane's first row.
        assert!(
            row_text(physical.screen(), 0, cols).starts_with("AGENT-LEFT"),
            "main pane row: {:?}",
            row_text(physical.screen(), 0, cols)
        );
        // Status text starts exactly at the status pane's x — and the
        // agent text never reached that far.
        let sx = layout.status.x;
        let status_cell: String = (0..layout.status.cols.min(12))
            .map(|c| {
                physical
                    .screen()
                    .cell(0, sx + c)
                    .map(|c| c.contents())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(status_cell, "STATUS-RIGHT", "status pane at x={sx}");
    }

    /// Switching the agent pane clears the previous agent's content
    /// (the bug from the MVP), and never touches the status pane.
    #[test]
    fn switching_agent_pane_clears_and_preserves_status() {
        let (rows, cols) = (24u16, 80u16);
        let layout = mux::layout(rows, cols);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);
        let status = feed(layout.status.rows, layout.status.cols, b"STATUS");
        let tabs = [tab("a", false), tab("b", false)];

        let tall = feed(layout.main.rows, layout.main.cols, b"AAA\r\nBBB");
        physical.process(&frame.draw(MainPane::Live(&tall), &status, layout, false, bar(&tabs, 0)));
        assert_eq!(row_text(physical.screen(), 1, layout.main.cols), "BBB");

        let short = feed(layout.main.rows, layout.main.cols, b"Z");
        physical.process(&frame.draw(
            MainPane::Live(&short),
            &status,
            layout,
            false,
            bar(&tabs, 1),
        ));
        assert_eq!(row_text(physical.screen(), 0, layout.main.cols), "Z");
        assert_eq!(
            row_text(physical.screen(), 1, layout.main.cols),
            "",
            "previous agent content cleared"
        );
        // Status pane survived untouched.
        let sx = layout.status.x;
        let status_cell: String = (0..6)
            .map(|c| {
                physical
                    .screen()
                    .cell(0, sx + c)
                    .map(|c| c.contents())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(status_cell, "STATUS");
    }

    #[test]
    fn dead_agent_pane_shows_respawn_prompt() {
        let (rows, cols) = (24u16, 80u16);
        let layout = mux::layout(rows, cols);
        let mut frame = Frame::new(rows, cols);
        let mut physical = vt100::Parser::new(rows, cols, 0);
        let status = feed(layout.status.rows, layout.status.cols, b"STATUS");
        let tabs = [tab("codex", false)];

        physical.process(&frame.draw(
            MainPane::Dead("codex"),
            &status,
            layout,
            false,
            bar(&tabs, 0),
        ));

        let shown: String = (0..layout.main.rows)
            .map(|r| row_text(physical.screen(), r, layout.main.cols))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            shown.contains("press Enter to respawn"),
            "dead pane prompt: {shown:?}"
        );
    }
}
