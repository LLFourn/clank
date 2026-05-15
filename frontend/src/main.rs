use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

mod api;
mod components;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <Router>
            <Routes fallback=NotFound>
                <Route path=path!("/") view=components::home::Home/>
                <Route
                    path=path!("/sessions/:session_id")
                    view=components::session_detail::SessionDetail
                />
                <Route
                    path=path!("/sessions/:session_id/plan/:sha/diff")
                    view=components::plan_diff::PlanDiff
                />
                <Route
                    path=path!("/sessions/:session_id/plan/:sha")
                    view=components::plan_revision::PlanRevision
                />
                <Route
                    path=path!("/sessions/:session_id/commit/:sha")
                    view=components::commit_diff::CommitDiff
                />
            </Routes>
        </Router>
    }
}

#[component]
fn NotFound() -> impl IntoView {
    view! {
        <h1>"Not found"</h1>
        <p>
            <a href="/">"← back to sessions"</a>
        </p>
    }
}
