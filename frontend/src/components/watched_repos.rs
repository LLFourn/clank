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
    // Keyed by basename so a `RepoCard` mid-confirm-unwatch survives an
    // unrelated `EventStore.tick` bump (e.g. another repo's
    // `repo_unwatched` event arriving while the user is staring at this
    // row's "Confirm" UI). Without the key, every row would remount on
    // any refetch and lose its local `confirming` signal.
    view! {
        <ul class="watched-repos-list">
            <For
                each=move || repos.clone()
                key=|r| r.basename.clone()
                children=move |r| view! { <RepoCard row=r/> }
            />
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
    // StoredValue is Copy, which makes the click closure Copy. Without
    // it, capturing `String` by-move makes the closure FnOnce, and the
    // surrounding <Show> children require Fn (Show re-invokes the
    // children closure whenever `when` flips).
    let basename_stored = StoredValue::new(basename.clone());

    // First click → enter confirm state. Two-click protection: the
    // initial "Unwatch" button is replaced by *different* DOM elements
    // ("Confirm" + "Cancel") when confirming, so a finger-twitch double
    // click on the original button can't fire on the now-mounted
    // Confirm. The original button is also hidden during pending so a
    // rapid second click after Confirm is a no-op.
    let on_request = move |_| {
        confirming.set(true);
    };
    let on_cancel = move |_| {
        confirming.set(false);
    };
    let on_confirm = move |_| {
        if pending.get() {
            return;
        }
        let basename = basename_stored.get_value();
        pending.set(true);
        spawn_local(async move {
            match delete_repo(basename).await {
                Ok(outcome) => {
                    // In-memory deregistration succeeded. Daemon emits
                    // `repo_unwatched`; EventStore.tick bumps and the
                    // parent's LocalResource refetches. If the registry
                    // file write failed, surface it inline so the
                    // operator knows the repo will resurrect on
                    // daemon restart.
                    if let Some(msg) = outcome.registry_write_error {
                        error_msg.set(Some(msg));
                    }
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
    let activity_label = format_activity(row.last_activity_ts);
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
                <span class="watched-repos-activity muted">{activity_label}</span>
            </div>
            <div class="watched-repos-actions">
                <Show when=move || !confirming.get()>
                    <button
                        class="watched-repos-unwatch"
                        type="button"
                        on:click=on_request
                    >
                        "Unwatch"
                    </button>
                </Show>
                <Show when=move || confirming.get()>
                    <button
                        class="watched-repos-cancel"
                        type="button"
                        on:click=on_cancel
                        prop:disabled=move || pending.get()
                    >
                        "Cancel"
                    </button>
                    <button
                        class="watched-repos-confirm"
                        type="button"
                        on:click=on_confirm
                        prop:disabled=move || pending.get()
                    >
                        {move || if pending.get() { "Removing…" } else { "Confirm unwatch" }}
                    </button>
                </Show>
            </div>
            <Show when=move || error_msg.get().is_some()>
                <p class="error watched-repos-error">
                    {move || error_msg.get().unwrap_or_default()}
                </p>
            </Show>
        </li>
    }
}

/// Human-friendly "last activity" stamp. The ts is unix seconds from
/// the daemon (canonical, UTC). Render a relative age — same as the
/// activity sidebar — to keep the homepage scanable at a glance.
/// `0` means "no commits / no feedback yet" (rare edge case; render
/// "never" rather than the 1970 fallback).
fn format_activity(ts: i64) -> String {
    if ts <= 0 {
        return "never".to_string();
    }
    let now = (js_sys::Date::now() / 1000.0) as i64;
    // Clamp to 0 when the browser clock is BEHIND the daemon clock.
    // Without this clamp, a small skew renders an "in 3m" string that
    // is misleading; with it, the worst case is "0s ago" on a freshly
    // updated row, which reads as a no-op and recovers on the next
    // refetch. Skew the other way (browser AHEAD) inflates the age,
    // which is the standard accepted behavior for relative timestamps.
    let delta = (now - ts).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 30 * 86_400 {
        format!("{}d ago", delta / 86_400)
    } else {
        format!("{}mo ago", delta / (30 * 86_400))
    }
}
