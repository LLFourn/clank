use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{FeedbackEntry, PlanDetail, fetch_plan};
use crate::components::feedback_card::{FeedbackCard, HeldFeedbackCard};
use crate::components::meta_strip::MetaStrip;
use crate::components::plan_preview::PlanPreview;
use crate::components::pr_hint_card::PrHintCard;
use crate::components::timeline::Timeline;
use crate::components::waiting_banner::WaitingBanner;
use crate::store::EventStore;

#[component]
pub fn SessionDetail() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let params = use_params_map();
    let plan_id = move || {
        let p = params.read();
        let repo = p.get("repo").unwrap_or_default();
        let stem = p.get("stem_md").unwrap_or_default();
        format!("{repo}/{stem}")
    };
    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_plan(plan_id())
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
                        None => view! { <p class="loading">"Loading…"</p> }.into_any(),
                    })
            }}
        </Suspense>
    }
}

fn detail_view(detail: PlanDetail) -> impl IntoView {
    let plan_id = detail.plan_id.clone();
    let plan_id_for_timeline = plan_id.clone();
    let waiting_on = detail.waiting_on.clone();
    let plan_feedback = detail.plan_feedback.clone();
    let impl_feedback = detail.impl_feedback.clone();
    let held_feedback = detail.held_plan_feedback.clone();
    let timeline_events = detail.timeline.clone();
    let pr_hint = detail.pr_hint.clone();
    let state_class = format!("state-chip state-{}", detail.state);
    let state_label = detail.state.clone();
    let plan_body_html = detail.plan_body_html.clone();
    let plan_body_truncated = detail.plan_body_truncated;
    let latest_target_sha = detail.review_target.as_ref().map(|t| t.commit_sha.clone());
    let revision_link = match &detail.latest_plan_revision {
        Some(rev) => format!("/plan/{}/revision/{}", detail.plan_id, rev.commit_sha),
        None => format!("/plan/{}", detail.plan_id),
    };
    let latest_review =
        latest_review_for_target(&plan_feedback, &impl_feedback, latest_target_sha.as_deref());

    view! {
        <article class="session-page">
            <header class="session-header">
                <a href="/" class="back-link">"← plans"</a>
                <h1>
                    <code>{plan_id}</code>
                    <span class=state_class>{state_label}</span>
                </h1>
            </header>
            <WaitingBanner waiting_on=waiting_on/>
            <div class="session-grid">
                <aside class="session-sidebar">
                    <MetaStrip session=detail/>
                    {pr_hint.map(|hint| view! { <PrHintCard hint=hint/> })}
                </aside>
                <main class="session-main">
                    <section class="session-section">
                        <h2>"Plan"</h2>
                        <PlanPreview
                            body_html=plan_body_html
                            truncated=plan_body_truncated
                            revision_link=revision_link
                        />
                        {latest_review_section(latest_review)}
                    </section>
                    <section class="session-section">
                        <h2>"Timeline"</h2>
                        <Timeline plan_id=plan_id_for_timeline events=timeline_events/>
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

/// Pick the newest feedback entry whose `target_sha` matches the
/// current review target. **Filter then sort** — sorting alone would
/// surface stale reviews against an older target when the current one
/// has none, which the plan explicitly forbids.
fn latest_review_for_target(
    plan_feedback: &[FeedbackEntry],
    impl_feedback: &[FeedbackEntry],
    target: Option<&str>,
) -> Option<FeedbackEntry> {
    let target = target?;
    plan_feedback
        .iter()
        .chain(impl_feedback.iter())
        .filter(|fb| fb.target_sha == target)
        .max_by_key(|fb| fb.created_at)
        .cloned()
}

fn latest_review_section(latest: Option<FeedbackEntry>) -> AnyView {
    match latest {
        Some(fb) => view! {
            <div class="latest-review">
                <h3>"Latest review"</h3>
                <FeedbackCard entry=fb/>
            </div>
        }
        .into_any(),
        None => view! {
            <div class="latest-review">
                <h3>"Latest review"</h3>
                <p class="muted">"No reviews yet for the current target."</p>
            </div>
        }
        .into_any(),
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
