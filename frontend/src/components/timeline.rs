use std::sync::Arc;

use leptos::prelude::*;

use crate::api::TimelineEvent;
use crate::components::expanded_commit::ExpandedCommit;
use crate::util::short_sha;

/// Chronological event list for one session. Each event is one row with
/// a phase-aware marker on the left, kind / SHA / verdict in the middle,
/// and (for reviews) the author on the right.
///
/// Rendered through `<For key=...>` so SSE-driven re-fetches diff against
/// the existing DOM instead of replacing every row — that keeps the
/// "pulse on insertion" CSS animation from firing on rows that didn't
/// actually change.
#[component]
pub fn Timeline(plan_id: String, events: Vec<TimelineEvent>) -> impl IntoView {
    if events.is_empty() {
        return view! { <p class="muted">"No activity yet."</p> }.into_any();
    }
    let plan_id = std::sync::Arc::new(plan_id);
    view! {
        <ol class="timeline">
            <For
                each=move || events.clone().into_iter().enumerate()
                key=|(idx, e)| timeline_key(*idx, e)
                children=move |(_, e)| {
                    let plan_id = plan_id.clone();
                    view! { <TimelineRow plan_id=plan_id event=e/> }
                }
            />
        </ol>
    }
    .into_any()
}

/// Stable per-row key. The index is included as a tiebreaker because the
/// same `(target, author, verdict)` can appear in multiple positions
/// across phases. For commit rows the SHA alone is unique.
fn timeline_key(_idx: usize, e: &TimelineEvent) -> String {
    match e {
        TimelineEvent::CommitPlan { sha, .. }
        | TimelineEvent::CommitImpl { sha, .. }
        | TimelineEvent::CommitMixed { sha, .. }
        | TimelineEvent::CommitFinalize { sha, .. } => format!("commit:{sha}"),
        TimelineEvent::Review {
            target,
            author,
            phase,
            ..
        } => format!("review:{phase}:{target}:{author}"),
    }
}

#[component]
fn TimelineRow(plan_id: std::sync::Arc<String>, event: TimelineEvent) -> impl IntoView {
    match event {
        TimelineEvent::CommitPlan { sha, subject, .. } => commit_row(
            plan_id,
            sha,
            subject,
            "timeline-row timeline-commit-plan",
            "Plan revision",
        )
        .into_any(),
        TimelineEvent::CommitImpl { sha, subject, .. } => commit_row(
            plan_id,
            sha,
            subject,
            "timeline-row timeline-commit-impl",
            "Implementation",
        )
        .into_any(),
        TimelineEvent::CommitMixed { sha, subject, .. } => commit_row(
            plan_id,
            sha,
            subject,
            "timeline-row timeline-commit-mixed",
            "Plan + impl",
        )
        .into_any(),
        TimelineEvent::CommitFinalize { sha, subject, .. } => commit_row(
            plan_id,
            sha,
            subject,
            "timeline-row timeline-commit-finalize",
            "Finalized",
        )
        .into_any(),
        TimelineEvent::Review {
            phase,
            target,
            author,
            verdict,
            ..
        } => {
            let short_target = short_sha(&target);
            let target_label = format!("on {short_target}");
            let verdict_class = format!("verdict-pill verdict-pill-sm verdict-{verdict}");
            let phase_label = format!("{phase} review");
            let verdict_label_text = verdict_label(&verdict).to_string();
            view! {
                <li class="timeline-row timeline-review">
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">{phase_label}</span>
                    <span class=verdict_class>{verdict_label_text}</span>
                    <span class="timeline-author">{author}</span>
                    <span class="timeline-sha timeline-target">
                        <code>{target_label}</code>
                    </span>
                </li>
            }
            .into_any()
        }
        // HeldFeedback variant deleted in phase 2.8 along with the
        // server-side queue.
        // (no branch — match is exhaustive)
        #[allow(unreachable_patterns)]
        _ => view! { <li class="timeline-row"></li> }.into_any(),
    }
}

fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "APPROVE",
        "request_changes" => "REQUEST_CHANGES",
        _ => "UNMARKED",
    }
}

/// Click-to-expand commit row. Subject + caret on the top line, full
/// commit (message body + structured diff) lazily mounted below when
/// `expanded` is set. Each instance owns its own RwSignal so toggling
/// is local; the `<For key>` in `<Timeline/>` is keyed by SHA so
/// expanded state survives re-fetches that don't change row identity.
fn commit_row(
    plan_id: Arc<String>,
    sha: String,
    subject: String,
    li_class: &'static str,
    kind_label: &'static str,
) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let short = short_sha(&sha);
    let sha_for_expand = sha.clone();
    let plan_id_for_expand = plan_id.clone();
    let caret = move || if expanded.get() { "▼" } else { "▸" };
    let subject_display = if subject.is_empty() {
        "(no message)".to_string()
    } else {
        subject.clone()
    };
    let subject_title = subject_display.clone();
    let on_toggle = move |_| expanded.update(|v| *v = !*v);
    let commit_href = format!("/plan/{}/commit/{}", plan_id.as_ref(), sha);
    // Avoid <a> inside <button> (invalid HTML). The row is a div with
    // ONE toggle button spanning caret + kind + subject, plus a
    // sibling <a> for the SHA. Two focus stops per row, not three —
    // the older shape had a separate toggle button AND a subject
    // button firing the same handler, which doubled the Tab stop
    // count for no gain. The link's click event stops propagation so
    // navigating to the dedicated commit page doesn't also flip the
    // accordion.
    let on_link_click = |ev: leptos::ev::MouseEvent| ev.stop_propagation();
    view! {
        <li class=li_class>
            <div class="timeline-commit-row">
                <button
                    class="timeline-commit-toggle"
                    type="button"
                    on:click=on_toggle
                    title=subject_title
                >
                    <span class="timeline-caret">{caret}</span>
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">{kind_label}</span>
                    <span class="timeline-commit-subject">{subject_display}</span>
                </button>
                <a
                    class="timeline-sha"
                    href=commit_href
                    on:click=on_link_click
                    title="Open commit page"
                >
                    <code>{short}</code>
                </a>
            </div>
            <Show when=move || expanded.get()>
                <ExpandedCommit
                    plan_id=plan_id_for_expand.as_ref().clone()
                    sha=sha_for_expand.clone()
                />
            </Show>
        </li>
    }
}
