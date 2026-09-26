//! The panel must ASK whether a waker died.
//!
//! Every other snapshot input announces itself: a watcher event fires
//! because a file changed. A waiter being killed changes no file, so
//! nothing raises `refresh_pending`, and the loop repaints the
//! snapshot it already holds — the panel showed a healthy row after
//! the waiter died, including across key repaints (codex 451c5dbc,
//! grok 8aa2736120d7).
//!
//! `waker_changed` is unit-tested; its CALL SITE is not, because the
//! loop owns a terminal and cannot be driven from a test. Deleting
//! that one call therefore restores the whole bug and passes the
//! entire suite — which is what this gate exists to stop, on the
//! `git_boundary` pattern already used here for rules the type
//! system cannot carry.

use std::fs;

#[test]
fn the_status_loop_probes_the_waker_fingerprint_on_its_own_turns() {
    let src = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/cli/status_tui/mod.rs"
    ))
    .expect("the panel's event loop");

    // Formatting-agnostic: find the call and read the window after
    // it, because rustfmt splits it across lines and a literal match
    // would fail for a reason that has nothing to do with the rule.
    let at = src
        .match_indices("waker_changed(")
        .find(|(at, _)| {
            let after = &src[*at..src.len().min(*at + 400)];
            // The loop's probe, not the unit test's: the real
            // fingerprint behind it, and its answer reaching the
            // rebuild flag. A loop that asks and ignores the answer
            // is the same bug.
            after.contains("waker_probe")
                && after.contains("crate::cli::status::waker_fingerprint(&repo)")
                && after.contains("refresh_pending = true")
        })
        .map(|(at, _)| at);
    let at = at.unwrap_or_else(|| {
        panic!(
            "the event loop must ask `waker_changed` on its own turns, with the \
             real fingerprint, and raise `refresh_pending` when it flips; without \
             that call a dead waker is invisible until some unrelated file \
             happens to change"
        )
    });

    // WHERE it sits is the property, not that it exists. Nested under
    // `if batch.refresh` the call would still be present and still be
    // gated on a filesystem event — which is the entire bug (grok
    // bfda96fdb769). Two structural checks, neither sufficient alone:
    //
    // 1. it comes AFTER the timeout arms, so an idle tick reaches it,
    //    and BEFORE the flush it feeds.
    //
    // The anchor has to be the IDLE arm. `find` returns the first
    // copy of that pattern, which is the overlay's — inside
    // `if detail.is_some()`, a branch that continues and never draws
    // the panel. Anchoring there passed a probe placed before
    // `recv_timeout`, which an idle tick does not reach until the
    // NEXT turn (grok 2f3fd290d62b). The idle arm is the one after
    // the spinner guard, so the guard is the landmark.
    let spinner_guard = "Err(mpsc::RecvTimeoutError::Timeout) if spinner_visible";
    assert_eq!(
        src.matches(spinner_guard).count(),
        1,
        "the spinner guard is the landmark for the idle arm; more than one \
         and this anchors on whichever comes first, which is the mistake \
         being fixed",
    );
    let guard_at = src.find(spinner_guard).expect("the spinner-tick arm");
    let timeout_arm = guard_at
        + src[guard_at..]
            .find("Err(mpsc::RecvTimeoutError::Timeout) => {}")
            .expect("the idle arm after the spinner guard");
    let flush = src
        .find("if refresh_pending && last_rebuild.elapsed() >= REBUILD_MIN")
        .expect("the trailing-edge flush");
    assert!(
        timeout_arm < at && at < flush,
        "the probe must sit after the IDLE timeout arm and before the flush \
         (idle arm {timeout_arm}, probe {at}, flush {flush})",
    );

    // 2. it is at the loop body's own indentation. Anything deeper is
    //    inside a branch, and the branch that matters is the one taken
    //    only when a file changed.
    let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &src[line_start..];
    let line = &line[..line.find('\n').unwrap_or(line.len())];
    let indent = line.len() - line.trim_start().len();
    assert!(
        indent == 8,
        "the probe must be a statement of the loop body, not nested in a \
         branch; found {indent} columns on {line:?}",
    );
}
