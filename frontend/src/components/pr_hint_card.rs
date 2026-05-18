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
    let label = label_for(option.kind);
    let command_for_view = option.command.clone();
    let command_for_button = option.command.clone();
    // Tri-state so the user knows whether the browser actually accepted
    // the clipboard write (HTTPS contexts, focus, permissions all gate
    // navigator.clipboard).
    let state: RwSignal<CopyState> = RwSignal::new(CopyState::Idle);
    let on_click = move |_| {
        let text = command_for_button.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match copy_to_clipboard(&text).await {
                Ok(()) => state.set(CopyState::Copied),
                Err(()) => state.set(CopyState::Failed),
            }
        });
    };
    let button_label = move || match state.get() {
        CopyState::Idle => "Copy",
        CopyState::Copied => "Copied ✓",
        CopyState::Failed => "Copy failed",
    };
    let button_class = move || match state.get() {
        CopyState::Failed => "copy-button copy-button-failed",
        _ => "copy-button",
    };
    view! {
        <li class="pr-hint-option">
            <div class="pr-hint-label">{label}</div>
            <pre class="pr-hint-command">
                <code>{command_for_view}</code>
            </pre>
            <button class=button_class type="button" on:click=on_click>
                {button_label}
            </button>
        </li>
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyState {
    Idle,
    Copied,
    Failed,
}

fn label_for(kind: trinity_core::PrHintOptionKind) -> &'static str {
    use trinity_core::PrHintOptionKind::*;
    match kind {
        KeepPlanInPr => "Keep plan in PR",
        ExcludePlanFromPr => "Exclude plan from PR",
    }
}

/// Resolve once `navigator.clipboard.writeText` finishes, so the UI's
/// "Copied ✓" / "Copy failed" state reflects the browser's actual
/// answer rather than optimistic success.
async fn copy_to_clipboard(text: &str) -> Result<(), ()> {
    let Some(window) = web_sys::window() else {
        return Err(());
    };
    let clipboard = window.navigator().clipboard();
    let promise = clipboard.write_text(text);
    JsFuture::from(promise).await.map(|_| ()).map_err(|_| ())
}
