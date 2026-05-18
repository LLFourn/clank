use leptos::prelude::*;

use crate::api::{PlanConflictRow, PlanRow, PlansIndex, fetch_plans};
use crate::components::watched_repos::WatchedRepos;
use crate::store::EventStore;

const SHOW_DONE_KEY: &str = "trinity.show_done";

#[component]
pub fn Home() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let plans = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_plans()
    });
    let show_done = RwSignal::new(load_show_done());
    Effect::new(move |_| {
        let v = show_done.get();
        persist_show_done(v);
    });

    view! {
        <div class="home-layout">
            <main class="home-main">
                <h1>"Trinity plans"</h1>
                <WatchedRepos/>
                <div class="plans-toolbar">
                    <label class="show-done-toggle">
                        <input
                            type="checkbox"
                            prop:checked=move || show_done.get()
                            on:change=move |ev| {
                                show_done.set(event_target_checked(&ev));
                            }
                        />
                        " show done"
                    </label>
                </div>
                <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
                    {move || {
                        plans
                            .with(|res| match res {
                                Some(Ok(index)) => {
                                    plans_view(index.clone(), show_done.get()).into_any()
                                }
                                Some(Err(e)) => {
                                    view! {
                                        <p class="error">"Failed to load: " {e.to_string()}</p>
                                    }
                                        .into_any()
                                }
                                None => view! { <p class="loading">"Loading…"</p> }.into_any(),
                            })
                    }}
                </Suspense>
            </main>
            <ActivitySidebar store=store/>
        </div>
    }
}

fn plans_view(index: PlansIndex, show_done: bool) -> impl IntoView {
    use crate::api::PlanLifecycle;
    let plans: Vec<PlanRow> = index
        .plans
        .into_iter()
        .filter(|p| show_done || p.lifecycle != PlanLifecycle::Finished)
        .collect();
    let conflicts = index.conflicts;
    view! {
        {plan_table(plans)}
        {conflicts_section(conflicts)}
    }
}

fn event_target_checked(ev: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

fn load_show_done() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|store| store.get_item(SHOW_DONE_KEY).ok().flatten())
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn persist_show_done(v: bool) {
    if let Some(store) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = store.set_item(SHOW_DONE_KEY, if v { "true" } else { "false" });
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
                            use crate::store::{LiveEvent, live_event_kind_str};
                            let (label, href) = match &e {
                                LiveEvent::Plan(p) => (
                                    p.plan_id.clone(),
                                    format!("/plan/{}", p.plan_id),
                                ),
                                LiveEvent::Repo(_) => (
                                    "—".to_string(),
                                    "#".to_string(),
                                ),
                            };
                            let kind = live_event_kind_str(&e);
                            let kind_class = format!("activity-kind activity-{kind}");
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
    use crate::store::{LiveEvent, live_event_kind_str, live_event_ts};
    let plan_id = match e {
        LiveEvent::Plan(p) => p.plan_id.as_str(),
        LiveEvent::Repo(_) => "-",
    };
    format!(
        "{}:{}:{}",
        live_event_ts(e),
        live_event_kind_str(e),
        plan_id
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
                    <th>"Who"</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|s| {
                        let role = s.waiting_on.role;
                        let waiting_class = format!("waiting waiting-{role}");
                        let plan_id = s.plan_id.clone().unwrap_or_default();
                        let state_class = format!("state-chip state-{}", s.lifecycle);
                        let agents = s.waiting_on.agents.clone();
                        view! {
                            <tr>
                                <td>
                                    <a href=format!("/plan/{plan_id}")>
                                        <code>{plan_id.clone()}</code>
                                    </a>
                                </td>
                                <td>
                                    <span class=state_class>{s.lifecycle.to_string()}</span>
                                </td>
                                <td>{s.phase.to_string()}</td>
                                <td>{s.plan_worktree_status.to_string()}</td>
                                <td>
                                    <span class=waiting_class>{role.to_string()}</span>
                                </td>
                                <td>{who_cell(agents)}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

fn who_cell(agents: Vec<trinity_core::AgentLabel>) -> AnyView {
    if agents.is_empty() {
        return view! { <span class="muted">"—"</span> }.into_any();
    }
    view! {
        <div class="waiting-who">
            {agents
                .into_iter()
                .map(|a| view! { <span class="agent-chip">{a.into_inner()}</span> })
                .collect_view()}
        </div>
    }
    .into_any()
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
