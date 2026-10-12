//! What an agent's input line holds, read from its pane's screen.
//!
//! A veto only: clank types into a pane only when this reads the input as
//! empty, and anything it cannot place or read is left alone. It is not a
//! claim that the input stays empty; someone can type after it is read.

use clank_core::Tool;

/// The input, as the screen shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Composer {
    /// Nothing typed: no text, or only the harness's own placeholder.
    Empty,
    /// Something a person typed.
    Draft,
    /// The input could not be found, bounded or read.
    Unreadable,
}

/// The glyph each harness draws at the start of its input.
fn prompt(tool: Tool) -> Option<char> {
    match tool {
        Tool::Claude => Some('❯'),
        Tool::Codex => Some('›'),
        Tool::Grok | Tool::OpenCode => None,
    }
}

/// The placeholders a harness draws, dim, in an empty input. Dim text that
/// is not one of these is not known to be a placeholder.
fn placeholder(tool: Tool, text: &str) -> bool {
    match tool {
        Tool::Claude => text.starts_with("Try \""),
        Tool::Codex => [
            "Ask Codex to do anything",
            "Explain this codebase",
            "Summarize recent commits",
            "Implement {feature}",
            "Find and fix a bug in @filename",
            "Write tests for @filename",
            "Improve documentation in @filename",
            "Run /review on my current changes",
            "Use /skills to list available skills",
        ]
        .contains(&text),
        Tool::Grok | Tool::OpenCode => false,
    }
}

/// One screen character, whether it was drawn dim, and its background.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    c: char,
    dim: bool,
    bg: Option<String>,
}

/// Read `tool`'s input from `screen`, a dump of its pane with its colours
/// kept. The input is the last line opening with the harness's prompt and
/// every line it spans, blank ones included: Claude's between its two
/// rules, Codex's the band of background it is drawn on. An input that
/// cannot be bounded that way, or with nothing drawn under it, is
/// unreadable.
pub(crate) fn composer(tool: Tool, screen: &str) -> Composer {
    let Some(glyph) = prompt(tool) else {
        return Composer::Unreadable;
    };
    let Some(lines) = screen.lines().map(cells).collect::<Option<Vec<_>>>() else {
        return Composer::Unreadable;
    };
    let text = |l: &[Cell]| l.iter().map(|c| c.c).collect::<String>();
    let Some(start) = lines
        .iter()
        .rposition(|l| text(l).trim_start().starts_with(glyph))
    else {
        return Composer::Unreadable;
    };
    let first = &lines[start];
    let at = first.iter().position(|c| c.c == glyph).expect("found");
    let rule = |l: &[Cell]| text(l).trim().starts_with("───");
    let band = first[at].bg.clone();
    let on_band = |l: &[Cell]| !l.is_empty() && band.is_some() && l.iter().all(|c| c.bg == band);
    let (above, spans) = match tool {
        // Claude's upper rule is sometimes labelled; its lower one is not
        // assumed to be.
        Tool::Claude => (
            start.checked_sub(1).is_some_and(|i| rule(&lines[i])),
            lines[start + 1..].iter().position(|l| rule(l)),
        ),
        _ => (
            start.checked_sub(1).is_some_and(|i| on_band(&lines[i]))
                && first[..at].iter().all(|c| c.bg == band),
            lines[start + 1..].iter().position(|l| !on_band(l)),
        ),
    };
    // Without what is drawn above and below it, a prompt is history, or
    // the input is covered or cut off.
    let Some(spans) = spans.filter(|_| above) else {
        return Composer::Unreadable;
    };
    let rest = &lines[start + 1..start + 1 + spans];
    let typed: Vec<&Cell> = first[at + 1..]
        .iter()
        .chain(rest.iter().flatten())
        .filter(|c| !c.c.is_whitespace())
        .collect();
    if typed.is_empty() {
        return Composer::Empty;
    }
    if typed.iter().any(|c| !c.dim) {
        return Composer::Draft;
    }
    let continued = rest.iter().any(|l| !text(l).trim().is_empty());
    if !continued && placeholder(tool, text(&first[at + 1..]).trim()) {
        Composer::Empty
    } else {
        Composer::Unreadable
    }
}

