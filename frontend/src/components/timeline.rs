use leptos::prelude::*;

use crate::api::TimelineEvent;

/// Chronological event list for one session. Each event is one row with
/// a phase-aware marker on the left, kind / SHA / verdict in the middle,
/// and (for reviews) the author on the right.
#[component]
pub fn Timeline(events: Vec<TimelineEvent>) -> impl IntoView {
    if events.is_empty() {
        return view! { <p class="muted">"No activity yet."</p> }.into_any();
    }
    let rows = events
        .into_iter()
        .map(|e| view! { <TimelineRow event=e/> })
        .collect_view();
    view! {
        <ol class="timeline">
            {rows}
        </ol>
    }
    .into_any()
}

#[component]
fn TimelineRow(event: TimelineEvent) -> impl IntoView {
    match event {
        TimelineEvent::CommitPlan { sha, .. } => {
            let short = short_sha(&sha);
            view! {
                <li class="timeline-row timeline-commit-plan">
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">"Plan revision"</span>
                    <span class="timeline-sha">
                        <code>{short}</code>
                    </span>
                </li>
            }
            .into_any()
        }
        TimelineEvent::CommitImpl { sha, .. } => {
            let short = short_sha(&sha);
            view! {
                <li class="timeline-row timeline-commit-impl">
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">"Implementation"</span>
                    <span class="timeline-sha">
                        <code>{short}</code>
                    </span>
                </li>
            }
            .into_any()
        }
        TimelineEvent::CommitMixed { sha, .. } => {
            let short = short_sha(&sha);
            view! {
                <li class="timeline-row timeline-commit-mixed">
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">"Plan + impl"</span>
                    <span class="timeline-sha">
                        <code>{short}</code>
                    </span>
                </li>
            }
            .into_any()
        }
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
        TimelineEvent::HeldFeedback { author, reason, .. } => {
            let reason_label = format!("held: {reason}");
            view! {
                <li class="timeline-row timeline-held">
                    <span class="timeline-marker"></span>
                    <span class="timeline-kind">"Held feedback"</span>
                    <span class="timeline-author">{author}</span>
                    <span class="timeline-target">{reason_label}</span>
                </li>
            }
            .into_any()
        }
    }
}

fn short_sha(sha: &str) -> String {
    if sha.len() > 8 {
        sha[..8].to_string()
    } else {
        sha.to_string()
    }
}

fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "approve" => "APPROVE",
        "request_changes" => "REQUEST_CHANGES",
        _ => "UNMARKED",
    }
}
