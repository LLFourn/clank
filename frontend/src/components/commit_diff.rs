use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{CommitDetail, CommitDetailResponse, fetch_commit_diff};
use crate::components::feedback_card::FeedbackCard;
use crate::components::finalize_snapshot::FinalizeSnapshot;
use crate::components::structured_diff::StructuredDiff;
use crate::store::EventStore;
use crate::util::short_sha;

#[component]
pub fn CommitDiff() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let params = use_params_map();
    let plan_id = move || {
        let p = params.read();
        let repo = p.get("repo").unwrap_or_default();
        let stem = p.get("stem_md").unwrap_or_default();
        format!("{repo}/{stem}")
    };
    let sha = move || params.read().get("sha").unwrap_or_default();
    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_commit_diff(plan_id(), sha())
    });

    view! {
        <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
            {move || {
                resource
                    .with(|res| match res {
                        Some(Ok(page)) => commit_view(page.clone()).into_any(),
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

fn commit_view(page: CommitDetailResponse) -> impl IntoView {
    let plan_id = page.plan_id.clone();
    let back_href = format!("/plan/{plan_id}");
    let short = short_sha(&page.commit_sha);
    let is_finalize = matches!(page.detail, CommitDetail::Finalize { .. });
    let title = if is_finalize {
        format!("Finalize {short}")
    } else {
        format!("Commit {short}")
    };
    let target_sha = page.commit_sha.clone();
    // The tagged `CommitDetail` enum is the kind-keyed payload:
    // Finalize → approval snapshot at the freeze tree; reviewable
    // kinds → live feedback. Match exhausts.
    let (finalize_section, feedback_section) = match page.detail {
        CommitDetail::Finalize { snapshot } => {
            (Some(view! { <FinalizeSnapshot approvals=snapshot/> }), None)
        }
        CommitDetail::PlanOnly { feedback }
        | CommitDetail::CodeOnly { feedback }
        | CommitDetail::Mixed { feedback } => {
            if feedback.is_empty() {
                (None, None)
            } else {
                let cards = feedback
                    .into_iter()
                    .map(|fb| {
                        let target = target_sha.clone();
                        view! { <FeedbackCard entry=fb target_sha=target/> }
                    })
                    .collect_view();
                (
                    None,
                    Some(view! {
                        <section class="session-section">
                            <h2>"Feedback on this commit"</h2>
                            <div class="feedback-list">{cards}</div>
                        </section>
                    }),
                )
            }
        }
        CommitDetail::MultiPlan {} => (None, None),
    };
    view! {
        <article class="commit-diff">
            <header class="session-header">
                <a href=back_href class="back-link">"← plan"</a>
                <h1>{title}</h1>
            </header>
            {finalize_section}
            <StructuredDiff files=page.diff_files/>
            {feedback_section}
        </article>
    }
}
