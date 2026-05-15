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
    view! {
        <aside class="activity-sidebar">
            <h3>"Recent activity"</h3>
            <ol class="activity-list">
                {move || {
                    store
                        .recent
                        .with(|events| {
                            if events.is_empty() {
                                view! {
                                    <li class="muted activity-empty">
                                        "Waiting for live events…"
                                    </li>
                                }
                                    .into_any()
                            } else {
                                events
                                    .iter()
                                    .take(20)
                                    .map(|e| {
                                        let session = e
                                            .session_id
                                            .clone()
                                            .unwrap_or_else(|| "—".to_string());
                                        let kind = e.kind.clone();
                                        let kind_class = format!("activity-kind activity-{kind}");
                                        let href = e
                                            .session_id
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
                                    })
                                    .collect_view()
                                    .into_any()
                            }
                        })
                }}
            </ol>
        </aside>
    }
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
