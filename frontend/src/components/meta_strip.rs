use leptos::prelude::*;

use crate::api::{PlanDetail, ReviewGate};
use crate::util::short_sha;

/// Sidebar block: phase / worktree-status / current path / latest
/// revisions / review-gate chips. The compact "everything-at-a-glance"
/// header for the plan detail page.
#[component]
pub fn MetaStrip(session: PlanDetail) -> impl IntoView {
    let phase = session.phase;
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
    let plan_id_for_plan = session.plan_id.clone().unwrap_or_default();
    let plan_id_for_impl = session.plan_id.clone().unwrap_or_default();

    view! {
        <section class="meta-strip">
            <dl class="meta-grid">
                <dt>"Phase"</dt>
                <dd>
                    <span class=phase_class>{phase.to_string()}</span>
                </dd>
                <dt>"Worktree"</dt>
                <dd>
                    <span class=status_class>{session.plan_worktree_status.to_string()}</span>
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
        </section>
    }
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
