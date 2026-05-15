use leptos::prelude::*;

use crate::api::{SessionRow, fetch_sessions};

#[component]
pub fn Home() -> impl IntoView {
    let sessions = LocalResource::new(fetch_sessions);

    view! {
        <h1>"Trinity sessions"</h1>
        <Suspense fallback=move || view! { <p class="loading">"Loading…"</p> }>
            {move || {
                // Inside `<Suspense>`, the closure only runs once the resource
                // has resolved — `sessions.with` returns `Some(_)`. Treat the
                // `None` branch as unreachable.
                sessions
                    .with(|res| match res {
                        Some(Ok(rows)) => session_table(rows.clone()).into_any(),
                        Some(Err(e)) => {
                            view! { <p class="error">"Failed to load: " {e.to_string()}</p> }
                                .into_any()
                        }
                        None => unreachable!(
                            "Suspense fallback handles the pending state; sessions \
                             should always be Some here"
                        ),
                    })
            }}
        </Suspense>
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
