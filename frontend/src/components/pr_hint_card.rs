use leptos::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::api::{PrHint, PrHintOption};

/// Sidebar card that surfaces the `pr_hint` shape — one command per
/// "keep plan in PR" / "exclude plan from PR" option with a copy
/// button. Only renders when the session detail carries pr_hint
/// (Phase::Implementing).
#[component]
pub fn PrHintCard(hint: PrHint) -> impl IntoView {
    let suggested = hint.suggested_message.clone();
    view! {
        <section class="pr-hint">
            <h3>"PR hint"</h3>
            <p class="pr-hint-suggested muted">
                "Suggested commit message: " <code>{suggested}</code>
            </p>
            <ul class="pr-hint-options">
                {hint
                    .options
                    .into_iter()
                    .map(|opt| view! { <PrHintOptionRow option=opt/> })
                    .collect_view()}
            </ul>
        </section>
    }
}

#[component]
fn PrHintOptionRow(option: PrHintOption) -> impl IntoView {
    let label = label_for(&option.name);
    let command_for_button = option.command.clone();
    let command_for_view = option.command.clone();
    let copied = RwSignal::new(false);
    let on_click = move |_| {
        copy_to_clipboard(&command_for_button);
        copied.set(true);
    };
    view! {
        <li class="pr-hint-option">
            <div class="pr-hint-label">{label}</div>
            <pre class="pr-hint-command">
                <code>{command_for_view}</code>
            </pre>
            <button class="copy-button" type="button" on:click=on_click>
                {move || if copied.get() { "Copied ✓" } else { "Copy" }}
            </button>
        </li>
    }
}

fn label_for(name: &str) -> &'static str {
    match name {
        "keep_plan_in_pr" => "Keep plan in PR",
        "exclude_plan_from_pr" => "Exclude plan from PR",
        _ => "Option",
    }
}

/// Best-effort write to `navigator.clipboard`. Returns silently on any
/// browser-side error (clipboard permission denied, http context, etc.).
fn copy_to_clipboard(text: &str) {
    let Some(window) = web_sys::window() else { return };
    let clipboard = window.navigator().clipboard();
    let promise = clipboard.write_text(text);
    // Drive the promise to completion in the background; we don't wait
    // on it (the visible "Copied ✓" state has already been set by the
    // caller via a signal toggle).
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
}
