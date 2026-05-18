use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::api::{CommitFeedback, CommitRowDetail, PlanDetail, fetch_plan};
use crate::components::feedback_card::FeedbackCard;
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
    let plan_id = detail.plan_id.clone().unwrap_or_default();
    let plan_id_for_timeline = plan_id.clone();
    let waiting_on = detail.waiting_on.clone();
    let commits = detail.commits.clone();
    let latest_target_sha = detail.latest_relevant_commit.clone();
    let timeline_events = detail.timeline.clone();
    let pr_hint = detail.pr_hint.clone();
    let state_class = format!("state-chip state-{}", detail.state);
    let state_label = detail.state.to_string();
    let plan_body_html = detail.plan_body_html.clone();
    let plan_body_truncated = detail.plan_body_truncated;
    let revision_link = match &detail.latest_plan_revision {
        Some(rev) => format!("/plan/{}/revision/{}", plan_id, rev.commit_sha),
        None => format!("/plan/{}", plan_id),
    };
    let latest_review = latest_review_for_target(&commits, latest_target_sha.as_deref());

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
                    {commit_feedback_section(commits)}
                </main>
            </div>
        </article>
    }
}

/// Latest feedback entry on the target commit's gate, paired with the
/// target SHA so the FeedbackCard can render its "on <sha>" header.
/// Filter to the target SHA first; sorting across all commits would
/// surface stale reviews against an earlier target when the current
/// one has none.
fn latest_review_for_target(
    commits: &[CommitRowDetail],
    target: Option<&str>,
) -> Option<(CommitFeedback, String)> {
    let target = target?;
    let commit = commits.iter().find(|c| c.sha == target)?;
    commit
        .feedback
        .iter()
        .max_by_key(|fb| fb.created_at)
        .map(|fb| (fb.clone(), commit.sha.clone()))
}

fn latest_review_section(latest: Option<(CommitFeedback, String)>) -> AnyView {
    match latest {
        Some((fb, target_sha)) => view! {
            <div class="latest-review">
                <h3>"Latest review"</h3>
                <FeedbackCard entry=fb target_sha=target_sha/>
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

/// One feedback section per commit, in `commits[]` order. Each commit
/// header carries the kind (plan_only / code_only / mixed) so the
/// reader can tell at a glance which kind of review they're looking
/// at — no plan-vs-impl split. Empty commits (no feedback yet) are
/// rendered with a muted "no reviews yet" line so the section
/// communicates the gate's missing-reviewer state.
fn commit_feedback_section(commits: Vec<CommitRowDetail>) -> AnyView {
    if commits.is_empty() {
        return view! {
            <section class="session-section">
                <h2>"Reviews"</h2>
                <p class="muted">"No reviewable commits yet."</p>
            </section>
        }
        .into_any();
    }
    view! {
        <section class="session-section">
            <h2>"Reviews"</h2>
            <div class="commit-feedback-list">
                {commits
                    .into_iter()
                    .map(|c| {
                        let kind = c.kind;
                        let kind_class = format!("commit-kind-chip commit-kind-{kind}");
                        let sha_short: String = c.sha.chars().take(7).collect();
                        let target_sha = c.sha.clone();
                        let cards: Vec<CommitFeedback> = c.feedback.clone();
                        view! {
                            <div class="commit-feedback-block">
                                <h3 class="commit-feedback-heading">
                                    <code>{sha_short}</code>
                                    <span class=kind_class>{kind.to_string()}</span>
                                </h3>
                                {if cards.is_empty() {
                                    view! {
                                        <p class="muted">"No reviews yet."</p>
                                    }.into_any()
                                } else {
                                    view! {
                                        <div class="feedback-list">
                                            {cards
                                                .into_iter()
                                                .map(|fb| {
                                                    let t = target_sha.clone();
                                                    view! { <FeedbackCard entry=fb target_sha=t/> }
                                                })
                                                .collect_view()}
                                        </div>
                                    }.into_any()
                                }}
                            </div>
                        }
                    })
                    .collect_view()}
            </div>
        </section>
    }
    .into_any()
}
