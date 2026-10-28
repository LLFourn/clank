//! Why a command source's run ended, kept bounded and sanitized for the
//! failure event an agent is shown.
//!
//! A producer's stderr is read in fixed chunks into a fixed-size tail, so
//! nothing it writes, however long or malformed, grows the host's memory.
//! What reaches an agent or a person passes one sanitizer first.

use std::collections::VecDeque;
use std::os::unix::ffi::OsStrExt;

/// The raw stderr bytes a run keeps: its last 4 KiB.
pub(crate) const TAIL_BYTES: usize = 4 * 1024;
/// How many of those lines a diagnostic shows.
const TAIL_LINES: usize = 20;
/// The rendered diagnostic's cap, after escaping and masking.
const RENDERED_BYTES: usize = 8 * 1024;
/// The longest line echoed to a terminal.
const ECHO_LINE: usize = 1024;
const CHUNK: usize = 4 * 1024;
const MASK: &str = "[redacted]";

/// A run's last stderr bytes.
#[derive(Debug, Default)]
pub(crate) struct Tail {
    bytes: VecDeque<u8>,
    /// Older bytes were dropped to keep within [`TAIL_BYTES`].
    truncated: bool,
}

impl Tail {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let over = self.bytes.len().saturating_sub(TAIL_BYTES);
        if over > 0 {
            self.bytes.drain(..over);
            self.truncated = true;
        }
    }

    /// The tail as text: decoded lossily, without a leading line cut by
    /// truncation, its last [`TAIL_LINES`] lines, sanitized, and capped.
    pub(crate) fn render(&self, secrets: &[String]) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<&str> = text.lines().collect();
        if self.truncated && !lines.is_empty() {
            lines.remove(0);
        }
        let start = lines.len().saturating_sub(TAIL_LINES);
        let rendered = lines[start..]
            .iter()
            .map(|line| sanitize(line, secrets))
            .collect::<Vec<_>>()
            .join("\n");
        cap(rendered, RENDERED_BYTES)
    }
}

fn cap(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text.truncate(at);
    text.push_str(" …[cut]");
    text
}

/// Where a run's stderr is echoed for a person watching: the source's
/// name, and the writer its lines go to.
pub(crate) type Echo = (String, NonBlocking);

/// A writer that never blocks: a line it cannot take now is dropped. The
/// echo is for a person watching, so a stalled terminal costs it lines,
/// never the capture its run depends on.
pub(crate) struct NonBlocking(std::fs::File);

impl NonBlocking {
    /// This process's terminal, opened afresh so its non-blocking mode is
    /// this writer's alone and not the shared stderr's. `None` when stderr
    /// is not a terminal.
    pub(crate) fn terminal() -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut name = [0 as libc::c_char; 256];
        // SAFETY: the buffer is valid for its length; ttyname_r writes a
        // NUL-terminated path into it or fails.
        if unsafe { libc::ttyname_r(2, name.as_mut_ptr(), name.len()) } != 0 {
            return None;
        }
        // SAFETY: ttyname_r succeeded, so `name` holds a C string.
        let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .ok()
            .map(NonBlocking)
    }

    /// `file`, made non-blocking. It must be this writer's own
    /// description: the mode is shared with every copy of it.
    #[cfg(test)]
    pub(crate) fn own(file: std::fs::File) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        // SAFETY: fcntl on a valid descriptor this function owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(NonBlocking(file))
    }

    fn offer(&mut self, line: &[u8]) {
        use std::io::Write;
        let _ = self.0.write(line);
    }
}

/// Read a run's stderr to its end, in fixed chunks, into `tail`; and, when
/// there is an `echo`, write each line to it, capped and sanitized.
/// Memory stays bounded whatever the producer writes.
pub(crate) async fn drain(
    mut stderr: impl tokio::io::AsyncRead + Unpin,
    tail: std::sync::Arc<std::sync::Mutex<Tail>>,
    mut echo: Option<Echo>,
    secrets: std::sync::Arc<Vec<String>>,
) {
    use tokio::io::AsyncReadExt;
    let mut chunk = vec![0u8; CHUNK];
    let mut line: Vec<u8> = Vec::with_capacity(ECHO_LINE);
    let mut cut = false;
    loop {
        let n = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        tail.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(&chunk[..n]);
        let Some((name, out)) = &mut echo else {
            continue;
        };
        for &byte in &chunk[..n] {
            if byte == b'\n' {
                print_echo(out, name, &line, cut, &secrets);
                line.clear();
                cut = false;
            } else if line.len() < ECHO_LINE {
                line.push(byte);
            } else {
                cut = true;
            }
        }
    }
    if let Some((name, out)) = &mut echo
        && !line.is_empty()
    {
        print_echo(out, name, &line, cut, &secrets);
    }
}

