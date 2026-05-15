use leptos::prelude::*;

use crate::api::{CommitDiffPage, fetch_commit_diff};
use crate::components::structured_diff::StructuredDiff;

/// Inline accordion body for one timeline commit row. Lazily fetches
/// the commit's full message body + structured diff via the existing
/// `/api/plan/{plan_id}/commit/{sha}` endpoint. The parent decides
/// when to mount; this component owns the fetch + render.
#[component]
pub fn ExpandedCommit(plan_id: String, sha: String) -> impl IntoView {
    let plan_id_for_fetch = plan_id.clone();
    let sha_for_fetch = sha.clone();
    let resource = LocalResource::new(move || {
        fetch_commit_diff(plan_id_for_fetch.clone(), sha_for_fetch.clone())
    });
    view! {
        <div class="commit-expanded">
            <Suspense fallback=move || {
                view! { <p class="muted">"Loading commit…"</p> }
            }>
                {move || {
                    resource
                        .with(|res| match res {
                            Some(Ok(page)) => commit_panel(page.clone()).into_any(),
                            Some(Err(e)) => {
                                view! {
                                    <p class="error">"Failed to load commit: " {e.to_string()}</p>
                                }
                                    .into_any()
                            }
                            None => view! { <p class="muted">"Loading commit…"</p> }.into_any(),
                        })
                }}
            </Suspense>
        </div>
    }
}

fn commit_panel(page: CommitDiffPage) -> impl IntoView {
    let body = page.message_body.clone();
    let has_body = !body.trim().is_empty();
    view! {
        <Show when=move || has_body>
            <pre class="commit-message-body">{body.clone()}</pre>
        </Show>
        <StructuredDiff files=page.diff_files/>
    }
}
