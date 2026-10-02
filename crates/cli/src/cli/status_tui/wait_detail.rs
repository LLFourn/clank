//! Pure, safe presentation of observed argv and process facts. Command encoding
//! runs once in the inspection thread. Compact rendering touches only the two
//! visible ends; the full viewer walks only its visible window, not all argv.

use super::text::{char_width, display_width};
use crate::proc_identity::ProcToken;
use crate::proc_identity::inspect::{Inspection, ProcessState, Unavailable};
use std::ffi::OsString;
use std::fmt::Write;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

pub(super) fn safe_text(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() || unicode_width::UnicodeWidthChar::width(c) == Some(0) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug)]
pub(super) struct Command {
    pub(super) text: String,
    width: usize,
}

impl Command {
    pub(super) fn encode(args: &[OsString]) -> Arc<Self> {
        let mut text = String::new();
        for (index, arg) in args.iter().enumerate() {
            if index != 0 {
                text.push(' ');
            }
            let bytes = arg.as_bytes();
            let mut clean = String::new();
            let mut bare = !bytes.is_empty();
            for chunk in bytes.utf8_chunks() {
                bare &= chunk
                    .valid()
                    .chars()
                    .all(|c| c.is_alphanumeric() || "/._-:=@+,%".contains(c));
                clean.push_str(&safe_text(chunk.valid()));
                for byte in chunk.invalid() {
                    bare = false;
                    let _ = write!(clean, "\\x{byte:02x}");
                }
            }
            if bare {
                text.push_str(&clean);
            } else {
                text.push('"');
                text.push_str(&clean);
                text.push('"');
            }
        }
        if args.is_empty() {
            text.push_str("no arguments reported");
        }
        let width = display_width(&text);
        Arc::new(Self { text, width })
    }

    pub(super) fn from_inspection(inspection: &Inspection) -> Option<Arc<Self>> {
        match inspection {
            Inspection::Verified(details) => {
                details.argv.as_ref().ok().map(|args| Self::encode(args))
            }
            _ => None,
        }
    }

    /// Cell-aware middle elision AFTER the label's budget is subtracted.
    /// The cached width avoids rescanning a potentially 1 MiB command.
    pub(super) fn middle(&self, cols: usize) -> String {
        if self.width <= cols {
            return self.text.clone();
        }
        if cols == 0 {
            return String::new();
        }
        let head_budget = (cols - 1) / 2;
        let tail_budget = cols - 1 - head_budget;
        let mut out = String::new();
        let mut width = 0;
        for ch in self.text.chars() {
            let w = char_width(ch);
            if width + w > head_budget {
                break;
            }
            width += w;
            out.push(ch);
        }
        out.push('…');
        let mut start = self.text.len();
        width = 0;
        for (index, ch) in self.text.char_indices().rev() {
            let w = char_width(ch);
            if width + w > tail_budget {
                break;
            }
            width += w;
            start = index;
        }
        out.push_str(&self.text[start..]);
        out
    }

    /// A byte window with exact whitespace and argument escapes preserved.
    /// Unlike word wrapping, repeated spaces inside a quoted argument survive.
    pub(super) fn line(&self, start: usize, cols: usize) -> (&str, usize) {
        let start = self.boundary(start);
        let mut end = start;
        let mut width = 0;
        for (offset, ch) in self.text[start..].char_indices() {
            let w = char_width(ch);
            if width + w > cols.max(1) && end > start {
                break;
            }
            width += w;
            end = start + offset + ch.len_utf8();
            if width >= cols.max(1) {
                break;
            }
        }
        (&self.text[start..end], end)
    }

    fn boundary(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    /// Walk by at most the requested number of visible rows. Reverse walks
    /// are also bounded; no full rewrap, line index or allocation is needed.
    pub(super) fn advance(&self, offset: usize, delta: i32, cols: usize) -> usize {
        let mut offset = self.boundary(offset);
        for _ in 0..delta.unsigned_abs() {
            if delta >= 0 {
                let next = self.line(offset, cols).1;
                if next == self.text.len() || next == offset {
                    break;
                }
                offset = next;
            } else {
                if offset == 0 {
                    break;
                }
                let mut width = 0;
                let mut start = offset;
                for (index, ch) in self.text[..offset].char_indices().rev() {
                    let w = char_width(ch);
                    if width + w > cols.max(1) && start < offset {
                        break;
                    }
                    width += w;
                    start = index;
                    if width >= cols.max(1) {
                        break;
                    }
                }
                offset = start;
            }
        }
        offset
    }

    pub(super) fn tail(&self, rows: usize, cols: usize) -> usize {
        self.advance(
            self.text.len(),
            -(rows.max(1).min(i32::MAX as usize) as i32),
            cols,
        )
    }
}

pub(super) fn unavailable(why: Unavailable) -> &'static str {
    use std::io::ErrorKind;
    match why {
        Unavailable::InvalidPid => "invalid recorded PID",
        Unavailable::Unsupported => "unsupported on this platform",
        Unavailable::Malformed => "OS data could not be decoded",
        Unavailable::TooLarge => "exceeds the inspection limit",
        Unavailable::Io(ErrorKind::PermissionDenied) => "permission denied",
        Unavailable::Io(ErrorKind::NotFound) => "process no longer available",
        Unavailable::Io(_) => "OS read failed",
    }
}