fn print_echo(out: &mut NonBlocking, name: &str, line: &[u8], cut: bool, secrets: &[String]) {
    let text = sanitize(&String::from_utf8_lossy(line), secrets);
    let more = if cut { " …[cut]" } else { "" };
    out.offer(format!("wait: command source `{name}`: {text}{more}\n").as_bytes());
}

/// A run's stderr reader, owned by its run: dropping it, on any return
/// or when the host is cancelled, stops it and releases its pipe.
pub(crate) struct Reader(Option<tokio::task::JoinHandle<()>>);

impl Reader {
    pub(crate) fn spawn(reading: impl std::future::Future<Output = ()> + Send + 'static) -> Self {
        Reader(Some(tokio::spawn(reading)))
    }

    /// Let it read until `by`, then stop it, and return only once it has
    /// ended either way.
    pub(crate) async fn finish_by(mut self, by: tokio::time::Instant) {
        if let Some(task) = self.0.as_mut()
            && tokio::time::timeout_at(by, &mut *task).await.is_err()
        {
            task.abort();
            let _ = (&mut *task).await;
        }
        self.0 = None;
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    Exit(i32),
    Signal(i32),
    Launch(String),
    /// Its status could not be read.
    Unknown,
}

impl Ended {
    pub(crate) fn of(status: Option<std::process::ExitStatus>) -> Self {
        use std::os::unix::process::ExitStatusExt;
        match status {
            Some(s) => match (s.code(), s.signal()) {
                (Some(code), _) => Ended::Exit(code),
                (None, Some(signal)) => Ended::Signal(signal),
                (None, None) => Ended::Unknown,
            },
            None => Ended::Unknown,
        }
    }

    fn describe(&self, secrets: &[String]) -> String {
        match self {
            Ended::Exit(code) => format!("exited with exit {code}"),
            Ended::Signal(signal) => format!("was killed by signal {signal}"),
            Ended::Launch(why) => format!("did not start: {}", sanitize(why, secrets)),
            Ended::Unknown => "ended, with a status that could not be read".to_string(),
        }
    }
}

/// The failure event's text: the sanitized command, the streak, how the
/// last run ended, and its sanitized stderr tail.
pub(crate) fn failure_text(
    command: &[String],
    stops: u32,
    ended: &Ended,
    tail: &str,
    secrets: &[String],
) -> String {
    let mut text = format!(
        "`{}` has stopped {stops} times in a row and is being restarted. Its last run {}.",
        sanitize(&command.join(" "), secrets),
        ended.describe(secrets)
    );
    if !tail.is_empty() {
        text.push_str("\nIts last stderr:\n");
        text.push_str(tail);
    }
    text
}

/// The values of this process's environment variables whose names look
/// like secrets: masked wherever a diagnostic would show them. Ordinary
/// context such as `PATH` or `HOME` is not among them.
pub(crate) fn host_secrets() -> Vec<String> {
    // Never `vars()`, which panics on a name or value that is not
    // Unicode: an unrelated variable must not stop a source from
    // running. Each is read lossily, as stderr is.
    secrets_in(std::env::vars_os().map(|(name, value)| {
        (
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }))
}

fn secrets_in(vars: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    const NAMED: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"];
    let mut values: Vec<String> = vars
        .into_iter()
        .filter(|(name, value)| {
            let name = name.to_ascii_uppercase();
            value.len() >= 8 && NAMED.iter().any(|n| name.contains(n))
        })
        .map(|(_, value)| value)
        .collect();
    // Longest first, so one value containing another is masked whole.
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values
}

/// Best-effort: mask known secret values and common secret shapes, then
/// escape terminal control characters. A secret in a shape this does not
/// recognise is not masked.
pub(crate) fn sanitize(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret.as_str(), MASK);
        }
    }
    let text = mask_after_keys(&text);
    let text = mask_userinfo(&text);
    let text = mask_long_runs(&text);
    escape(&text)
}