/// `line`'s characters with their dimness and background, or `None` when
/// it holds an escape this cannot follow.
fn cells(line: &str) -> Option<Vec<Cell>> {
    let mut out = Vec::new();
    let mut dim = false;
    let mut bg: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(Cell {
                c,
                dim,
                bg: bg.clone(),
            });
            continue;
        }
        if chars.next() != Some('[') {
            return None;
        }
        let mut params = String::new();
        loop {
            match chars.next()? {
                'm' => break,
                p if p.is_ascii_digit() || p == ';' => params.push(p),
                _ => return None,
            }
        }
        let mut params = params.split(';');
        while let Some(p) = params.next() {
            match p {
                "" | "0" => {
                    dim = false;
                    bg = None;
                }
                "2" => dim = true,
                "22" => dim = false,
                "49" => bg = None,
                // A colour's own numbers, `5;n` or `2;r;g;b`, are not
                // attributes: a `2` among them is not dim.
                "38" | "48" | "58" => {
                    let colour = match params.next() {
                        Some("5") => params.next().map(|n| format!("5;{n}")),
                        Some("2") => {
                            let rgb: Vec<&str> = params.by_ref().take(3).collect();
                            (rgb.len() == 3).then(|| format!("2;{}", rgb.join(";")))
                        }
                        _ => None,
                    }?;
                    if p == "48" {
                        bg = Some(colour);
                    }
                }
                p if p.len() == 2 && p.starts_with('4') => bg = Some(p.to_string()),
                _ => {}
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIM: &str = "\u{1b}[2m";
    const RESET: &str = "\u{1b}[0m";
    const RULE: &str = "────────────────────────────────";

    /// Claude's input between its two rules, under some history that
    /// echoes earlier prompts with the same glyph.
    fn claude(input: &[&str]) -> String {
        format!(
            "❯ an old prompt\n● done\n\n{RULE}\n{}\n{RULE}\n  {DIM}? for shortcuts{RESET}",
            input.join("\n")
        )
    }

    const BAND: &str = "\u{1b}[48;2;65;69;76m";

    /// Codex's input on its band, a blank band row above and below it,
    /// under some history, and its footer off the band.
    fn codex(input: &[&str]) -> String {
        let banded = |l: &str| format!("{BAND}{l}{BAND}   \u{1b}[49m");
        let rows: Vec<String> = std::iter::once("")
            .chain(input.iter().copied())
            .chain(std::iter::once(""))
            .map(banded)
            .collect();
        format!(
            "› an old prompt\n• done\n\n{}\n\u{1b}[m  {DIM}100% context left · ? for shortcuts{RESET}",
            rows.join("\n")
        )
    }

    #[test]
    fn an_empty_input_or_a_known_placeholder_is_empty() {
        assert_eq!(composer(Tool::Claude, &claude(&["❯ "])), Composer::Empty);
        assert_eq!(
            composer(
                Tool::Claude,
                &claude(&[&format!("❯ {DIM}Try \"fix lint errors\"{RESET}")])
            ),
            Composer::Empty
        );
        assert_eq!(composer(Tool::Codex, &codex(&["› "])), Composer::Empty);
        assert_eq!(
            composer(
                Tool::Codex,
                &codex(&[&format!("› {DIM}Ask Codex to do anything{RESET}")])
            ),
            Composer::Empty
        );
    }

    /// A draft on the prompt line, on a continuation line under a blank
    /// first line, or wrapped over several lines, is a draft.
    #[test]
    fn a_draft_anywhere_in_the_input_is_a_draft() {
        for input in [
            &["❯ half a thought"][..],
            &["❯ ", "  unfinished draft"],
            &["❯ a long line that the terminal", "  wrapped onto a second"],
        ] {
            assert_eq!(
                composer(Tool::Claude, &claude(input)),
                Composer::Draft,
                "{input:?}"
            );
        }
        for input in [
            &["› half a thought"][..],
            &["› ", "  unfinished draft"],
            &["› a long line that the terminal", "  wrapped onto a second"],
        ] {
            assert_eq!(
                composer(Tool::Codex, &codex(input)),
                Composer::Draft,
                "{input:?}"
            );
        }
    }

    /// Dim text is a placeholder only when it is a known one: anything
    /// else dim, or dim text over more lines, cannot be read as empty.
    #[test]
    fn dim_text_alone_is_not_a_placeholder() {
        let dim = |t: &str| format!("❯ {DIM}{t}{RESET}");
        assert_eq!(
            composer(Tool::Claude, &claude(&[&dim("a styled draft")])),
            Composer::Unreadable
        );
        assert_eq!(
            composer(
                Tool::Claude,
                &claude(&[&dim("Try \"x\""), &format!("  {DIM}more{RESET}")])
            ),
            Composer::Unreadable
        );
        assert_eq!(
            composer(
                Tool::Codex,
                &codex(&[&format!("› {DIM}a styled draft{RESET}")])
            ),
            Composer::Unreadable
        );
    }

    /// The lines Claude Code 2.1.280 and Codex drew around an empty input,
    /// as `zellij action dump-screen --ansi` captured them: Claude's
    /// labelled upper rule and its prompt followed by a no-break space;
    /// Codex's input on a band, its truecolour SGR carrying a bare `2`.
    #[test]
    fn captured_empty_inputs_are_empty() {
        let attrs = "\u{1b}[49m\u{1b}[29m\u{1b}[28m\u{1b}[27m\u{1b}[25m\u{1b}[25m";
        let rule = |label: &str| {
            format!(
                "\u{1b}[38;2;136;136;136m{attrs}\u{1b}[22m\u{1b}[24m\u{1b}[22m\u{1b}[23m{RULE}{label}"
            )
        };
        let claude = format!(
            "{}\n\u{1b}[m❯\u{a0}\n{}\n\u{1b}[m  ⏵⏵ auto mode on",
            rule(" repo · claude ─"),
            rule("")
        );
        assert_eq!(composer(Tool::Claude, &claude), Composer::Empty);
        assert_eq!(
            composer(Tool::Claude, &claude.replace('\u{a0}', "\u{a0}draft")),
            Composer::Draft
        );
        let band =
            "\u{1b}[39m\u{1b}[48;2;65;69;76m\u{1b}[29m\u{1b}[28m\u{1b}[27m\u{1b}[25m\u{1b}[25m";
        let blank = format!(
            "{band}\u{1b}[22m\u{1b}[24m\u{1b}[22m\u{1b}[23m{}",
            " ".repeat(40)
        );
        let codex = format!(
            "\n{blank}\n{band}\u{1b}[1m\u{1b}[24m\u{1b}[22m\u{1b}[1m\u{1b}[23m›{band}\u{1b}[22m\u{1b}[24m\u{1b}[22m\u{1b}[23m {band}\u{1b}[22m\u{1b}[24m\u{1b}[2m\u{1b}[23mAsk Codex to do anything{band}\u{1b}[22m\u{1b}[24m\u{1b}[22m\u{1b}[23m   \n{blank}\n\u{1b}[m  GPT high · ~/src"
        );
        assert_eq!(composer(Tool::Codex, &codex), Composer::Empty);
        assert_eq!(
            composer(
                Tool::Codex,
                &codex.replace("Ask Codex to do anything", "Ask Codex x")
            ),
            Composer::Unreadable,
            "dim, but not a known placeholder"
        );
    }

    /// A colour's own numbers are not attributes: the `2` in a truecolour
    /// or 256-colour SGR does not make text dim.
    #[test]
    fn a_colour_is_not_dimness() {
        let dim_of = |s: &str| cells(s).unwrap().iter().map(|c| c.dim).collect::<Vec<_>>();
        assert_eq!(dim_of("\u{1b}[38;2;1;2;3mx"), [false]);
        assert_eq!(dim_of("\u{1b}[48;5;2mx"), [false]);
        assert_eq!(dim_of("\u{1b}[38;2;9;9;9;2mx"), [true]);
    }

    /// Blank rows inside the input are part of it, wherever they fall: a
    /// draft before, between or after them is still a draft.
    #[test]
    fn blank_rows_do_not_end_the_input() {
        for input in [
            &["❯ ", "", "  unfinished draft"][..],
            &["❯ first", "", "  second"],
            &["❯ ", "  draft", ""],
        ] {
            assert_eq!(
                composer(Tool::Claude, &claude(input)),
                Composer::Draft,
                "{input:?}"
            );
        }
        for input in [
            &["› ", "", "  unfinished draft"][..],
            &["› first", "", "  second"],
            &["› ", "  draft", ""],
        ] {
            assert_eq!(
                composer(Tool::Codex, &codex(input)),
                Composer::Draft,
                "{input:?}"
            );
        }
        assert_eq!(
            composer(Tool::Codex, &codex(&["› ", "", ""])),
            Composer::Empty
        );
    }

    /// A Codex prompt off any band, or whose band runs to the end of the
    /// screen, is not an input this can bound.
    #[test]
    fn a_codex_prompt_without_its_band_is_unreadable() {
        let history_only = "› an old prompt\n• done\n\n  footer";
        assert_eq!(composer(Tool::Codex, history_only), Composer::Unreadable);
        let cut = format!("{BAND}   \n{BAND}› \n{BAND}  more");
        assert_eq!(composer(Tool::Codex, &cut), Composer::Unreadable);
    }

    /// An input that cannot be placed or bounded is left alone: covered by
    /// a dialog, with no rule under it, a prompt that is only history, a
    /// harness without one, or an escape this cannot follow.
    #[test]
    fn an_input_that_cannot_be_placed_is_unreadable() {
        let covered = "❯ an old prompt\n● done\n╭ Allow this tool? ╮\n│ 1. Yes │\n╰────╯";
        assert_eq!(composer(Tool::Claude, covered), Composer::Unreadable);
        let unbounded = format!("{RULE}\n❯ \n  still going");
        assert_eq!(composer(Tool::Claude, &unbounded), Composer::Unreadable);
        assert_eq!(
            composer(Tool::Codex, "› \n  never ends"),
            Composer::Unreadable
        );
        assert_eq!(composer(Tool::Grok, &claude(&["❯ "])), Composer::Unreadable);
        assert_eq!(
            composer(Tool::Claude, &claude(&["❯ \u{1b}]8;;x"])),
            Composer::Unreadable
        );
    }
}
