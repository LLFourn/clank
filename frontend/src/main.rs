use leptos::prelude::*;

mod api;
mod components;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! { <components::home::Home/> }
}
