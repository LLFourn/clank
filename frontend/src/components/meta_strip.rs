use leptos::prelude::*;

use crate::api::{ReviewGate, SessionDetail, post_move_to_done};
use crate::store::EventStore;
use crate::util::short_sha;

/// Sidebar block: phase / worktree-status / plan path / latest revisions
/// / review-gate chips. The compact "everything-at-a-glance" header for
/// the session detail page.
#[component]
pub fn MetaStrip(session: SessionDetail) -> impl IntoView {
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
    let session_id_for_plan = session.session_id.clone();
    let session_id_for_impl = session.session_id.clone();

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
                <dt>"Plan"</dt>
                <dd>
                    <code>{session.plan_path}</code>
                </dd>
                <dt>"Latest plan revision"</dt>
                <dd>
                    {match latest_plan {
                        Some(sha) => {
                            let href = format!(
                                "/sessions/{session_id_for_plan}/plan/{sha}",
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
                                "/sessions/{session_id_for_impl}/commit/{sha}",
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
            {move_to_done_button(
                &session.session_id,
                &session.repo,
                &session.waiting_on.reason,
            )}
        </section>
    }
}

/// Render the "Move plan to done/" action only when the master is in
/// the `ready_to_finish` state. Posts to `/api/sessions/:id/done` and
/// nudges the global event tick so the open page re-fetches its own
/// state (the watcher will also fire a `repo_rebuilt` event shortly,
/// but the local nudge avoids a perceptible delay).
fn move_to_done_button(session_id: &str, repo: &str, reason: &str) -> AnyView {
    if reason != "ready_to_finish" {
        return ().into_any();
    }
    let store = expect_context::<EventStore>();
    let session_id = session_id.to_string();
    let repo = repo.to_string();
    let status: RwSignal<DoneButtonState> = RwSignal::new(DoneButtonState::Idle);
    let on_click = move |_| {
        let sid = session_id.clone();
        let repo = repo.clone();
        status.set(DoneButtonState::Pending);
        wasm_bindgen_futures::spawn_local(async move {
            match post_move_to_done(sid, repo).await {
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
    view! {
        <div class="review-gate">
            <div class="gate-header">
                <span class="gate-phase">"Review gate (" {gate.phase} ")"</span>
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
