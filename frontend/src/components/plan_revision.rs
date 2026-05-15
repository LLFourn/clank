use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{PlanRevisionPage, fetch_plan_revision};
use crate::components::feedback_card::FeedbackCard;
use crate::store::EventStore;
use crate::util::short_sha;

#[component]
pub fn PlanRevision() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let params = use_params_map();
    let session_id = move || params.read().get("session_id").unwrap_or_default();
    let sha = move || params.read().get("sha").unwrap_or_default();
    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_plan_revision(session_id(), sha())
    });

    view! {
        <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
            {move || {
                resource
                    .with(|res| match res {
                        Some(Ok(page)) => revision_view(page.clone()).into_any(),
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

fn revision_view(page: PlanRevisionPage) -> impl IntoView {
    let session_id = page.session_id.clone();
    let back_href = format!("/sessions/{session_id}");
    let short = short_sha(&page.commit_sha);
    let title = format!("Plan @ {short}");
    let prev_link = nav_link(&page.session_id, page.previous_sha.as_deref(), "← previous");
    let next_link = nav_link(&page.session_id, page.next_sha.as_deref(), "next →");
    let compare_link = compare_link(
        &page.session_id,
        &page.commit_sha,
        page.previous_sha.as_deref(),
    );
    let body_html = page.body_html.clone();
    let feedback_section = if page.feedback.is_empty() {
        None
    } else {
        let cards = page
            .feedback
            .into_iter()
            .map(|fb| view! { <FeedbackCard entry=fb/> })
            .collect_view();
        Some(view! {
            <section class="session-section">
                <h2>"Feedback on this revision"</h2>
                <div class="feedback-list">{cards}</div>
            </section>
        })
    };
    view! {
        <article class="plan-revision">
            <header class="session-header">
                <a href=back_href class="back-link">"← session"</a>
                <h1>{title}</h1>
            </header>
            <nav class="revision-nav">{prev_link}{next_link}{compare_link}</nav>
            <section class="plan-body prose" inner_html=body_html></section>
            {feedback_section}
        </article>
    }
}

fn compare_link(session_id: &str, this_sha: &str, previous_sha: Option<&str>) -> AnyView {
    match previous_sha {
        Some(prev) if !prev.is_empty() => {
            let href = format!("/sessions/{session_id}/plan/{this_sha}/diff?vs={prev}");
            view! {
                <a href=href class="revision-nav-link">
                    "diff vs previous"
                </a>
            }
            .into_any()
        }
        _ => ().into_any(),
    }
}

fn nav_link(session_id: &str, sha: Option<&str>, label: &'static str) -> AnyView {
    match sha {
        Some(s) if !s.is_empty() => {
            let href = format!("/sessions/{session_id}/plan/{s}");
            view! {
                <a href=href class="revision-nav-link">
                    {label}
                </a>
            }
            .into_any()
        }
        _ => view! { <span class="revision-nav-link muted">{label}</span> }.into_any(),
    }
}
