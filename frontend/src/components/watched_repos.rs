use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{RepoRow, ReposIndex, delete_repo, fetch_repos};
use crate::store::EventStore;

#[component]
pub fn WatchedRepos() -> impl IntoView {
    let store = expect_context::<EventStore>();
    let resource = LocalResource::new(move || {
        let _ = store.tick.get();
        fetch_repos()
    });
    view! {
        <section class="watched-repos">
            <h2>"Watched repos"</h2>
            <Suspense fallback=move || view! { <p class="muted">"Loading…"</p> }>
                {move || {
                    resource
                        .with(|res| match res {
                            Some(Ok(ReposIndex { repos })) => repos_table(repos.clone()).into_any(),
                            Some(Err(e)) => {
                                view! {
                                    <p class="error">"Failed to load: " {e.to_string()}</p>
                                }
                                    .into_any()
                            }
                            None => view! { <p class="muted">"Loading…"</p> }.into_any(),
                        })
                }}
            </Suspense>
        </section>
    }
}

fn repos_table(repos: Vec<RepoRow>) -> impl IntoView {
    if repos.is_empty() {
        return view! { <p class="muted">"No repos watched yet."</p> }.into_any();
    }
    view! {
        <ul class="watched-repos-list">
            {repos
                .into_iter()
                .map(|r| view! { <RepoCard row=r/> })
                .collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn RepoCard(row: RepoRow) -> impl IntoView {
    let confirming = RwSignal::new(false);
    let pending = RwSignal::new(false);
    let error_msg: RwSignal<Option<String>> = RwSignal::new(None);
    let basename = row.basename.clone();
    let basename_for_action = basename.clone();
    let on_unwatch = move |_| {
        if !confirming.get() {
            confirming.set(true);
            return;
        }
        if pending.get() {
            return;
        }
        pending.set(true);
        let basename = basename_for_action.clone();
        spawn_local(async move {
            match delete_repo(basename).await {
                Ok(()) => {
                    // Daemon emits `repo_unwatched`; EventStore.tick
                    // bumps and the parent's LocalResource refetches.
                    confirming.set(false);
                    pending.set(false);
                }
                Err(e) => {
                    error_msg.set(Some(e.to_string()));
                    pending.set(false);
                    confirming.set(false);
                }
            }
        });
    };
    let on_cancel = move |_| {
        confirming.set(false);
    };
    let unwatch_label = move || {
        if pending.get() {
            "Removing…"
        } else if confirming.get() {
            "Confirm unwatch"
        } else {
            "Unwatch"
        }
    };
    view! {
        <li class="watched-repos-row">
            <div class="watched-repos-meta">
                <span class="watched-repos-basename">
                    <code>{basename}</code>
                </span>
                <span class="watched-repos-root muted">{row.root}</span>
                <span class="watched-repos-count">
                    {row.plan_count} " plan" {if row.plan_count == 1 { "" } else { "s" }}
                </span>
            </div>
            <div class="watched-repos-actions">
                <Show when=move || confirming.get() && !pending.get()>
                    <button
                        class="watched-repos-cancel"
                        type="button"
                        on:click=on_cancel
                    >
                        "Cancel"
                    </button>
                </Show>
                <button
                    class="watched-repos-unwatch"
                    type="button"
                    on:click=on_unwatch
                    prop:disabled=move || pending.get()
                >
                    {unwatch_label}
                </button>
            </div>
            <Show when=move || error_msg.get().is_some()>
                <p class="error watched-repos-error">
                    {move || error_msg.get().unwrap_or_default()}
                </p>
            </Show>
        </li>
    }
}
