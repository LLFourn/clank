use leptos::prelude::*;

use crate::api::{WaitingOn, WaitingRole};

#[component]
pub fn WaitingBanner(waiting_on: WaitingOn) -> impl IntoView {
    let role = waiting_on.role;
    let class = format!("waiting-banner waiting-banner-{role}");
    let role_label = role_label(role).to_string();
    view! {
        <aside class=class>
            <span class="waiting-banner-role">{role_label}</span>
            <p class="waiting-banner-desc">{waiting_on.description}</p>
        </aside>
    }
}

fn role_label(role: WaitingRole) -> &'static str {
    match role {
        WaitingRole::Master => "Waiting on master",
        WaitingRole::Reviewers => "Waiting on reviewers",
        WaitingRole::None => "Idle",
    }
}