pub(super) fn state(state: Result<ProcessState, Unavailable>) -> String {
    match state {
        Ok(ProcessState::Initializing) => "initializing".into(),
        Ok(ProcessState::Runnable) => "runnable".into(),
        Ok(ProcessState::Sleeping) => "sleeping".into(),
        Ok(ProcessState::UninterruptibleSleep) => "uninterruptible sleep".into(),
        Ok(ProcessState::Stopped) => "stopped".into(),
        Ok(ProcessState::TracingStop) => "tracing stop".into(),
        Ok(ProcessState::Zombie) => "zombie".into(),
        Ok(ProcessState::Dead) => "dead".into(),
        Ok(ProcessState::Idle) => "idle".into(),
        Ok(ProcessState::Parked) => "parked".into(),
        Ok(ProcessState::Other(code)) => format!("unknown OS state ({code})"),
        Err(why) => format!("unknown ({})", unavailable(why)),
    }
}

pub(super) fn birth(token: &ProcToken) -> String {
    match token {
        ProcToken::MacStart { sec, usec } if *usec < 1_000_000 => {
            let nanos = i128::from(*sec) * 1_000_000_000 + i128::from(*usec) * 1_000;
            time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
                .ok()
                .and_then(|at| {
                    at.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .unwrap_or_else(|| "unknown (birth time outside supported range)".into())
        }
        ProcToken::LinuxStart { ticks, boot } => format!(
            "{ticks} ticks since boot {} · wall-clock time unknown",
            safe_text(boot)
        ),
        _ => "unknown (unsupported birth identity)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn argv_preserves_boundaries_empty_arguments_and_repeated_spaces() {
        let command = Command::encode(&[
            "claude".into(),
            "--agent".into(),
            "reviewer".into(),
            "".into(),
            "two  spaces".into(),
            "a\"b\\c".into(),
        ]);
        assert_eq!(
            command.text,
            "claude --agent reviewer \"\" \"two  spaces\" \"a\\\"b\\\\c\""
        );
        assert_eq!(Command::encode(&[]).text, "no arguments reported");
        assert_eq!(Command::encode(&["".into()]).text, "\"\"");
    }

    #[test]
    fn terminal_controls_bidi_zero_width_and_invalid_utf8_are_visible_escapes() {
        let command = Command::encode(&[
            OsString::from_vec(b"tool\x1b]52;c;secret\x07\n\t\xff".to_vec()),
            "界\u{202e}\u{200d}\u{301}".into(),
        ]);
        assert!(
            command
                .text
                .contains("\\u{1b}]52;c;secret\\u{7}\\n\\t\\xff")
        );
        assert!(command.text.contains("界\\u{202e}\\u{200d}\\u{301}"));
        assert!(command.text.chars().all(|ch| !ch.is_control()));
        assert!(!command.text.contains('\u{202e}'));
    }

    #[test]
    fn middle_elision_uses_cells_and_keeps_discriminating_arguments() {
        let command = Command::encode(&[
            format!("/{}界/tool", "directory/".repeat(20)).into(),
            "--agent".into(),
            "reviewer".into(),
        ]);
        for cols in 0..=command.width + 2 {
            let value = command.middle(cols);
            assert!(display_width(&value) <= cols);
        }
        let short = command.middle(40);
        assert!(short.starts_with("/directory/"));
        assert!(short.ends_with("--agent reviewer"));
        assert!(short.contains('…'));
    }

    #[test]
    fn full_view_windows_reconstruct_every_byte_without_collapsing_whitespace() {
        let command = Command::encode(&[
            "/path/界/tool".into(),
            "two  spaces\nlast".into(),
            "".into(),
        ]);
        for cols in [2, 3, 8, 20, 80] {
            let mut text = String::new();
            let mut offset = 0;
            while offset < command.text.len() {
                let (line, next) = command.line(offset, cols);
                assert!(next > offset);
                assert!(display_width(line) <= cols);
                text.push_str(line);
                offset = next;
            }
            assert_eq!(text, command.text);
            let tail = command.tail(2, cols);
            assert!(command.text.is_char_boundary(tail));
            assert!(tail < command.text.len());
            assert_eq!(command.advance(tail, -100, cols), 0);
        }
    }

    #[test]
    fn large_commands_support_small_prefix_tail_and_visible_windows() {
        let command = Command::encode(&[
            format!("/{}", "x".repeat(1024 * 1024)).into(),
            "--model".into(),
            "fast".into(),
        ]);
        assert_eq!(display_width(&command.middle(40)), 40);
        assert!(command.middle(40).ends_with("--model fast"));
        let tail = command.tail(2, 20);
        assert!(command.text.len() - tail <= 40);
        assert!(command.line(tail, 20).0.len() <= 20);
    }

    #[test]
    fn birth_comes_from_the_full_checked_token_and_linux_does_not_guess_a_clock() {
        let token = ProcToken::MacStart { sec: 100, usec: 1 };
        let rendered = birth(&token);
        assert_eq!(rendered, "1970-01-01T00:01:40.000001Z");
        let parsed =
            time::OffsetDateTime::parse(&rendered, &time::format_description::well_known::Rfc3339)
                .unwrap();
        assert_eq!(parsed.unix_timestamp_nanos(), 100_000_001_000);
        assert!(
            birth(&ProcToken::MacStart {
                sec: 100,
                usec: 1_000_000
            })
            .starts_with("unknown")
        );
        let linux = birth(&ProcToken::LinuxStart {
            ticks: 12345,
            boot: "boot-id".into(),
        });
        assert!(linux.contains("12345 ticks since boot boot-id"));
        assert!(linux.contains("wall-clock time unknown"));
        assert!(birth(&ProcToken::Unrecognised).starts_with("unknown"));
    }
}
