use leptos::prelude::*;

use crate::api::{PlanDetail, ReviewGate, post_move_to_done};
use crate::store::EventStore;
use crate::util::short_sha;

/// Sidebar block: phase / worktree-status / current path / latest
/// revisions / review-gate chips. The compact "everything-at-a-glance"
/// header for the plan detail page.
#[component]
pub fn MetaStrip(session: PlanDetail) -> impl IntoView {
    let phase = session.phase.clone();
    let phase_class = format!("phase-chip phase-{phase}");
    let status_class = format!("status-chip status-{}", session.plan_worktree_status);

    let latest_plan = session
        .latest_plan_revision
        .as_ref()
        .map(|r| r.commit_sha.clone());
    let latest_impl = session
        .latest_implementation_revision
        .as_ref()
        .map(|r| r.commit_sha.clone());
    let plan_id_for_plan = session.plan_id.clone();
    let plan_id_for_impl = session.plan_id.clone();

    view! {
        <section class="meta-strip">
            <dl class="meta-grid">
                <dt>"Phase"</dt>
                <dd>
                    <span class=phase_class>{phase}</span>
                </dd>
                <dt>"Worktree"</dt>
                <dd>
                    <span class=status_class>{session.plan_worktree_status}</span>
                </dd>
                <dt>"Plan file"</dt>
                <dd>
                    <code>{session.current_path}</code>
                </dd>
                <dt>"Latest plan revision"</dt>
                <dd>
                    {match latest_plan {
                        Some(sha) => {
                            let href = format!(
                                "/plan/{plan_id_for_plan}/revision/{sha}",
                            );
                            let short = short_sha(&sha);
                            view! {
                                <a href=href>
                                    <code>{short}</code>
                                </a>
                            }
                                .into_any()
                        }
                        None => view! { <span class="muted">"—"</span> }.into_any(),
                    }}
                </dd>
                <dt>"Latest impl commit"</dt>
                <dd>
                    {match latest_impl {
                        Some(sha) => {
                            let href = format!(
                                "/plan/{plan_id_for_impl}/commit/{sha}",
                            );
                            let short = short_sha(&sha);
                            view! {
                                <a href=href>
                                    <code>{short}</code>
                                </a>
                            }
                                .into_any()
                        }
                        None => view! { <span class="muted">"—"</span> }.into_any(),
                    }}
                </dd>
            </dl>
            {session.review_gate.map(|gate| view! { <ReviewGateChips gate=gate/> })}
            {move_to_done_button(&session.plan_id, &session.waiting_on.reason)}
        </section>
    }
}

/// Render the "Move plan to done/" action only when the master is in
/// the `ready_to_start_implementation` state (renamed from
/// `ready_to_finish` → `ready_to_move_forward` → current). Posts to
/// `/api/plan/{plan_id}/done`.
fn move_to_done_button(plan_id: &str, reason: &str) -> AnyView {
    if reason != "ready_to_start_implementation" {
        return ().into_any();
    }
    let store = expect_context::<EventStore>();
    let plan_id = plan_id.to_string();
    let status: RwSignal<DoneButtonState> = RwSignal::new(DoneButtonState::Idle);
    let on_click = move |_| {
        let id = plan_id.clone();
        status.set(DoneButtonState::Pending);
        wasm_bindgen_futures::spawn_local(async move {
            match post_move_to_done(id).await {
                Ok(_) => {
                    status.set(DoneButtonState::Done);
                    store.tick.update(|t| *t = t.wrapping_add(1));
                }
                Err(e) => status.set(DoneButtonState::Failed(e.to_string())),
            }
        });
    };
    let label = move || match status.get() {
        DoneButtonState::Idle => "Move plan to done/".to_string(),
        DoneButtonState::Pending => "Moving…".to_string(),
        DoneButtonState::Done => "Moved ✓".to_string(),
        DoneButtonState::Failed(msg) => format!("Failed: {msg}"),
    };
    let disabled = move || {
        matches!(
            status.get(),
            DoneButtonState::Pending | DoneButtonState::Done
        )
    };
    view! {
        <div class="meta-actions">
            <button class="primary-button" type="button" on:click=on_click prop:disabled=disabled>
                {label}
            </button>
        </div>
    }
    .into_any()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DoneButtonState {
    Idle,
    Pending,
    Done,
    Failed(String),
}

#[component]
fn ReviewGateChips(gate: ReviewGate) -> impl IntoView {
    let state_class = format!("gate-state gate-state-{}", gate.state);
    let approvals_chip = chip_view("approvals", &gate.approvals, "approve");
    let request_changes_chip =
        chip_view("request_changes", &gate.request_changes, "request-changes");
    let missing_chip = chip_view("missing", &gate.missing_approvals, "pending");
    // `gate.phase` is the legacy back-derived "plan"/"impl" tag.
    // Phase 2.8 drops it from the chip header — the per-commit
    // chip palette in the timeline already communicates which kind
    // of review the gate is about.
    view! {
        <div class="review-gate">
            <div class="gate-header">
                <span class="gate-phase">"Review gate"</span>
                <span class=state_class>{gate.state}</span>
            </div>
            <div class="gate-chips">{approvals_chip}{request_changes_chip}{missing_chip}</div>
        </div>
    }
}

fn chip_view(label: &str, agents: &[String], kind: &str) -> AnyView {
    if agents.is_empty() {
        return ().into_any();
    }
    let class = format!("gate-chip gate-chip-{kind}");
    let joined = agents.join(", ");
    let label_text = format!("{label}: ");
    view! {
        <span class=class>
            <span class="gate-chip-label">{label_text}</span>
            <span class="gate-chip-value">{joined}</span>
        </span>
    }
    .into_any()
}
