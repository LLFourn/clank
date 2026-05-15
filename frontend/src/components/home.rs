use leptos::prelude::*;

use crate::api::{PlanConflictRow, PlanRow, PlansIndex, fetch_plans};
use crate::store::EventStore;

#[component]
pub fn Home() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let plans = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_plans()
    });

    view! {
        <div class="home-layout">
            <main class="home-main">
                <h1>"Trinity plans"</h1>
                <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
                    {move || {
                        plans
                            .with(|res| match res {
                                Some(Ok(index)) => plans_view(index.clone()).into_any(),
                                Some(Err(e)) => {
                                    view! {
                                        <p class="error">"Failed to load: " {e.to_string()}</p>
                                    }
                                        .into_any()
                                }
                                None => unreachable!(
                                    "Suspense fallback handles the pending state",
                                ),
                            })
                    }}
                </Suspense>
            </main>
            <ActivitySidebar store=store/>
        </div>
    }
}

fn plans_view(index: PlansIndex) -> impl IntoView {
    let plans = index.plans;
    let conflicts = index.conflicts;
    view! {
        {plan_table(plans)}
        {conflicts_section(conflicts)}
    }
}

#[component]
fn ActivitySidebar(store: EventStore) -> impl IntoView {
    view! {
        <aside class="activity-sidebar">
            <h3>"Recent activity"</h3>
            <Show
                when=move || !store.recent.with(|v| v.is_empty())
                fallback=|| {
                    view! { <p class="muted activity-empty">"Waiting for live events…"</p> }
                }
            >
                <ol class="activity-list">
                    <For
                        each=move || {
                            store.recent.get().into_iter().take(20).enumerate().collect::<Vec<_>>()
                        }
                        key=|(_, e)| activity_key(e)
                        children=move |(_, e)| {
                            let label = e
                                .plan_id
                                .clone()
                                .or_else(|| e.slug.clone())
                                .unwrap_or_else(|| "—".to_string());
                            let kind = e.kind.clone();
                            let kind_class = format!("activity-kind activity-{kind}");
                            let href = e
                                .plan_id
                                .as_ref()
                                .map(|p| format!("/plan/{p}"))
                                .unwrap_or_else(|| "#".to_string());
                            view! {
                                <li class="activity-row">
                                    <span class=kind_class>{kind}</span>
                                    <a href=href class="activity-session">
                                        {label}
                                    </a>
                                </li>
                            }
                        }
                    />
                </ol>
            </Show>
        </aside>
    }
}

fn activity_key(e: &crate::store::LiveEvent) -> String {
    // ts + kind + plan_id is unique-enough across the rolling 50-entry
    // window.
    format!(
        "{}:{}:{}",
        e.ts,
        e.kind,
        e.plan_id.as_deref().unwrap_or("-")
    )
}

fn plan_table(rows: Vec<PlanRow>) -> impl IntoView {
    view! {
        <table>
            <thead>
                <tr>
                    <th>"Plan"</th>
                    <th>"State"</th>
                    <th>"Phase"</th>
                    <th>"Worktree"</th>
                    <th>"Waiting on"</th>
                    <th>"Description"</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|s| {
                        let role = s.waiting_on.role;
                        let waiting_class = format!("waiting waiting-{role}");
                        let plan_id_for_href = s.plan_id.clone();
                        let plan_id_for_text = s.plan_id.clone();
                        let state_class = format!("state-chip state-{}", s.state);
                        view! {
                            <tr>
                                <td>
                                    <a href=format!("/plan/{plan_id_for_href}")>
                                        <code>{plan_id_for_text}</code>
                                    </a>
                                </td>
                                <td>
                                    <span class=state_class>{s.state}</span>
                                </td>
                                <td>{s.phase}</td>
                                <td>{s.worktree_status}</td>
                                <td>
                                    <span class=waiting_class>{role}</span>
                                </td>
                                <td>{s.waiting_on.description}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

fn conflicts_section(conflicts: Vec<PlanConflictRow>) -> AnyView {
    if conflicts.is_empty() {
        return ().into_any();
    }
    view! {
        <section class="home-conflicts">
            <h2>"Plan-key conflicts"</h2>
            <p class="muted">
                "The same plan stem maps to multiple files on disk; resolve before any feedback can route to it."
            </p>
            <ul class="conflict-list">
                {conflicts
                    .into_iter()
                    .map(|c| {
                        let paths = c.paths.join(", ");
                        view! {
                            <li>
                                <strong>{c.slug}</strong>
                                ": "
                                <code>{paths}</code>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
    }
    .into_any()
}
