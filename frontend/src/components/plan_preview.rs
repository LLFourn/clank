use leptos::prelude::*;

use crate::markdown;

/// Inline preview of the current plan body, capped to a fixed height
/// with a CSS fade. The wasm frontend renders the raw markdown
/// body locally and decides truncation locally (no `body_html` or
/// `plan_body_truncated` field crosses the wire).
#[component]
pub fn PlanPreview(plan_body: String, revision_link: String) -> impl IntoView {
    let truncated = plan_body.chars().count() > 4000;
    let body_html = markdown::render(&plan_body);
    let expanded = RwSignal::new(false);
    let class = move || {
        if expanded.get() {
            "plan-preview plan-preview-expanded"
        } else {
            "plan-preview"
        }
    };
    let toggle_label = move || {
        if expanded.get() {
            "Collapse"
        } else {
            "See full plan"
        }
    };
    let on_toggle = move |_| expanded.update(|v| *v = !*v);
    view! {
        <section class=class>
            <div class="plan-preview-body" inner_html=body_html></div>
            <Show when=move || truncated>
                <div class="plan-preview-fade"></div>
                <div class="plan-preview-actions">
                    <button class="plan-preview-toggle" type="button" on:click=on_toggle>
                        {toggle_label}
                    </button>
                    <a href=revision_link.clone() class="plan-preview-link muted">
                        "Open as page"
                    </a>
                </div>
            </Show>
        </section>
    }
}