/// `token=…`, `password=…`, `secret=…`, `api_key=…`, an `Authorization:`
/// value, and the word after `Bearer`.
fn mask_after_keys(text: &str) -> String {
    const KEYS: [&str; 4] = ["token=", "password=", "secret=", "api_key="];
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &lower[i..];
        if let Some(key) = KEYS.iter().find(|k| rest.starts_with(**k)) {
            out.push_str(&text[i..i + key.len()]);
            i += key.len();
            // A quoted value is the whole quoted run, to its closing quote
            // or, if cut off, to the end of the line.
            if let Some(quote) = text[i..].chars().next().filter(|c| matches!(c, '"' | '\'')) {
                out.push(quote);
                i += 1;
                let close = closing_quote(&text[i..], quote).map_or(text.len(), |e| i + e);
                out.push_str(MASK);
                i = close;
                if text[i..].starts_with(quote) {
                    out.push(quote);
                    i += 1;
                }
                continue;
            }
            let end = text[i..]
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ','))
                .map_or(text.len(), |e| i + e);
            if end > i {
                out.push_str(MASK);
            }
            i = end;
            continue;
        }
        if rest.starts_with("authorization:") {
            out.push_str(&text[i..i + "authorization:".len()]);
            out.push(' ');
            out.push_str(MASK);
            i = text[i..].find('\n').map_or(text.len(), |e| i + e);
            continue;
        }
        if rest.starts_with("bearer ") {
            out.push_str(&text[i..i + "bearer ".len()]);
            i += "bearer ".len();
            let end = text[i..]
                .find(char::is_whitespace)
                .map_or(text.len(), |e| i + e);
            out.push_str(MASK);
            i = end;
            continue;
        }
        let c = text[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Where a quoted value that opened with `quote` closes in `rest`: the
/// first `quote` not escaped by a backslash, or the end of the line when
/// it never closes.
fn closing_quote(rest: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (at, c) in rest.char_indices() {
        match c {
            '\n' => return Some(at),
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            _ if c == quote => return Some(at),
            _ => {}
        }
    }
    None
}

/// `scheme://user:pass@host` loses its userinfo.
fn mask_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(userinfo_end) => {
                out.push_str(MASK);
                rest = &tail[userinfo_end..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// Hex runs of 32 or more characters, and base64-like runs of 40 or more.
fn mask_long_runs(text: &str) -> String {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-');
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let hex = run.len() >= 32 && run.chars().all(|c| c.is_ascii_hexdigit());
        let b64 = run.len() >= 40;
        out.push_str(if hex || b64 { MASK } else { run.as_str() });
        run.clear();
    };
    for c in text.chars() {
        if word(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Terminal control characters as `\xNN`.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (c as u32) < 0x20 || c == '\x7f' {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_only_its_last_bytes_however_they_arrive() {
        let mut tail = Tail::default();
        let line = vec![b'a'; 3 * 1024 * 1024];
        for chunk in line.chunks(CHUNK) {
            tail.push(chunk);
        }
        tail.push(b"\nlast words\n");
        assert!(tail.bytes.len() <= TAIL_BYTES);
        assert_eq!(tail.render(&[]), "last words", "the cut line is dropped");

        let mut odd = Tail::default();
        odd.push(b"bad \xff\xfe bytes \x1b[31mred\nno newline at the end");
        let shown = odd.render(&[]);
        assert!(
            shown.contains("bad \u{fffd}\u{fffd} bytes \\x1b[31mred"),
            "{shown}"
        );
        assert!(shown.ends_with("no newline at the end"), "{shown}");

        let mut many = Tail::default();
        for n in 0..100 {
            many.push(format!("line {n}\n").as_bytes());
        }
        let shown = many.render(&[]);
        assert_eq!(shown.lines().count(), TAIL_LINES);
        assert!(shown.ends_with("line 99"));

        let mut controls = Tail::default();
        controls.push(&vec![0x01; TAIL_BYTES]);
        assert!(controls.render(&[]).len() <= RENDERED_BYTES + " …[cut]".len());
    }

    #[test]
    fn secrets_are_masked_wherever_they_appear() {
        let secrets = secrets_in([
            ("ANX_TOKEN".to_string(), "s3cret-value-123".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), "/Users/someone".to_string()),
        ]);
        assert_eq!(secrets, vec!["s3cret-value-123".to_string()]);
        let shown = sanitize(
            "auth failed for s3cret-value-123 token=abc123 PASSWORD=hunter2 \
             Authorization: Basic Zm9v\n",
            &secrets,
        );
        for leaked in ["s3cret-value-123", "abc123", "hunter2", "Zm9v"] {
            assert!(!shown.contains(leaked), "{leaked} in {shown}");
        }
        assert!(
            sanitize("curl https://bob:pw@example.com/x", &[])
                .contains("https://[redacted]@example.com/x")
        );
        assert!(!sanitize("Bearer eyJhbGciOi", &[]).contains("eyJhbGciOi"));
        assert!(!sanitize(&"ab".repeat(20), &[]).contains("abab"));
        assert!(!sanitize(&"Zz9+".repeat(12), &[]).contains("Zz9+"));
        let ordinary = sanitize("PATH=/usr/bin:/bin HOME=/Users/someone", &secrets);
        assert!(ordinary.contains("/usr/bin:/bin") && ordinary.contains("/Users/someone"));
    }

    #[test]
    fn the_failure_text_says_how_the_last_run_ended() {
        let command = vec!["anx".into(), "events".into(), "--token=abc123".into()];
        let exit = failure_text(&command, 3, &Ended::Exit(3), "boom", &[]);
        assert!(
            exit.contains("exit 3") && exit.contains("boom") && !exit.contains("abc123"),
            "{exit}"
        );
        assert!(failure_text(&command, 3, &Ended::Signal(9), "", &[]).contains("signal 9"));
        let launch = failure_text(
            &command,
            3,
            &Ended::Launch("anx: not found (token=xyz789)".into()),
            "",
            &[],
        );
        assert!(
            launch.contains("did not start") && !launch.contains("xyz789"),
            "{launch}"
        );
    }

    /// A non-blocking echo into a pipe this test owns, and its read end.
    fn echo_pipe() -> (NonBlocking, std::io::PipeReader) {
        let (read, write) = std::io::pipe().unwrap();
        let file = std::fs::File::from(std::os::fd::OwnedFd::from(write));
        (NonBlocking::own(file).unwrap(), read)
    }

    async fn drained(input: Vec<u8>, echo: bool) -> (String, String) {
        use std::io::Read;
        let tail = std::sync::Arc::new(std::sync::Mutex::new(Tail::default()));
        let (sink, mut seen) = echo_pipe();
        let echo = echo.then(|| ("src".to_string(), sink));
        drain(&input[..], tail.clone(), echo, Default::default()).await;
        let rendered = tail.lock().unwrap().render(&[]);
        let mut echoed = String::new();
        seen.read_to_string(&mut echoed).unwrap();
        (rendered, echoed)
    }

    /// A piped wait echoes nothing, however much is written, and still
    /// keeps the tail; a terminal echo is line-capped and sanitized.
    #[tokio::test]
    async fn the_echo_is_bounded_and_off_unless_asked_for() {
        let flood = "short line\n".repeat(200_000).into_bytes();
        let (tail, echoed) = drained(flood, false).await;
        assert!(echoed.is_empty());
        assert!(tail.ends_with("short line"));

        let mut long = vec![b'x'; 10 * ECHO_LINE];
        long.extend_from_slice(b"\ntoken=abc123\n");
        let (_, echoed) = drained(long, true).await;
        let lines: Vec<&str> = echoed.lines().collect();
        assert_eq!(lines.len(), 2, "{echoed}");
        assert!(lines[0].len() < ECHO_LINE + 100 && lines[0].ends_with("…[cut]"));
        assert!(!lines[1].contains("abc123"), "{echoed}");
    }

    /// A terminal that stops taking the echo costs the echo lines, never
    /// the capture: the reader still stops at its cutoff and lets go of
    /// its pipe.
    #[tokio::test]
    async fn a_stalled_echo_cannot_hold_the_reader_past_its_cutoff() {
        use tokio::io::AsyncWriteExt;
        let (sink, _never_read) = echo_pipe();
        let (mut stderr, read) = tokio::io::duplex(64 * 1024);
        let tail = std::sync::Arc::new(std::sync::Mutex::new(Tail::default()));
        let reader = Reader::spawn(drain(
            read,
            tail.clone(),
            Some(("src".into(), sink)),
            Default::default(),
        ));
        for _ in 0..2000 {
            stderr.write_all(&[b'y'; 511]).await.unwrap();
            stderr.write_all(b"\n").await.unwrap();
        }
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.finish_by(started + std::time::Duration::from_millis(50)),
        )
        .await
        .expect("a stalled echo held the reader past its cutoff");
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        let mut released = false;
        for _ in 0..100 {
            if stderr.write_all(b"after\n").await.is_err() {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(released, "the stopped reader still held its pipe");
        assert!(
            !tail.lock().unwrap().render(&[]).is_empty(),
            "the capture lost the stderr the echo could not take"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_value() {
        for text in [
            r#"exec api --password="abc\"hunter2" after"#,
            r#"exec api --password='abc\'hunter2' after"#,
            r#"exec api --password="abc\\" hunter2-is-not-secret"#,
            r#"exec api --password="abc\"hunter2"#,
        ] {
            let shown = sanitize(text, &[]);
            assert!(
                !shown.contains("abc") && !shown.contains("hunter2\""),
                "{shown}"
            );
        }
        let shown = sanitize(r#"exec api --password="abc\"hunter2" after"#, &[]);
        assert!(
            shown.ends_with("\" after"),
            "the context after it stays: {shown}"
        );
        let escaped_backslash = sanitize(r#"--password="abc\\" next"#, &[]);
        assert!(escaped_backslash.ends_with(" next"), "{escaped_backslash}");
    }

    #[test]
    fn a_quoted_value_is_masked_whole() {
        for text in [
            "api --token=\"abc123\" password='hunter2' after",
            "error: token=\"abc123 still going",
        ] {
            let shown = sanitize(text, &[]);
            assert!(
                !shown.contains("abc123") && !shown.contains("hunter2"),
                "{shown}"
            );
        }
        assert_eq!(
            sanitize("api --token=\"abc123\" after", &[]),
            "api --token=\"[redacted]\" after",
            "the surrounding context stays"
        );
        let launch = failure_text(
            &[
                "sh".into(),
                "-c".into(),
                "exec api --token=\"abc123\" password='hunter2'".into(),
            ],
            3,
            &Ended::Launch("api: refused password='hunter2'".into()),
            &Tail::default().render(&[]),
            &[],
        );
        assert!(
            !launch.contains("abc123") && !launch.contains("hunter2"),
            "{launch}"
        );
    }

    /// An environment value that is not Unicode does not stop the secret
    /// scan: it is read lossily, like stderr.
    #[test]
    fn a_non_unicode_value_is_read_lossily() {
        use std::os::unix::ffi::OsStrExt;
        let value = std::ffi::OsStr::from_bytes(b"secret-\xffvalue");
        let read = value.to_string_lossy().into_owned();
        assert_eq!(
            secrets_in([("SOME_TOKEN".to_string(), read.clone())]),
            vec![read]
        );
        let _ = host_secrets();
    }

    /// Dropping a reader stops it and lets go of its pipe; finishing one
    /// returns only once it has ended, stopping it at the cutoff.
    #[tokio::test]
    async fn a_reader_is_owned_by_its_run() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, read) = tokio::io::duplex(64);
        let tail = std::sync::Arc::new(std::sync::Mutex::new(Tail::default()));
        let reader = Reader::spawn(drain(read, tail.clone(), None, Default::default()));
        writer.write_all(b"before\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(reader);
        let mut released = false;
        for _ in 0..100 {
            if writer.write_all(b"after\n").await.is_err() {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(released, "a dropped reader still held its pipe");

        let (_still_open, read) = tokio::io::duplex(64);
        let reader = Reader::spawn(drain(read, tail, None, Default::default()));
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.finish_by(started + std::time::Duration::from_millis(50)),
        )
        .await
        .expect("finishing a reader past its cutoff did not stop it");
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }
}
