use leptos::prelude::*;

use crate::api::FinalizeApproval;

/// Render the `.trinity/finished/<stem>/` approval snapshot sealed
/// by a finalize commit. This is NOT live feedback — it's the
/// fixed contents of the finished directory at the freeze commit's
/// tree. Used by both the dedicated commit page and the inline
/// timeline expansion.
#[component]
pub fn FinalizeSnapshot(approvals: Vec<FinalizeApproval>) -> impl IntoView {
    let n = approvals.len();
    let cards = approvals
        .into_iter()
        .map(|a| {
            view! { <FinalizeApprovalCard approval=a/> }
        })
        .collect_view();
    let count_label = if n == 1 {
        "1 approving review".to_string()
    } else {
        format!("{n} approving reviews")
    };
    view! {
        <section class="finalize-snapshot">
            <header class="finalize-snapshot-header">
                <h2>"Finalized"</h2>
                <span class="finalize-snapshot-meta">{count_label}</span>
            </header>
            <p class="finalize-snapshot-note muted">
                "Approval snapshot at the freeze commit. Sealed when the plan finished."
            </p>
            <div class="finalize-snapshot-list">{cards}</div>
        </section>
    }
}

#[component]
fn FinalizeApprovalCard(approval: FinalizeApproval) -> impl IntoView {
    let FinalizeApproval {
        author,
        filename,
        body_html,
    } = approval;
    view! {
        <article class="finalize-approval-card">
            <header class="finalize-approval-card-header">
                <span class="finalize-approval-author">{author}</span>
                <span class="finalize-approval-filename muted">{filename}</span>
                <span class="verdict-pill verdict-pill-sm verdict-approve">"APPROVE"</span>
            </header>
            <div class="finalize-approval-body" inner_html=body_html></div>
        </article>
    }
}
