use leptos::prelude::*;
use leptos_router::hooks::{use_params_map, use_query_map};

use crate::api::{DiffPage, fetch_diff, fetch_session};
use crate::components::structured_diff::StructuredDiff;

/// `/sessions/:session_id/plan/:sha/diff?vs=:other` — compare two plan
/// revisions of the same session. We resolve the plan_path via a
/// `fetch_session` round-trip (it's the same path across revisions), then
/// hand off to `/api/diff?from=&to=&path=`.
#[component]
pub fn PlanDiff() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let session_id = move || params.read().get("session_id").unwrap_or_default();
    let to_sha = move || params.read().get("sha").unwrap_or_default();
    let from_sha = move || query.read().get("vs").unwrap_or_default();

    let resource = LocalResource::new(move || {
        let sid = session_id();
        let from = from_sha();
        let to = to_sha();
        async move {
            // Two fetches: the session detail to discover plan_path, then
            // the /api/diff endpoint with that path.
            let session = fetch_session(sid).await?;
            fetch_diff(from, to, session.plan_path).await
        }
    });

    view! {
        <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
            {move || {
                resource
                    .with(|res| match res {
                        Some(Ok(page)) => diff_view(page.clone()).into_any(),
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

fn diff_view(page: DiffPage) -> impl IntoView {
    let from_short = short_sha(&page.from);
    let to_short = short_sha(&page.to);
    let title = format!("Plan diff {from_short} → {to_short}");
    let path = page.path.clone();
    view! {
        <article class="plan-diff">
            <header class="session-header">
                <h1>{title}</h1>
                <span class="muted">
                    <code>{path}</code>
                </span>
            </header>
            <StructuredDiff files=page.diff_files/>
        </article>
    }
}

fn short_sha(sha: &str) -> String {
    if sha.len() > 8 {
        sha[..8].to_string()
    } else {
        sha.to_string()
    }
}
