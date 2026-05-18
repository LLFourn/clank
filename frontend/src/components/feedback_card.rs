use leptos::prelude::*;

use crate::api::{CommitFeedback, Verdict};
use crate::markdown;
use crate::util::short_sha;

/// Single feedback file as a card. Verdict pill on the left, author +
/// timestamp + target SHA in the header, sanitized markdown body below.
/// `target_sha` is passed as a sibling prop because feedback is keyed
/// per-commit on the wire — the SHA is parent context, not intrinsic
/// to the feedback entry.
///
/// Markdown rendering happens in this component (wasm-side) — the
/// wire ships only `body` (raw markdown); `body_html` is gone.
#[component]
pub fn FeedbackCard(entry: CommitFeedback, target_sha: String) -> impl IntoView {
    let verdict_class = verdict_class(entry.verdict);
    let target_short = short_sha(&target_sha);
    let target_label = format!("on {target_short}");
    let timestamp = format_timestamp(entry.created_at);
    let body_html = markdown::render_feedback(&entry.body, entry.verdict);
    let author = entry.author.to_string();
    view! {
        <article class="feedback-card">
            <header class="feedback-header">
                <span class=verdict_class>{verdict_label(entry.verdict)}</span>
                <span class="feedback-author">{author}</span>
                <span class="feedback-target">
                    <code>{target_label}</code>
                </span>
                <span class="feedback-time">{timestamp}</span>
            </header>
            <div class="feedback-body" inner_html=body_html></div>
        </article>
    }
}

fn verdict_class(verdict: Verdict) -> String {
    format!("verdict-pill verdict-{verdict}")
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "APPROVE",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Unmarked => "UNMARKED",
    }
}

/// Format a unix-seconds timestamp as a compact "YYYY-MM-DD HH:MM"
/// string in UTC. We don't pull chrono into the WASM bundle for one
/// stamp; a tiny epoch-to-Gregorian conversion stays small.
fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let secs = ts as u64;
    let (year, month, day) = ymd_from_unix(secs);
    let hour = (secs / 3600) % 24;
    let minute = (secs / 60) % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// Civil-from-days conversion (Howard Hinnant's algorithm). Avoids the
/// chrono dep entirely for ~30 LOC.
fn ymd_from_unix(secs: u64) -> (i32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y } as i32;
    (year, m as u32, d as u32)
}
