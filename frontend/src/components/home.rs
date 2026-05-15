use leptos::prelude::*;

use crate::api::{SessionRow, fetch_sessions};
use crate::store::EventStore;

#[component]
pub fn Home() -> impl IntoView {
    let store = expect_context::<EventStore>();
    // Resource keys on the global event tick — any SSE message
    // invalidates and re-fetches. Phase 4 is coarse; per-repo /
    // per-session keying lands later.
    let sessions = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_sessions()
    });

    view! {
        <div class="home-layout">
            <main class="home-main">
                <h1>"Trinity sessions"</h1>
                <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
                    {move || {
                        sessions
                            .with(|res| match res {
                                Some(Ok(rows)) => session_table(rows.clone()).into_any(),
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

#[component]
fn ActivitySidebar(store: EventStore) -> impl IntoView {
    // `<For>` with a stable key so only newly-prepended rows animate
    // their accent pulse. Without this, every event re-creates every
    // <li> as a fresh DOM node and the whole sidebar flashes.
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
                            let session = e
                                .slug
                                .clone()
                                .unwrap_or_else(|| "—".to_string());
                            let kind = e.kind.clone();
                            let kind_class = format!("activity-kind activity-{kind}");
                            let href = e
                                .slug
                                .as_ref()
                                .map(|s| format!("/sessions/{s}"))
                                .unwrap_or_else(|| "#".to_string());
                            view! {
                                <li class="activity-row">
                                    <span class=kind_class>{kind}</span>
                                    <a href=href class="activity-session">
                                        {session}
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
    // ts + kind + slug is unique-enough across the rolling 50-entry
    // window; same ts+kind would only collide on duplicate broadcasts,
    // which we'd want to dedupe visually anyway.
    format!("{}:{}:{}", e.ts, e.kind, e.slug.as_deref().unwrap_or("-"))
}

fn session_table(rows: Vec<SessionRow>) -> impl IntoView {
    view! {
        <table>
            <thead>
                <tr>
                    <th>"Session"</th>
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
                        view! {
                            <tr>
                                <td>{s.session_id}</td>
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
