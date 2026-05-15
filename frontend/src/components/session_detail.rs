use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{SessionDetail as SessionDetailData, fetch_session};
use crate::components::feedback_card::{FeedbackCard, HeldFeedbackCard};
use crate::components::meta_strip::MetaStrip;
use crate::components::pr_hint_card::PrHintCard;
use crate::components::timeline::Timeline;
use crate::components::waiting_banner::WaitingBanner;
use crate::store::EventStore;

#[component]
pub fn SessionDetail() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let params = use_params_map();
    let session_id = move || params.read().get("session_id").unwrap_or_default();
    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_session(session_id())
    });

    view! {
        <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
            {move || {
                resource
                    .with(|res| match res {
                        Some(Ok(detail)) => detail_view(detail.clone()).into_any(),
                        Some(Err(e)) => {
                            view! { <p class="error">"Failed to load: " {e.to_string()}</p> }
                                .into_any()
                        }
                        None => unreachable!(
                            "Suspense fallback handles the pending state",
                        ),
                    })
            }}
        </Suspense>
    }
}

fn detail_view(detail: SessionDetailData) -> impl IntoView {
    let session_id = detail.session_id.clone();
    let waiting_on = detail.waiting_on.clone();
    let plan_feedback = detail.plan_feedback.clone();
    let impl_feedback = detail.impl_feedback.clone();
    let held_feedback = detail.held_plan_feedback.clone();
    let timeline_events = detail.timeline.clone();
    let pr_hint = detail.pr_hint.clone();

    view! {
        <article class="session-page">
            <header class="session-header">
                <a href="/" class="back-link">"← sessions"</a>
                <h1>{session_id}</h1>
            </header>
            <WaitingBanner waiting_on=waiting_on/>
            <div class="session-grid">
                <aside class="session-sidebar">
                    <MetaStrip session=detail/>
                    {pr_hint.map(|hint| view! { <PrHintCard hint=hint/> })}
                </aside>
                <main class="session-main">
                    <section class="session-section">
                        <h2>"Timeline"</h2>
                        <Timeline events=timeline_events/>
                    </section>
                    {feedback_section(
                        "Plan feedback",
                        plan_feedback,
                        held_feedback,
                    )}
                    {impl_feedback_section(impl_feedback)}
                </main>
            </div>
        </article>
    }
}

fn feedback_section(
    title: &'static str,
    plan: Vec<crate::api::FeedbackEntry>,
    held: Vec<crate::api::HeldFeedbackEntry>,
) -> AnyView {
    if plan.is_empty() && held.is_empty() {
        return view! {
            <section class="session-section">
                <h2>{title}</h2>
                <p class="muted">"No plan-phase feedback yet."</p>
            </section>
        }
        .into_any();
    }
    view! {
        <section class="session-section">
            <h2>{title}</h2>
            <div class="feedback-list">
                {plan
                    .into_iter()
                    .map(|fb| view! { <FeedbackCard entry=fb/> })
                    .collect_view()}
                {held
                    .into_iter()
                    .map(|fb| view! { <HeldFeedbackCard entry=fb/> })
                    .collect_view()}
            </div>
        </section>
    }
    .into_any()
}

fn impl_feedback_section(entries: Vec<crate::api::FeedbackEntry>) -> AnyView {
    if entries.is_empty() {
        return view! {
            <section class="session-section">
                <h2>"Implementation feedback"</h2>
                <p class="muted">"No impl-phase feedback yet."</p>
            </section>
        }
        .into_any();
    }
    view! {
        <section class="session-section">
            <h2>"Implementation feedback"</h2>
            <div class="feedback-list">
                {entries
                    .into_iter()
                    .map(|fb| view! { <FeedbackCard entry=fb/> })
                    .collect_view()}
            </div>
        </section>
    }
    .into_any()
}
