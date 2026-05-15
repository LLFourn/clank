use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{DiffPage, fetch_diff};
use crate::components::structured_diff::StructuredDiff;
use crate::store::EventStore;
use crate::util::short_sha;

/// `/plan/:repo/:stem_md/diff/:from/:to` — compare two plan
/// revisions of the same plan. The daemon's
/// `/api/plan/{repo}/{stem_md}/diff/{from}/{to}` knows the plan path
/// internally; no need to round-trip the detail page first.
#[component]
pub fn PlanDiff() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let params = use_params_map();
    let plan_id = move || {
        let p = params.read();
        let repo = p.get("repo").unwrap_or_default();
        let stem = p.get("stem_md").unwrap_or_default();
        format!("{repo}/{stem}")
    };
    let from_sha = move || params.read().get("from").unwrap_or_default();
    let to_sha = move || params.read().get("to").unwrap_or_default();

    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_diff(plan_id(), from_sha(), to_sha())
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
    let path_label = if page.from_path == page.to_path {
        page.from_path.clone()
    } else {
        format!("{} → {}", page.from_path, page.to_path)
    };
    view! {
        <article class="plan-diff">
            <header class="session-header">
                <h1>{title}</h1>
                <span class="muted">
                    <code>{path_label}</code>
                </span>
            </header>
            <StructuredDiff files=page.diff_files/>
        </article>
    }
}
